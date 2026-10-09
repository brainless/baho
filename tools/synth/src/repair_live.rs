//! Concrete, confined execution of the fixed repair checks. The model never
//! receives a process handle or a path to private input.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::cli_run::parse_run;
use crate::contracts::compare_plan;
use crate::repair::{
    BuildIdentity, CheckResult, ConfinedWorker, FrozenCase, Observation, Outcome, SuiteId,
    SyntheticReduction,
};
use crate::repair_build::{BuildError, IsolatedSource, SandboxWorker, tree_digest};

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone)]
pub struct PrivateCase {
    pub csv: Vec<u8>,
    pub prompt: String,
    pub literal: String,
}

pub struct LiveWorker {
    repo: PathBuf,
    attempt: PathBuf,
    revision: String,
    timeout: Duration,
    source: Option<IsolatedSource>,
    sandbox: SandboxWorker,
    builds: BTreeMap<String, (PathBuf, PathBuf)>,
    identities: BTreeMap<String, BuildIdentity>,
    cases: BTreeMap<(String, String), PrivateCase>,
}

impl LiveWorker {
    pub fn new(
        repo: &Path,
        attempt: &Path,
        revision: &str,
        timeout: Duration,
        cases: Vec<PrivateCase>,
    ) -> Result<Self, BuildError> {
        let build_root = attempt.join("build");
        fs::create_dir(&build_root)?;
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cargo")
            });
        let rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".rustup")
            });
        let cargo_bin = fs::canonicalize(cargo_home.join("bin/cargo"))?;
        let sandbox = SandboxWorker::new(
            &build_root,
            &attempt.join("protected"),
            &cargo_home,
            &rustup_home,
            &cargo_bin,
        )?;
        let mut keyed = BTreeMap::new();
        for case in cases {
            let key = (digest(&case.csv), digest(&case.prompt));
            if keyed.insert(key, case).is_some() {
                return Err(BuildError::Export);
            }
        }
        Ok(Self {
            repo: repo.into(),
            attempt: attempt.into(),
            revision: revision.into(),
            timeout,
            source: None,
            sandbox,
            builds: BTreeMap::new(),
            identities: BTreeMap::new(),
            cases: keyed,
        })
    }

    fn build(&mut self, name: &str, regression: Option<&Path>) -> Result<BuildIdentity, Outcome> {
        let source = self.source.as_ref().ok_or(Outcome::EnvironmentFailure)?;
        let tree = source
            .fresh_build(&self.sandbox.build_root.join(name), regression)
            .map_err(map_build)?;
        let result = self
            .sandbox
            .build_cli(&tree, self.timeout)
            .map_err(map_build)?;
        if result.timed_out {
            return Err(Outcome::ProcessTimeout);
        }
        if result.exit_code != Some(0) {
            return Err(Outcome::BuildFailure);
        }
        let built_binary = self.sandbox.build_root.join("target/debug/baho");
        if !built_binary.is_file() {
            return Err(Outcome::BuildFailure);
        }
        let binary = self.sandbox.build_root.join(format!("{name}-baho"));
        fs::copy(&built_binary, &binary).map_err(|_| Outcome::EnvironmentFailure)?;
        let binary_sha256 = digest(fs::read(&binary).map_err(|_| Outcome::EnvironmentFailure)?);
        let tree_sha256 = tree_digest(&tree).map_err(map_build)?;
        let toolchain = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "default".into());
        let build_id = digest(format!(
            "{}:{}:{}:{}",
            self.revision, tree_sha256, toolchain, binary_sha256
        ));
        self.builds.insert(build_id.clone(), (tree, binary));
        let identity = BuildIdentity {
            build_id,
            revision: self.revision.clone(),
            tree_sha256,
            toolchain,
            binary_sha256,
        };
        self.identities
            .insert(identity.build_id.clone(), identity.clone());
        Ok(identity)
    }

    fn paths(&self, build: &BuildIdentity) -> Result<&(PathBuf, PathBuf), Outcome> {
        self.builds
            .get(&build.build_id)
            .ok_or(Outcome::EnvironmentFailure)
    }
}

/// Cloneable access for stage tools; all worker operations remain serialized.
pub struct SharedLiveWorker {
    pub inner: Arc<Mutex<LiveWorker>>,
    checkout: PathBuf,
}

impl SharedLiveWorker {
    pub fn new(worker: LiveWorker, checkout: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(worker)),
            checkout,
        }
    }
}

impl ConfinedWorker for SharedLiveWorker {
    fn baseline(&mut self, revision: &str) -> Result<BuildIdentity, Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .baseline(revision)
    }
    fn checkout(&self) -> &Path {
        &self.checkout
    }
    fn probe(&mut self, build: &BuildIdentity, case: &FrozenCase) -> Result<Observation, Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .probe(build, case)
    }
    fn baseline_checks(&mut self, build: &BuildIdentity) -> Result<(), Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .baseline_checks(build)
    }
    fn register_reduction(
        &mut self,
        case: &FrozenCase,
        reduction: &SyntheticReduction,
    ) -> Result<(), Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .register_reduction(case, reduction)
    }
    fn regression_baseline(
        &mut self,
        build: &BuildIdentity,
        regression: &Path,
    ) -> Result<bool, Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .regression_baseline(build, regression)
    }
    fn candidate(
        &mut self,
        patch_sha256: &str,
        regression: &Path,
    ) -> Result<BuildIdentity, Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .candidate(patch_sha256, regression)
    }
    fn check(&mut self, build: &BuildIdentity, suite: SuiteId) -> Result<CheckResult, Outcome> {
        self.inner
            .lock()
            .map_err(|_| Outcome::EnvironmentFailure)?
            .check(build, suite)
    }
}

