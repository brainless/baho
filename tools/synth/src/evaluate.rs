//! The `synth evaluate` pipeline (Epic 016, T4): wires intake, CLI execution, comparison,
//! review import, and redaction into a sanitized `evaluation.json` plus a private sidecar.
//!
//! Layout of one evaluation directory (all private except `evaluation.json`, and all under
//! the gitignored output root):
//!
//! - `evaluation.json`: the sanitized report. Its SHA-256 is the evaluation hash.
//! - `redaction-sidecar.json`: the neutral mapping, owner-only, bound to that hash.
//! - `input.csv` and `.baho/`: the Role-only derived CSV and raw CLI run artifacts.
//!
//! Only `evaluation.json` may be shown to a model. The sidecar, the derived CSV, and the
//! raw runs hold source literals and stay with the trusted harness.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cli_run::{
    CliBinary, CliRunError, DerivedCsv, ParsedRun, RepoState, RunLimits, create_evaluation_dir,
    derive_role_csv, parse_run, run_case, spawn_failure_code,
};
use crate::contracts::{
    Aggregates, CaseEvidence, CaseReport, ContractError, EvaluationReport, LexicalFacts,
    ObservedResult, PlanComparison, REPORT_SCHEMA_VERSION, ReviewFile, ToolProvenance,
};
use crate::dataset::{DatasetError, LoadedDataset};
use crate::evaluation::{build_case_report, classify, import_reviews};
use crate::intake::{
    IntakeCase, IntakeError, OracleCheck, load_generation, recheck_oracle, select_latest_generation,
};
use crate::redaction::{RedactionError, Redactor, ViolationKind, write_sidecar};

pub const REPORT_FILE: &str = "evaluation.json";
pub const SIDECAR_FILE: &str = "redaction-sidecar.json";
const INPUT_FILE: &str = "input.csv";
const MAX_REVIEW_BYTES: u64 = 1024 * 1024;
const MAX_MESSAGES: usize = 5;
const MAX_MESSAGE_CHARS: usize = 200;

