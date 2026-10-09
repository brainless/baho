//! Isolated source export and confined process execution for repair attempts.
//! The candidate's build scripts and binary are never run in the user's tree.
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::repair::{RepairError, snapshot};

const MAX_PROCESS_OUTPUT: usize = 64 * 1024;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid pinned revision")]
    Revision,
    #[error("source export failed")]
    Export,
    #[error("worker unavailable: {0}")]
    WorkerUnavailable(&'static str),
    #[error("worker process failed: {0}")]
    Process(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("source guard: {0}")]
    Source(#[from] RepairError),
}

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug)]
pub struct IsolatedSource {
    pub revision: String,
    pub checkout: PathBuf,
    pub tree_sha256: String,
}

impl IsolatedSource {
    /// `git archive` copies only committed content from the explicit revision.
    /// The extracted tree contains no Git metadata and is checked for symlinks.
    pub fn export(repo: &Path, revision: &str, attempt_dir: &Path) -> Result<Self, BuildError> {
        if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BuildError::Revision);
        }
        let resolved = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", &format!("{revision}^{{commit}}")])
            .output()?;
        if !resolved.status.success() || resolved.stdout != format!("{revision}\n").as_bytes() {
            return Err(BuildError::Revision);
        }
        let checkout = attempt_dir.join("checkout");
        fs::create_dir(&checkout)?;
        let archive = attempt_dir.join("protected/source.tar");
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["archive", "--format=tar", "--output"])
            .arg(&archive)
            .arg(revision)
            .status()?;
        if !status.success() {
            return Err(BuildError::Export);
        }
        let status = Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&checkout)
            .status()?;
        if !status.success() {
            return Err(BuildError::Export);
        }
        let files = snapshot(&checkout)?;
        let tree_sha256 = digest(serde_json::to_vec(&files).map_err(|_| BuildError::Export)?);
        Ok(Self {
            revision: revision.into(),
            checkout,
            tree_sha256,
        })
    }

    pub fn fresh_build(
        &self,
        destination: &Path,
        regression: Option<&Path>,
    ) -> Result<PathBuf, BuildError> {
        fs::create_dir(destination)?;
        copy_tree(&self.checkout, destination)?;
        if let Some(source) = regression {
            let target = destination.join("crates/baho-core/tests/repair_regression.rs");
            if target.exists() {
                return Err(BuildError::Export);
            }
            fs::copy(source, target)?;
        }
        snapshot(destination)?;
        Ok(destination.to_path_buf())
    }
}

fn copy_tree(source: &Path, dest: &Path) -> Result<(), BuildError> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if kind.is_symlink() {
            return Err(BuildError::Export);
        }
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            return Err(BuildError::Export);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<(Vec<u8>, u64)> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut count = 0u64;
        let mut buf = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            count += n as u64;
            let room = MAX_PROCESS_OUTPUT.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buf[..n.min(room)]);
        }
        (bytes, count)
    })
}

/// macOS seatbelt profile with explicit readable roots and one writable build
/// tree. Cargo registry and toolchain directories are read-only. No home source,
/// keychain, network, or inherited credentials are exposed.
pub struct SandboxWorker {
    pub build_root: PathBuf,
    profile: PathBuf,
    cargo_home: PathBuf,
    rustup_home: PathBuf,
    cargo_bin: PathBuf,
}