impl LiveWorker {
    pub fn synthetic_probe(
        &mut self,
        build_id: &str,
        csv: &str,
        prompt: &str,
        sql: Option<&str>,
    ) -> Result<Observation, Outcome> {
        let build = self
            .identities
            .get(build_id)
            .cloned()
            .ok_or(Outcome::EnvironmentFailure)?;
        crate::probe::validate_synthetic(csv.as_bytes()).map_err(|_| Outcome::NeedsDesign)?;
        if prompt.is_empty() || prompt.len() > 2048 {
            return Err(Outcome::NeedsDesign);
        }
        let case = if let Some(sql) = sql {
            crate::repair::freeze_synthetic(&SyntheticReduction {
                csv: csv.into(),
                prompt: prompt.into(),
                sql: sql.into(),
            })?
        } else {
            FrozenCase {
                fixture_sha256: digest(csv),
                prompt_sha256: digest(prompt),
                sql_sha256: digest(""),
                expected_source_rows: Vec::new(),
                expected_literal: String::new(),
            }
        };
        let key = (case.fixture_sha256.clone(), case.prompt_sha256.clone());
        self.cases.entry(key).or_insert_with(|| PrivateCase {
            csv: csv.as_bytes().to_vec(),
            prompt: prompt.into(),
            literal: case.expected_literal.clone(),
        });
        let mut observation = self.probe(&build, &case)?;
        if sql.is_none() {
            observation.plan = crate::contracts::PlanComparison::Unavailable;
        }
        Ok(observation)
    }
    pub fn fixed_check(&mut self, build_id: &str, suite: SuiteId) -> Result<CheckResult, Outcome> {
        let build = self
            .identities
            .get(build_id)
            .cloned()
            .ok_or(Outcome::EnvironmentFailure)?;
        self.check(&build, suite)
    }
}

fn map_build(error: BuildError) -> Outcome {
    match error {
        BuildError::WorkerUnavailable(_) => Outcome::EnvironmentFailure,
        BuildError::Process(_) => Outcome::BuildFailure,
        _ => Outcome::EnvironmentFailure,
    }
}

impl ConfinedWorker for LiveWorker {
    fn baseline(&mut self, revision: &str) -> Result<BuildIdentity, Outcome> {
        if revision != self.revision {
            return Err(Outcome::InvalidInput);
        }
        self.source =
            Some(IsolatedSource::export(&self.repo, revision, &self.attempt).map_err(map_build)?);
        self.build("baseline", None)
    }

    fn checkout(&self) -> &Path {
        &self
            .source
            .as_ref()
            .expect("baseline precedes coding")
            .checkout
    }

    fn probe(&mut self, build: &BuildIdentity, case: &FrozenCase) -> Result<Observation, Outcome> {
        let (tree, binary) = self.paths(build)?.clone();
        let input = self
            .cases
            .get(&(case.fixture_sha256.clone(), case.prompt_sha256.clone()))
            .ok_or(Outcome::InvalidInput)?;
        let input_path = tree.join(format!("repair-input-{}.csv", &case.fixture_sha256[..16]));
        fs::write(&input_path, &input.csv).map_err(|_| Outcome::EnvironmentFailure)?;
        let result = self
            .sandbox
            .run_cli(&binary, &input_path, &input.prompt, &tree, self.timeout)
            .map_err(map_build)?;
        if result.timed_out {
            return Err(Outcome::ProcessTimeout);
        }
        let runs = tree.join(".baho/runs");
        let run = fs::read_dir(runs)
            .map_err(|_| Outcome::EnvironmentFailure)?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .max_by_key(|e| e.file_name())
            .ok_or(Outcome::EnvironmentFailure)?;
        let parsed = parse_run(&run.path()).map_err(|_| Outcome::EnvironmentFailure)?;
        let plan = compare_plan("Role", &input.literal, parsed.plan_evidence.as_ref());
        let result_id = digest(format!(
            "{}:{}:{}:{:?}:{:?}",
            build.build_id, case.fixture_sha256, case.prompt_sha256, parsed.result_rows, plan
        ));
        Ok(Observation {
            build_id: build.build_id.clone(),
            result_id,
            fixture_sha256: case.fixture_sha256.clone(),
            prompt_sha256: case.prompt_sha256.clone(),
            source_rows: parsed.result_rows,
            plan,
        })
    }