#[derive(Debug, Error)]
pub enum EvaluateError {
    #[error("dataset: {0}")]
    Dataset(#[from] DatasetError),
    #[error("generation: {0}")]
    Intake(#[from] IntakeError),
    #[error("cli run: {0}")]
    Cli(#[from] CliRunError),
    #[error("contract: {0}")]
    Contract(#[from] ContractError),
    #[error("redaction: {0}")]
    Redaction(#[from] RedactionError),
    #[error("{what}: {reason}")]
    Io { what: &'static str, reason: String },
    #[error("dataset changed while evaluating")]
    DatasetChanged,
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> EvaluateError {
    move |e| EvaluateError::Io {
        what,
        reason: e.kind().to_string(),
    }
}

#[derive(Clone, Debug)]
pub struct EvaluateOptions {
    pub dataset: PathBuf,
    /// Without it, the numerically greatest generation under `output` is used.
    pub generation: Option<PathBuf>,
    pub baho_bin: PathBuf,
    pub review: Option<PathBuf>,
    /// Gitignored root holding generations and receiving the evaluation directory.
    pub output: PathBuf,
    /// Repository whose revision and dirty flag are recorded.
    pub repo: PathBuf,
    pub limits: RunLimits,
    pub oracle_timeout: Duration,
}

#[derive(Debug)]
pub struct EvaluateOutcome {
    pub dir: PathBuf,
    pub report_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub evaluation_hash: String,
    pub report: EvaluationReport,
}

impl EvaluateOutcome {
    /// True when at least one case was replayed and none of the replays could start the
    /// process; the report is then not evidence about baho at all.
    pub fn every_replay_failed_to_spawn(&self) -> bool {
        let spawn = |c: &&CaseReport| c.evidence.spawn_failure.is_some();
        let replayed = |c: &&CaseReport| c.evidence.oracle_code.is_none();
        let r = self.report.cases.iter().filter(replayed);
        r.clone().next().is_some() && r.clone().all(|c| spawn(&c))
    }
}

pub fn evaluate(options: &EvaluateOptions) -> Result<EvaluateOutcome, EvaluateError> {
    let dataset = LoadedDataset::load(&options.dataset)?;
    let source = fs::read(&options.dataset).map_err(io("dataset"))?;
    if format!("{:x}", Sha256::digest(&source)) != dataset.hash {
        return Err(EvaluateError::DatasetChanged);
    }
    let generation_dir = match &options.generation {
        Some(dir) => dir.clone(),
        None => select_latest_generation(&options.output)?,
    };
    let generation = load_generation(&generation_dir, &dataset)?;

    // Everything that can reject the inputs runs before any directory is created.
    let review_file = options.review.as_deref().map(read_review).transpose()?;
    let reviews = import_reviews(review_file.as_ref(), &generation)?;
    let binary = CliBinary::probe(&options.baho_bin)?;
    let repo = RepoState::capture(&options.repo)?;
    let derived = derive_role_csv(&source)?;
    let redactor = Redactor::new(dataset.profile.roles.iter().map(|r| r.value.as_str()))?;

    let eval_dir = fs::canonicalize(create_evaluation_dir(&options.output)?)
        .map_err(io("evaluation directory"))?;
    let input = eval_dir.join(INPUT_FILE);
    fs::write(&input, &derived.bytes).map_err(io("derived input"))?;

    let mut cases = Vec::with_capacity(generation.cases.len());
    for case in &generation.cases {
        let replay = replay_case(
            case, &dataset, &binary, &eval_dir, &input, &derived, options,
        );
        let mut report = build_case_report(case, replay.classification, &reviews);
        report.evidence = replay.evidence;
        report.evidence.lexical = lexical_facts(&case.prompt);
        cases.push(report);
    }

    let limitations = redactor.limitations();
    let mut notes = vec![
        "rows_match means agreement on this dataset only, not semantic correctness".to_owned(),
        "counts describe this candidate batch, not real user phrasing".to_owned(),
        "interpretation text is not collected; plan evidence comes from structured artifacts"
            .to_owned(),
    ];
    if !limitations.is_empty() {
        notes.push(
            "some stand-ins cannot preserve properties of the source values; \
             withhold semantic conclusions for them"
                .to_owned(),
        );
    }
    let spawn_failures = cases
        .iter()
        .filter(|c| c.evidence.spawn_failure.is_some())
        .count();
    if spawn_failures > 0 {
        notes.push(format!(
            "{spawn_failures} of {} case(s) could not start the baho-cli process \
             (evidence.spawn_failure); their `error` results say nothing about baho behavior",
            cases.len()
        ));
    }
    let report = EvaluationReport {
        schema_version: REPORT_SCHEMA_VERSION,
        evaluation_id: eval_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned(),
        generation_id: generation.id,
        generation_hash: generation.generation_hash,
        candidates_hash: generation.candidates_hash,
        metadata_hash: generation.metadata_hash,
        dataset_hash: generation.dataset_hash,
        provenance: ToolProvenance {
            binary_sha256: binary.sha256,
            binary_version: binary.version,
            repo_revision: repo.revision,
            repo_dirty: repo.dirty,
        },
        aggregates: aggregate(&cases),
        limitations,
        notes,
        cases,
    };

    // Every model-visible free-text field goes through `sanitize_checked` inside
    // `sanitize_report`; a field that cannot be made safe aborts publication.
    let mut clean = redactor.sanitize_report(&report)?;
    for case in &mut clean.cases {
        // Bounds apply after sanitization. A prefix of a clean string is still clean.
        case.evidence.diagnostic_messages.truncate(MAX_MESSAGES);
        for m in &mut case.evidence.diagnostic_messages {
            *m = m.chars().take(MAX_MESSAGE_CHARS).collect();
        }
    }
    let bytes = serde_json::to_vec_pretty(&clean).map_err(|e| EvaluateError::Io {
        what: "report serialization",
        reason: e.to_string(),
    })?;
    // Final scan of the exact bytes for absolute paths. Literals and credentials were
    // checked per free-text field; the tool's own IDs (long, digit-bearing, dash-joined)
    // look credential-shaped to the token scanner, so they are not scanned for that.
    let text = String::from_utf8_lossy(&bytes);
    let leaked: Vec<ViolationKind> = redactor
        .scan(&text)
        .into_iter()
        .filter(|k| *k == ViolationKind::AbsolutePath)
        .collect();
    if !leaked.is_empty() {
        return Err(RedactionError::Unsafe(leaked).into());
    }

    let evaluation_hash = format!("{:x}", Sha256::digest(&bytes));
    let sidecar_path = eval_dir.join(SIDECAR_FILE);
    write_sidecar(&sidecar_path, &redactor.to_sidecar(&evaluation_hash))?;
    let report_path = eval_dir.join(REPORT_FILE);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .map_err(io("report"))?;
    file.write_all(&bytes).map_err(io("report"))?;
    Ok(EvaluateOutcome {
        dir: eval_dir,
        report_path,
        sidecar_path,
        evaluation_hash,
        report: clean,
    })
}

fn read_review(path: &Path) -> Result<ReviewFile, EvaluateError> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(io("review file"))?
        .take(MAX_REVIEW_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io("review file"))?;
    if bytes.len() as u64 > MAX_REVIEW_BYTES {
        return Err(ContractError::Invalid {
            what: "review file",
            reason: "exceeds size bound".into(),
        }
        .into());
    }
    Ok(ReviewFile::parse(&bytes)?)
}

struct Replay {
    classification: crate::evaluation::Classification,
    evidence: CaseEvidence,
}

fn replay_case(
    case: &IntakeCase,
    dataset: &LoadedDataset,
    binary: &CliBinary,
    eval_dir: &Path,
    input: &Path,
    derived: &DerivedCsv,
    options: &EvaluateOptions,
) -> Replay {
    let oracle = recheck_oracle(case, dataset, options.oracle_timeout);
    let mut evidence = CaseEvidence::default();
    let (mut run, mut parsed) = (None, None);
    match &oracle {
        OracleCheck::Verified { .. } => {
            // A failed spawn or unreadable artifacts classify as `error`; they never abort
            // the other cases.
            match run_case(binary, eval_dir, input, &case.prompt, options.limits) {
                Ok(r) => run = Some(r),
                Err(e) => evidence.spawn_failure = Some(spawn_failure_code(&e).to_owned()),
            }
            parsed = run
                .as_ref()
                .and_then(|r| r.run_dir.as_deref())
                .and_then(|dir| parse_run(dir).ok())
                .map(|p| map_rows(p, derived));
            evidence.run_id = run.as_ref().and_then(|r| r.run_id.clone());
        }
        OracleCheck::Mismatch { .. } => evidence.oracle_code = Some("oracle_mismatch".into()),
        OracleCheck::Error { code } => evidence.oracle_code = Some((*code).to_owned()),
    }
    if let Some(p) = &parsed {
        evidence.refusal_reason = p.refusal_reason.clone();
        evidence.plan = p.plan_evidence.clone();
        evidence.diagnostic_messages = p
            .diagnostics
            .iter()
            .take(MAX_MESSAGES)
            .map(|d| d.message.clone())
            .collect();
    }
    let classification = classify(
        &oracle,
        dataset.profile.column,
        &case.expected_rows,
        run.as_ref(),
        parsed.as_ref(),
    );
    Replay {
        classification,
        evidence,
    }
}

/// Translates derived-CSV row numbers to source rows; an unmappable row drops the rows so
/// the case is classified as an error rather than compared on a guess.
fn map_rows(mut parsed: ParsedRun, derived: &DerivedCsv) -> ParsedRun {
    parsed.result_rows = parsed.result_rows.and_then(|rows| {
        rows.into_iter()
            .map(|r| derived.source_row(r))
            .collect::<Option<Vec<_>>>()
    });
    parsed
}

pub fn lexical_facts(prompt: &str) -> LexicalFacts {
    LexicalFacts {
        words: prompt.split_whitespace().count(),
        chars: prompt.chars().count(),
        has_quote: prompt.contains(['\'', '"']),
        ends_with_punctuation: prompt
            .trim_end()
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_punctuation()),
    }
}

/// Separate tallies per assessment; no combined pass rate.
pub fn aggregate(cases: &[CaseReport]) -> Aggregates {
    let mut observed = BTreeMap::new();
    let mut review = BTreeMap::new();
    let mut plan_comparison: BTreeMap<PlanComparison, usize> = BTreeMap::new();
    for c in cases {
        *observed.entry(c.observed).or_insert(0) += 1;
        *review.entry(c.review).or_insert(0) += 1;
        *plan_comparison.entry(c.plan_comparison).or_insert(0) += 1;
    }
    let count = |r| cases.iter().filter(|c| c.observed == r).count();
    Aggregates {
        cases: cases.len(),
        observed,
        review,
        plan_comparison,
        confirmed_wrong_plan: cases.iter().filter(|c| c.is_confirmed_wrong_plan()).count(),
        materialized: count(ObservedResult::RowsMatch) + count(ObservedResult::WrongRows),
        rows_match: count(ObservedResult::RowsMatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_run::stub::{Mode, write as write_stub};
    use crate::contracts::{ReviewStatus, SIDECAR_SCHEMA_VERSION};
    use crate::redaction::load_sidecar;

    const GEN: &str = "generation-1000-7";
    const SQL_A: &str =
        "SELECT _row FROM data WHERE lower(\"Role\") = lower('Analyst') ORDER BY _row";
    const SQL_L: &str = "SELECT _row FROM data WHERE lower(\"Role\") = lower('Lead') ORDER BY _row";

    struct Fixture {
        tmp: tempfile::TempDir,
        options: EvaluateOptions,
    }

    fn fixture(mode: Mode, cases: &[(&str, &str, &str)]) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let csv = tmp.path().join("data.csv");
        fs::write(&csv, "ID,Role\n1,Analyst\n2,analyst\n3,Lead\n").unwrap();
        let hash = LoadedDataset::load(&csv).unwrap().hash;
        let out = tmp.path().join("out");
        let dir = out.join(GEN);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("generation.json"),
            format!(r#"{{"schema_version":1,"id":"{GEN}","dataset_hash":"{hash}"}}"#),
        )
        .unwrap();
        let (mut text, mut meta) = (String::new(), String::new());
        for (n, (prompt, sql, rows)) in cases.iter().enumerate() {
            let id = format!("{GEN}-{}", n + 1);
            text.push_str(&format!(
                "case_id: {id}\nprompt:  {prompt}\nsql:     {sql}\n\n"
            ));
            meta.push_str(&format!(
                r#"{{"schema_version":2,"case_id":"{id}","dataset_hash":"{hash}","expected_rows":{rows},"generation_record_id":"{GEN}"}}"#
            ));
            meta.push('\n');
        }
        fs::write(dir.join("candidates.txt"), text).unwrap();
        fs::write(dir.join("metadata.jsonl"), meta).unwrap();
        let bin = write_stub(tmp.path(), mode);
        let options = EvaluateOptions {
            dataset: csv,
            generation: None,
            baho_bin: bin,
            review: None,
            output: out,
            repo: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            limits: RunLimits::default(),
            oracle_timeout: Duration::from_secs(30),
        };
        Fixture { tmp, options }
    }

    fn two_cases() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            ("Show rows where Role = Analyst", SQL_A, "[1,2]"),
            ("Show rows where Role = Lead", SQL_L, "[3]"),
        ]
    }

    fn eval_dirs(out: &Path) -> usize {
        fs::read_dir(out)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("evaluation-")
            })
            .count()
    }

    #[test]
    fn publishes_sanitized_report_and_matching_private_sidecar() {
        let f = fixture(Mode::Materialize, &two_cases());
        let out = evaluate(&f.options).unwrap();
        let text = fs::read_to_string(&out.report_path).unwrap();
        let lower = text.to_lowercase();
        assert!(
            !lower.contains("analyst") && !lower.contains("lead"),
            "{text}"
        );
        assert!(!text.contains(f.tmp.path().to_str().unwrap()));
        assert_eq!(out.report.aggregates.rows_match, 2);
        assert_eq!(out.report.aggregates.confirmed_wrong_plan, 0);
        for c in &out.report.cases {
            assert_eq!(c.observed, ObservedResult::RowsMatch);
            assert_eq!(c.plan_comparison, PlanComparison::Equivalent);
            assert_eq!(c.review, ReviewStatus::Unreviewed);
            assert_eq!(c.row_counts.unwrap().missing, 0);
            assert!(c.evidence.run_id.is_some());
        }
        // Report bytes hash to the evaluation hash the sidecar is bound to.
        let on_disk = fs::read(&out.report_path).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&on_disk)),
            out.evaluation_hash
        );
        let redactor = load_sidecar(&out.sidecar_path, &out.evaluation_hash).unwrap();
        assert!(!redactor.scan("Analyst").is_empty());
        assert!(load_sidecar(&out.sidecar_path, "other").is_err());
        let sidecar: crate::contracts::RedactionSidecar =
            serde_json::from_slice(&fs::read(&out.sidecar_path).unwrap()).unwrap();
        assert_eq!(sidecar.schema_version, SIDECAR_SCHEMA_VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out.sidecar_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        // Raw artifacts stay private in the evaluation directory.
        assert!(out.dir.join(".baho/runs").is_dir());
        assert_eq!(out.report.cases[0].evidence.lexical.words, 6);
    }

