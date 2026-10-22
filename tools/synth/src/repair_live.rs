//! Concrete, confined execution of the fixed repair checks. The model never
//! receives a process handle or a path to private input.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::anchored_edits::{Planned, apply};
use crate::cli_run::parse_run;
use crate::contracts::{PlanComparison, PlanEvidence, compare_plan};
use crate::probe::{OracleObservation, ProbeObservation, selected_body_rows, validate_synthetic};
use crate::repair::{
    BuildIdentity, CheckResult, ConfinedWorker, FrozenCase, Observation, Outcome, SuiteId,
    SyntheticReduction,
};
use crate::repair_build::{BuildError, IsolatedSource, ProcessResult, SandboxWorker, tree_digest};

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn generated_regression_failed(result: &ProcessResult) -> bool {
    result.exit_code != Some(0)
        && !result.timed_out
        && result
            .stdout
            .contains("test frozen_role_equality_regression ... FAILED")
}

fn case_plan_comparison(case: &FrozenCase, evidence: Option<&PlanEvidence>) -> PlanComparison {
    if case.sql_sha256 == digest("") {
        PlanComparison::Unavailable
    } else {
        compare_plan("Role", &case.expected_literal, evidence)
    }
}

fn observation_id(
    build: &BuildIdentity,
    case: &FrozenCase,
    rows: &Option<Vec<u64>>,
    plan: PlanComparison,
) -> String {
    digest(format!(
        "{}:{}:{}:{}:{rows:?}:{plan:?}",
        build.build_id, case.fixture_sha256, case.prompt_sha256, case.sql_sha256
    ))
}

#[derive(Clone)]
pub struct PrivateCase {
    pub csv: Vec<u8>,
    pub prompt: String,
}

fn index_cases(
    cases: Vec<PrivateCase>,
) -> Result<BTreeMap<(String, String), PrivateCase>, BuildError> {
    let mut keyed = BTreeMap::<(String, String), PrivateCase>::new();
    for case in cases {
        let key = (digest(&case.csv), digest(&case.prompt));
        if let Some(existing) = keyed.get(&key) {
            if existing.csv != case.csv || existing.prompt != case.prompt {
                return Err(BuildError::Export);
            }
        } else {
            keyed.insert(key, case);
        }
    }
    Ok(keyed)
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
    staged_checks: u32,
}

const MAX_STAGED_CHECKS: u32 = 6;

pub struct StagedCheckResult {
    pub tree_sha256: String,
    pub check: CheckResult,
}