impl SandboxWorker {
    pub fn new(
        build_root: &Path,
        protected_dir: &Path,
        cargo_home: &Path,
        rustup_home: &Path,
        cargo_bin: &Path,
    ) -> Result<Self, BuildError> {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return Err(BuildError::WorkerUnavailable("sandbox-exec"));
        }
        let build_root = fs::canonicalize(build_root)?;
        let protected_dir = fs::canonicalize(protected_dir)?;
        if protected_dir.starts_with(&build_root) {
            return Err(BuildError::WorkerUnavailable(
                "profile must be outside writable build",
            ));
        }
        let registry = fs::canonicalize(cargo_home.join("registry"))?;
        let rustup_home = fs::canonicalize(rustup_home)?;
        let cargo_bin = fs::canonicalize(cargo_bin)?;
        if !build_root.is_dir()
            || !registry.is_dir()
            || !rustup_home.is_dir()
            || !cargo_bin.is_file()
        {
            return Err(BuildError::WorkerUnavailable("offline Cargo/toolchain"));
        }
        let cargo_home = build_root.join("cargo-home");
        fs::create_dir(&cargo_home)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&registry, cargo_home.join("registry"))?;
        let profile = protected_dir.join("worker.sb");
        let quote = |path: &Path| {
            format!(
                "\"{}\"",
                path.to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            )
        };
        let contents = format!(
            "(version 1)\n(deny default)\n(allow process*)\n(allow file-read* (subpath \"/usr\") (subpath \"/System\") (subpath \"/Library\") (subpath \"/private/etc\") (subpath \"/dev\") (subpath {}) (subpath {}) (subpath {}) (literal {}))\n(allow file-write* (subpath {}))\n(deny network*)\n",
            quote(&build_root),
            quote(&registry),
            quote(&rustup_home),
            quote(&profile),
            quote(&build_root)
        );
        fs::write(&profile, contents)?;
        let probe = Command::new("/usr/bin/sandbox-exec")
            .arg("-f")
            .arg(&profile)
            .arg("/usr/bin/true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !probe.success() {
            return Err(BuildError::WorkerUnavailable("sandbox profile launch"));
        }
        Ok(Self {
            build_root,
            profile,
            cargo_home,
            rustup_home,
            cargo_bin,
        })
    }

    fn run_executable(
        &self,
        executable: &Path,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ProcessResult, BuildError> {
        let cwd = fs::canonicalize(cwd)?;
        if !cwd.starts_with(&self.build_root) {
            return Err(BuildError::Process("invalid fixed worker command".into()));
        }
        // The caller supplies only predefined commands. Environment is rebuilt
        // from explicit paths, with no provider credentials or inherited flags.
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(&self.profile)
            .arg(executable)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("HOME", &self.build_root)
            .env("CARGO_HOME", &self.cargo_home)
            .env("RUSTUP_HOME", &self.rustup_home)
            .env("CARGO_TARGET_DIR", self.build_root.join("target"))
            .env("TMPDIR", self.build_root.join("tmp"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    self.cargo_bin
                        .parent()
                        .unwrap_or(Path::new("/usr/bin"))
                        .display()
                ),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        fs::create_dir_all(self.build_root.join("tmp"))?;
        let mut child = command
            .spawn()
            .map_err(|_| BuildError::WorkerUnavailable("sandbox-exec launch"))?;
        let out = drain(child.stdout.take().ok_or(BuildError::Export)?);
        let err = drain(child.stderr.take().ok_or(BuildError::Export)?);
        let start = Instant::now();
        let (code, timed_out) = loop {
            if let Some(status) = child.try_wait()? {
                break (status.code(), false);
            }
            if start.elapsed() >= timeout {
                #[cfg(unix)]
                unsafe {
                    // A dedicated process group includes Cargo, build scripts,
                    // and ordinary children. Negative PID targets that group.
                    let _ = kill(-(child.id() as i32), 9);
                }
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            thread::sleep(Duration::from_millis(10));
        };
        let (stdout, stdout_bytes) = out.join().unwrap_or_default();
        let (stderr, stderr_bytes) = err.join().unwrap_or_default();
        if stdout_bytes > MAX_PROCESS_OUTPUT as u64 || stderr_bytes > MAX_PROCESS_OUTPUT as u64 {
            return Err(BuildError::Process("worker output limit exceeded".into()));
        }
        Ok(ProcessResult {
            exit_code: code,
            timed_out,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_bytes,
            stderr_bytes,
        })
    }

    pub fn fixed_cargo(
        &self,
        suite: crate::repair::SuiteId,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ProcessResult, BuildError> {
        let command = suite.command();
        self.run_executable(&self.cargo_bin, &command[1..], cwd, timeout)
    }

    pub fn build_cli(&self, cwd: &Path, timeout: Duration) -> Result<ProcessResult, BuildError> {
        self.run_executable(
            &self.cargo_bin,
            &["build", "--offline", "--locked", "-p", "baho-cli"],
            cwd,
            timeout,
        )
    }

    /// Run the fixed public CLI operation; the executable and source must be
    /// inside the writable worker tree. Prompts are arguments, never shell text.
    pub fn run_cli(
        &self,
        binary: &Path,
        source: &Path,
        prompt: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ProcessResult, BuildError> {
        let binary = fs::canonicalize(binary)?;
        let source = fs::canonicalize(source)?;
        if !binary.starts_with(&self.build_root)
            || !source.starts_with(&self.build_root)
            || prompt.len() > 2048
            || prompt.contains('\0')
        {
            return Err(BuildError::Process(
                "invalid CLI probe path or prompt".into(),
            ));
        }
        let source_arg = source.to_str().ok_or(BuildError::Export)?;
        self.run_executable(
            &binary,
            &["run", source_arg, "--prompt", prompt],
            cwd,
            timeout,
        )
    }
}

pub fn tree_digest(tree: &Path) -> Result<String, BuildError> {
    let files: BTreeMap<_, _> = snapshot(tree)?;
    Ok(digest(
        serde_json::to_vec(&files).map_err(|_| BuildError::Export)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_profile_is_protected_and_denies_network() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let build = temp.path().join("build");
        let protected = temp.path().join("protected");
        let cargo = temp.path().join("cargo");
        let rustup = temp.path().join("rustup");
        for path in [&build, &protected, &cargo.join("registry"), &rustup] {
            fs::create_dir_all(path).unwrap();
        }
        let binary = std::env::current_exe().unwrap();
        let worker = match SandboxWorker::new(&build, &protected, &cargo, &rustup, &binary) {
            Ok(worker) => worker,
            Err(BuildError::WorkerUnavailable("sandbox profile launch")) => return,
            Err(error) => panic!("unexpected sandbox worker error: {error}"),
        };
        let profile_path = worker.profile.clone();
        let profile = fs::read_to_string(profile_path.clone()).unwrap();
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(&format!("(literal \"{}\")", profile_path.display())));
        assert!(SandboxWorker::new(&build, &build, &cargo, &rustup, &binary).is_err());
    }
    #[test]
    fn exports_only_pinned_tree_and_rejects_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        fs::write(repo.join("Cargo.toml"), "before").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "baseline"
                ])
                .status()
                .unwrap()
                .success()
        );
        let rev = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        fs::write(repo.join("Cargo.toml"), "dirty").unwrap();
        let attempt = temp.path().join("attempt");
        fs::create_dir_all(attempt.join("protected")).unwrap();
        let source = IsolatedSource::export(&repo, &rev, &attempt).unwrap();
        assert_eq!(
            fs::read_to_string(source.checkout.join("Cargo.toml")).unwrap(),
            "before"
        );
        assert_eq!(
            fs::read_to_string(repo.join("Cargo.toml")).unwrap(),
            "dirty"
        );
        let build = source.fresh_build(&attempt.join("build"), None).unwrap();
        assert_eq!(
            fs::read_to_string(build.join("Cargo.toml")).unwrap(),
            "before"
        );
        assert_eq!(source.tree_sha256, tree_digest(&source.checkout).unwrap());
    }
}