    #[test]
    fn repeated_runs_are_deterministic_apart_from_volatile_metadata() {
        let f = fixture(Mode::Materialize, &two_cases());
        let strip = |mut r: EvaluationReport| {
            r.evaluation_id.clear();
            for c in &mut r.cases {
                c.evidence.run_id = None;
            }
            r
        };
        let a = strip(evaluate(&f.options).unwrap().report);
        let b = strip(evaluate(&f.options).unwrap().report);
        assert_eq!(a, b);
        assert_eq!(eval_dirs(&f.options.output), 2);
    }

    #[test]
    fn refusals_are_reported_with_codes_and_no_row_lists() {
        let f = fixture(Mode::Refuse, &two_cases());
        let out = evaluate(&f.options).unwrap();
        let c = &out.report.cases[0];
        assert_eq!(c.observed, ObservedResult::Refused);
        assert_eq!(c.plan_comparison, PlanComparison::Unavailable);
        assert!(
            c.diagnostic_codes
                .contains(&"intent.column_not_found".to_owned())
        );
        assert_eq!(
            c.evidence.refusal_reason.as_deref(),
            Some("intent.column_not_found")
        );
        assert_eq!(out.report.aggregates.observed[&ObservedResult::Refused], 2);
        assert_eq!(out.report.aggregates.materialized, 0);
    }