fn staged_preview_tree(
    source: &IsolatedSource,
    destination: &Path,
    regression: &Path,
    planned: &Planned,
) -> Result<(PathBuf, String), Outcome> {
    let protected_path = Path::new("crates/baho-core/tests/repair_regression.rs");
    if planned.files.iter().any(|file| file.path == protected_path)
        || planned
            .new_files
            .iter()
            .any(|(path, _)| path == protected_path)
    {
        return Err(Outcome::InvalidInput);
    }
    let tree = source
        .fresh_build(destination, Some(regression))
        .map_err(map_build)?;
    apply(&tree, planned).map_err(|_| Outcome::EnvironmentFailure)?;
    let tree_sha256 = tree_digest(&tree).map_err(map_build)?;
    Ok((tree, tree_sha256))
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
        let cargo_bin = cargo_home.join("bin/cargo");
        let sandbox = SandboxWorker::new(
            &build_root,
            &attempt.join("protected"),
            &cargo_home,
            &rustup_home,
            &cargo_bin,
        )?;
        let keyed = index_cases(cases)?;
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
            staged_checks: 0,
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
    pub fn check_staged(
        &mut self,
        planned: &Planned,
        suite: SuiteId,
    ) -> Result<StagedCheckResult, Outcome> {
        if self.staged_checks >= MAX_STAGED_CHECKS {
            return Err(Outcome::ModelExhausted);
        }
        self.staged_checks += 1;
        let source = self.source.as_ref().ok_or(Outcome::EnvironmentFailure)?;
        let regression = self.attempt.join("protected/repair_regression.rs");
        if !regression.is_file() {
            return Err(Outcome::EnvironmentFailure);
        }
        let destination = self
            .sandbox
            .build_root
            .join(format!("staged-check-{:04}", self.staged_checks));
        let (tree, tree_sha256) = staged_preview_tree(source, &destination, &regression, planned)?;
        let result = self
            .sandbox
            .fixed_cargo(suite, &tree, self.timeout)
            .map_err(map_build)?;
        Ok(StagedCheckResult {
            tree_sha256,
            check: CheckResult {
                suite,
                command: suite.command().iter().map(|x| (*x).into()).collect(),
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                output_excerpt: format!("{}{}", result.stdout, result.stderr)
                    .chars()
                    .take(4096)
                    .collect(),
            },
        })
    }

    pub fn has_issued_build_id(&self, build_id: &str) -> bool {
        self.identities.contains_key(build_id)
    }

    pub fn synthetic_probe(
        &mut self,
        build_id: &str,
        csv: &str,
        prompt: &str,
        sql: Option<&str>,
    ) -> Result<ProbeObservation, Outcome> {
        let build = self
            .identities
            .get(build_id)
            .cloned()
            .ok_or(Outcome::EnvironmentFailure)?;
        let source_rows = validate_synthetic(csv.as_bytes()).map_err(|_| Outcome::NeedsDesign)?;
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
        let (tree, binary) = self.paths(&build)?.clone();
        let input_path = tree.join(format!(
            "repair-synthetic-{}.csv",
            &case.fixture_sha256[..16]
        ));
        fs::write(&input_path, csv).map_err(|_| Outcome::EnvironmentFailure)?;
        let result = self
            .sandbox
            .run_cli(&binary, &input_path, prompt, &tree, self.timeout)
            .map_err(map_build)?;
        if result.timed_out {
            return Err(Outcome::ProcessTimeout);
        }
        let run = fs::read_dir(tree.join(".baho/runs"))
            .map_err(|_| Outcome::EnvironmentFailure)?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .max_by_key(|e| e.file_name())
            .ok_or(Outcome::EnvironmentFailure)?;
        let parsed = parse_run(&run.path()).map_err(|_| Outcome::EnvironmentFailure)?;
        let body_rows = selected_body_rows(&run.path()).map_err(|_| Outcome::EnvironmentFailure)?;
        let oracle = sql.map(|_| OracleObservation {
            ordered_rows_match: parsed.result_rows.as_ref() == Some(&case.expected_source_rows),
            plan_comparison: case_plan_comparison(&case, parsed.plan_evidence.as_ref()),
            expected_rows: case.expected_source_rows.clone(),
        });
        let mut observation = ProbeObservation {
            build_id: build.build_id,
            binary_sha256: build.binary_sha256,
            fixture_sha256: case.fixture_sha256,
            prompt_sha256: case.prompt_sha256,
            result_id: String::new(),
            run_id: parsed.run_id,
            outcome: format!("{:?}", parsed.outcome),
            source_rows,
            body_rows,
            result_rows: parsed.result_rows,
            diagnostic_codes: parsed.diagnostics.iter().map(|d| d.code.clone()).collect(),
            diagnostics: parsed.diagnostics,
            plan: parsed.plan_evidence,
            oracle,
        };
        let mut stable = observation.clone();
        stable.run_id.clear();
        observation.result_id =
            digest(serde_json::to_vec(&stable).map_err(|_| Outcome::EnvironmentFailure)?);
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
        let plan = case_plan_comparison(case, parsed.plan_evidence.as_ref());
        let result_id = observation_id(build, case, &parsed.result_rows, plan);
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
        Ok(generated_regression_failed(&result))
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
    use crate::anchored_edits::Planned;
    use crate::contracts::{PlanLiteral, PlanOperator, TextMatchPolicy};
    use std::process::Command;

    #[test]
    fn same_input_with_distinct_sql_keeps_each_probe_expectation() {
        let csv = b"Id,Role\n1,Alpha\n2,Beta\n";
        let prompt = "show matching roles";
        let cases = index_cases(vec![
            PrivateCase {
                csv: csv.to_vec(),
                prompt: prompt.into(),
            },
            PrivateCase {
                csv: csv.to_vec(),
                prompt: prompt.into(),
            },
        ])
        .unwrap();
        assert_eq!(cases.len(), 1);

        let mut case = FrozenCase {
            fixture_sha256: digest(csv),
            prompt_sha256: digest(prompt),
            sql_sha256: digest("SELECT Alpha"),
            expected_source_rows: vec![1],
            expected_literal: "Alpha".into(),
        };
        let plan = PlanEvidence {
            column: "Role".into(),
            operator: PlanOperator::Equal,
            literal: PlanLiteral::Text("Alpha".into()),
            text_match_policy: TextMatchPolicy::UnicodeLowercase,
        };
        let build = BuildIdentity {
            build_id: "build".into(),
            revision: "revision".into(),
            tree_sha256: "tree".into(),
            toolchain: "toolchain".into(),
            binary_sha256: "binary".into(),
        };
        let alpha_plan = case_plan_comparison(&case, Some(&plan));
        assert_eq!(alpha_plan, PlanComparison::Equivalent);
        let alpha_id = observation_id(&build, &case, &Some(vec![1]), alpha_plan);

        case.sql_sha256 = digest("SELECT Beta");
        case.expected_literal = "Beta".into();
        case.expected_source_rows = vec![2];
        let beta_plan = case_plan_comparison(&case, Some(&plan));
        assert_eq!(beta_plan, PlanComparison::Different);
        assert_ne!(
            alpha_id,
            observation_id(&build, &case, &Some(vec![1]), beta_plan)
        );

        case.sql_sha256 = digest("");
        case.expected_literal.clear();
        assert_eq!(
            case_plan_comparison(&case, Some(&plan)),
            PlanComparison::Unavailable
        );
    }

    #[test]
    fn regression_failure_requires_generated_test_failure() {
        let mut result = ProcessResult {
            exit_code: Some(101),
            timed_out: false,
            stdout: "running 1 test\ntest frozen_role_equality_regression ... FAILED\n".into(),
            stderr: "error: test failed".into(),
            stdout_bytes: 0,
            stderr_bytes: 0,
        };
        assert!(generated_regression_failed(&result));

        result.stdout.clear();
        result.stderr = "error: could not compile baho-core".into();
        assert!(!generated_regression_failed(&result));

        result.stdout = "test frozen_role_equality_regression ... FAILED".into();
        result.exit_code = Some(0);
        assert!(!generated_regression_failed(&result));
    }

    #[test]
    fn staged_preview_contains_draft_and_leaves_checkout_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let tests = checkout.join("crates/baho-core/tests");
        fs::create_dir_all(&tests).unwrap();
        fs::write(
            checkout.join("crates/baho-core/tests/existing.rs"),
            "#[test] fn existing() {}\n",
        )
        .unwrap();
        let source = IsolatedSource {
            revision: "test".into(),
            checkout: checkout.clone(),
            tree_sha256: tree_digest(&checkout).unwrap(),
        };
        let regression = temp.path().join("frozen.rs");
        fs::write(&regression, "#[test] fn frozen() {}\n").unwrap();
        let planned = Planned {
            files: vec![],
            new_files: vec![(
                PathBuf::from("crates/baho-core/tests/staged.rs"),
                "use baho_core::missing_import;\n".into(),
            )],
        };
        let (preview, hash) =
            staged_preview_tree(&source, &temp.path().join("preview"), &regression, &planned)
                .unwrap();
        assert_eq!(hash, tree_digest(&preview).unwrap());
        assert!(preview.join("crates/baho-core/tests/staged.rs").is_file());
        assert!(
            preview
                .join("crates/baho-core/tests/repair_regression.rs")
                .is_file()
        );
        assert!(!checkout.join("crates/baho-core/tests/staged.rs").exists());
        assert!(
            !checkout
                .join("crates/baho-core/tests/repair_regression.rs")
                .exists()
        );
        assert_eq!(source.tree_sha256, tree_digest(&checkout).unwrap());
        assert_ne!(source.tree_sha256, hash);
        let protected_draft = Planned {
            files: vec![],
            new_files: vec![(
                PathBuf::from("crates/baho-core/tests/repair_regression.rs"),
                "#[test] fn replacement() {}\n".into(),
            )],
        };
        assert!(matches!(
            staged_preview_tree(
                &source,
                &temp.path().join("rejected"),
                &regression,
                &protected_draft
            ),
            Err(Outcome::InvalidInput)
        ));
        assert!(!temp.path().join("rejected").exists());
    }

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
            }],
        )
        .unwrap();
        let baseline = worker.baseline(revision).unwrap();
        let before = worker.probe(&baseline, &case).unwrap();
        let checkout = worker.checkout().to_path_buf();
        let regression = protected.join("repair_regression.rs");
        fs::write(
            &regression,
            "#[test] fn frozen_role_equality_regression() { assert!(true); }\n",
        )
        .unwrap();
        let bad_draft = Planned {
            files: vec![],
            new_files: vec![(
                PathBuf::from("crates/baho-core/tests/staged_invalid.rs"),
                "use baho_core::missing_import;\n#[test] fn staged_invalid() {}\n".into(),
            )],
        };
        let preview = worker
            .check_staged(&bad_draft, SuiteId::AdditiveTests)
            .unwrap();
        assert_ne!(preview.check.exit_code, Some(0));
        assert!(preview.check.output_excerpt.contains("missing_import"));
        assert!(
            !checkout
                .join("crates/baho-core/tests/staged_invalid.rs")
                .exists()
        );
        let changed = checkout.join("crates/baho-core/src/intent.rs");
        let mut source = fs::read_to_string(&changed).unwrap();
        source.push_str("\n// Deterministic repair worker smoke patch.\n");
        fs::write(changed, source).unwrap();
        let candidate = worker.candidate("smoke-patch", &regression).unwrap();
        let after = worker.probe(&candidate, &case).unwrap();
        assert_ne!(baseline.tree_sha256, candidate.tree_sha256);
        assert_ne!(baseline.build_id, candidate.build_id);
        assert_eq!(before.source_rows, after.source_rows);
        assert_eq!(before.plan, after.plan);
        assert_eq!(before.fixture_sha256, after.fixture_sha256);
        assert_eq!(before.prompt_sha256, after.prompt_sha256);
        let check = worker
            .check(&candidate, SuiteId::ProtectedCompatibility)
            .unwrap();
        assert_eq!(check.exit_code, Some(0), "{}", check.output_excerpt);
    }
}