    fn baseline_checks(&mut self, build: &BuildIdentity) -> Result<(), Outcome> {
        let tree = self.paths(build)?.0.clone();
        let result = self
            .sandbox
            .fixed_cargo(SuiteId::OwningCrate, &tree, self.timeout)
            .map_err(map_build)?;
        if result.timed_out {
            Err(Outcome::ProcessTimeout)
        } else if result.exit_code == Some(0) {
            Ok(())
        } else {
            Err(Outcome::BaselineFailure)
        }
    }

    fn register_reduction(
        &mut self,
        case: &FrozenCase,
        reduction: &SyntheticReduction,
    ) -> Result<(), Outcome> {
        let key = (case.fixture_sha256.clone(), case.prompt_sha256.clone());
        if self.cases.contains_key(&key) {
            return Err(Outcome::NeedsDesign);
        }
        self.cases.insert(
            key,
            PrivateCase {
                csv: reduction.csv.as_bytes().to_vec(),
                prompt: reduction.prompt.clone(),
                literal: case.expected_literal.clone(),
            },
        );
        Ok(())
    }

    fn regression_baseline(
        &mut self,
        _: &BuildIdentity,
        regression: &Path,
    ) -> Result<bool, Outcome> {
        let source = self.source.as_ref().ok_or(Outcome::EnvironmentFailure)?;
        let tree = source
            .fresh_build(
                &self.sandbox.build_root.join("regression-baseline"),
                Some(regression),
            )
            .map_err(map_build)?;
        let result = self
            .sandbox
            .fixed_cargo(SuiteId::ProtectedCompatibility, &tree, self.timeout)
            .map_err(map_build)?;
        if result.timed_out {
            return Err(Outcome::ProcessTimeout);
        }
        // An assertion failure in the generated test is required; a compile
        // failure or an unrelated failing test does not establish a regression.
        Ok(result.exit_code != Some(0)
            && result
                .stderr
                .contains("test frozen_role_equality_regression ... FAILED"))
    }

    fn candidate(&mut self, _: &str, regression: &Path) -> Result<BuildIdentity, Outcome> {
        self.build("candidate", Some(regression))
    }

    fn check(&mut self, build: &BuildIdentity, suite: SuiteId) -> Result<CheckResult, Outcome> {
        let tree = self.paths(build)?.0.clone();
        let result = self
            .sandbox
            .fixed_cargo(suite, &tree, self.timeout)
            .map_err(map_build)?;
        Ok(CheckResult {
            suite,
            command: suite.command().iter().map(|x| (*x).into()).collect(),
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            output_excerpt: format!("{}{}", result.stdout, result.stderr)
                .chars()
                .take(4096)
                .collect(),
        })
    }
}

#[cfg(test)]
mod smoke_tests {
    use super::*;
    use std::process::Command;

    /// Run explicitly before a live agent trial. This uses real pinned source,
    /// confined Cargo builds, and public CLI artifacts without a provider.
    #[test]
    #[ignore = "requires macOS sandbox-exec and an offline Cargo cache"]
    fn real_baseline_and_candidate_artifacts() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let revision = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(revision.status.success());
        let revision = String::from_utf8(revision.stdout).unwrap();
        let revision = revision.trim();
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("protected");
        fs::create_dir(&protected).unwrap();
        let csv = "Id,Role\n1,Alpha\n2,Beta\n";
        let prompt = "show rows where Role = Alpha";
        let reduction = SyntheticReduction {
            csv: csv.into(),
            prompt: prompt.into(),
            sql: "SELECT _row FROM data WHERE lower(\"Role\") = lower('Alpha') ORDER BY _row"
                .into(),
        };
        let case = crate::repair::freeze_synthetic(&reduction).unwrap();
        let mut worker = LiveWorker::new(
            &repo,
            temp.path(),
            revision,
            Duration::from_secs(180),
            vec![PrivateCase {
                csv: csv.as_bytes().to_vec(),
                prompt: prompt.into(),
                literal: case.expected_literal.clone(),
            }],
        )
        .unwrap();
        let baseline = worker.baseline(revision).unwrap();
        let before = worker.probe(&baseline, &case).unwrap();
        let checkout = worker.checkout().to_path_buf();
        let changed = checkout.join("crates/baho-core/src/intent.rs");
        let mut source = fs::read_to_string(&changed).unwrap();
        source.push_str("\n// Deterministic repair worker smoke patch.\n");
        fs::write(changed, source).unwrap();
        let regression = protected.join("repair_regression.rs");
        fs::write(
            &regression,
            "#[test] fn frozen_role_equality_regression() { assert!(true); }\n",
        )
        .unwrap();
        let candidate = worker.candidate("smoke-patch", &regression).unwrap();
        let after = worker.probe(&candidate, &case).unwrap();
        assert_ne!(baseline.tree_sha256, candidate.tree_sha256);
        assert_ne!(baseline.build_id, candidate.build_id);
        assert_eq!(before.source_rows, after.source_rows);
        assert_eq!(before.plan, after.plan);
        assert_eq!(before.fixture_sha256, after.fixture_sha256);
        assert_eq!(before.prompt_sha256, after.prompt_sha256);
        assert_eq!(
            worker
                .check(&candidate, SuiteId::ProtectedCompatibility)
                .unwrap()
                .exit_code,
            Some(0)
        );
    }
}