    #[test]
    fn oracle_mismatch_is_not_replayed() {
        let f = fixture(
            Mode::Materialize,
            &[("Show rows where Role = Analyst", SQL_A, "[1,3]")],
        );
        let out = evaluate(&f.options).unwrap();
        let c = &out.report.cases[0];
        assert_eq!(c.observed, ObservedResult::OracleError);
        assert_eq!(c.evidence.run_id, None);
        assert_eq!(c.evidence.oracle_code.as_deref(), Some("oracle_mismatch"));
        assert!(!out.dir.join(".baho").exists());
    }

    #[test]
    fn review_rationale_is_sanitized_and_bound_decisions_are_imported() {
        let mut f = fixture(Mode::Materialize, &two_cases());
        let probe = evaluate(&f.options).unwrap();
        let hash = probe.report.cases[0].candidate_hash.clone();
        let review = f.tmp.path().join("review.json");
        fs::write(
            &review,
            format!(
                r#"{{"schema_version":1,"generation_id":"{GEN}","decisions":[{{"case_id":"{GEN}-1","candidate_hash":"{hash}","decision":"confirmed","rationale":"analyst rows listed in /Users/someone/private.csv"}}]}}"#
            ),
        )
        .unwrap();
        f.options.review = Some(review);
        let out = evaluate(&f.options).unwrap();
        let c = &out.report.cases[0];
        assert_eq!(c.review, ReviewStatus::Confirmed);
        assert_eq!(out.report.cases[1].review, ReviewStatus::Unreviewed);
        let rationale = c.review_rationale.as_deref().unwrap();
        assert!(!rationale.to_lowercase().contains("analyst"));
        assert!(!rationale.contains("/Users/"));
    }

    #[test]
    fn invalid_review_fails_before_creating_an_evaluation_directory() {
        let mut f = fixture(Mode::Materialize, &two_cases());
        let review = f.tmp.path().join("review.json");
        fs::write(
            &review,
            format!(
                r#"{{"schema_version":1,"generation_id":"{GEN}","decisions":[{{"case_id":"{GEN}-1","candidate_hash":"stale","decision":"confirmed"}}]}}"#
            ),
        )
        .unwrap();
        f.options.review = Some(review);
        assert!(matches!(
            evaluate(&f.options),
            Err(EvaluateError::Contract(_))
        ));
        assert_eq!(eval_dirs(&f.options.output), 0);
    }

    #[test]
    fn cli_failures_become_error_cases_not_aborts() {
        let f = fixture(Mode::FutureSchema, &two_cases());
        let out = evaluate(&f.options).unwrap();
        assert!(
            out.report
                .cases
                .iter()
                .all(|c| c.observed == ObservedResult::Error)
        );
    }

    #[test]
    fn unspawnable_binary_is_visible_in_report_and_outcome() {
        let f = fixture(Mode::Materialize, &two_cases());
        // Passes the probe, then disappears before any case runs is not reproducible
        // here, so exercise the replay path directly with a missing binary.
        let ghost = CliBinary {
            path: f.tmp.path().join("ghost"),
            sha256: String::new(),
            version: String::new(),
        };
        let dataset = LoadedDataset::load(&f.options.dataset).unwrap();
        let generation_dir = f.options.output.join(GEN);
        let generation = load_generation(&generation_dir, &dataset).unwrap();
        let derived = derive_role_csv(&fs::read(&f.options.dataset).unwrap()).unwrap();
        let replay = replay_case(
            &generation.cases[0],
            &dataset,
            &ghost,
            f.tmp.path(),
            f.tmp.path(),
            &derived,
            &f.options,
        );
        assert_eq!(
            replay.evidence.spawn_failure.as_deref(),
            Some("spawn_not_found")
        );
        assert_eq!(replay.evidence.run_id, None);
    }

    #[test]
    fn healthy_runs_report_no_spawn_failure() {
        let f = fixture(Mode::Materialize, &two_cases());
        let out = evaluate(&f.options).unwrap();
        assert!(!out.every_replay_failed_to_spawn());
        assert!(
            out.report
                .cases
                .iter()
                .all(|c| c.evidence.spawn_failure.is_none())
        );
    }

    #[test]
    fn wrong_dataset_and_bad_binary_are_rejected() {
        let mut f = fixture(Mode::Materialize, &two_cases());
        fs::write(&f.options.dataset, "ID,Role\n1,Other\n").unwrap();
        assert!(matches!(
            evaluate(&f.options),
            Err(EvaluateError::Intake(IntakeError::DatasetMismatch))
        ));
        let mut g = fixture(Mode::BadVersion, &two_cases());
        g.options.generation = Some(g.options.output.join(GEN));
        assert!(matches!(evaluate(&g.options), Err(EvaluateError::Cli(_))));
        f.options.output = f.tmp.path().join("missing");
        assert!(evaluate(&f.options).is_err());
    }

    #[test]
    fn aggregates_keep_assessments_separate() {
        let case = |review, observed, plan| CaseReport {
            case_id: "c".into(),
            candidate_hash: "h".into(),
            prompt: "p".into(),
            review,
            review_rationale: None,
            observed,
            plan_comparison: plan,
            diagnostic_codes: vec![],
            row_counts: None,
            evidence: CaseEvidence::default(),
        };
        let a = aggregate(&[
            case(
                ReviewStatus::Confirmed,
                ObservedResult::WrongRows,
                PlanComparison::Different,
            ),
            case(
                ReviewStatus::Unreviewed,
                ObservedResult::RowsMatch,
                PlanComparison::Equivalent,
            ),
            case(
                ReviewStatus::Unreviewed,
                ObservedResult::Refused,
                PlanComparison::Unavailable,
            ),
        ]);
        assert_eq!(a.cases, 3);
        assert_eq!(a.confirmed_wrong_plan, 1);
        assert_eq!((a.materialized, a.rows_match), (2, 1));
        assert_eq!(a.review[&ReviewStatus::Unreviewed], 2);
        assert_eq!(a.plan_comparison[&PlanComparison::Different], 1);
    }

    #[test]
    fn lexical_facts_are_plain_counts() {
        let f = lexical_facts("Please keep 'x' rows.");
        assert_eq!((f.words, f.chars), (4, 21));
        assert!(f.has_quote && f.ends_with_punctuation);
    }
}
