//! Trusted intake for one reviewed evaluation case and pinned baseline.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rig::{DynModel, operation::Completion};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cli_run::derive_role_csv;
use crate::contracts::{EvaluationReport, ReviewDecision, ReviewFile};
use crate::dataset::LoadedDataset;
use crate::intake::load_generation;
use crate::redaction::{Redactor, load_sidecar};
use crate::repair::{
    ATTEMPT_SCHEMA, AttemptRecord, AttemptSettings, FrozenCase, Outcome, RepairContract,
    SyntheticReduction, run_attempt,
};
use crate::repair_agents::RigRepairAgents;
use crate::repair_build::BuildError;
use crate::repair_live::{LiveWorker, PrivateCase, SharedLiveWorker};
use crate::repair_progress::RepairProgress;
use crate::sql::{SqlRunner, parse_role_query};

#[derive(Debug, Error)]
pub enum EntryError {
    #[error("invalid repair intake: {0}")]
    Invalid(&'static str),
    #[error("repair I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("repair contract: {0}")]
    Contract(#[from] crate::repair::RepairError),
}

fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(_: impl std::fmt::Display) -> EntryError {
    EntryError::Invalid("source, oracle, or schema verification failed")
}

fn generation_path(evaluation: &Path, generation_id: &str) -> Result<PathBuf, EntryError> {
    let output_root = evaluation
        .parent()
        .and_then(Path::parent)
        .ok_or(EntryError::Invalid("evaluation path"))?;
    Ok(output_root.join(generation_id))
}

pub struct RepairOptions {
    pub repo: PathBuf,
    pub evaluation: PathBuf,
    pub case_id: String,
    pub baseline_revision: String,
    pub dataset: PathBuf,
    pub review: PathBuf,
    pub output: PathBuf,
    pub settings: AttemptSettings,
}

impl RepairOptions {
    pub fn default_settings() -> AttemptSettings {
        AttemptSettings {
            investigator_prompt_version: "repair-investigator-v5".into(),
            coder_prompt_version: "repair-coder-v2".into(),
            reviewer_prompt_version: "repair-reviewer-v2".into(),
            model: "neuralwatt".into(),
            max_investigator_turns: 12,
            max_agent_turns: 6,
            max_tool_calls: 20,
            api_request_timeout_seconds: 600,
            process_timeout_seconds: 300,
            max_output_bytes: 64 * 1024,
            max_read_bytes: 16 * 1024,
            max_outline_bytes: 6 * 1024,
            max_search_matches: 40,
            max_exposed_bytes: 48 * 1024,
        }
    }
}

/// Validate all private inputs before reserving an attempt. No private source
/// values or exact row lists are sent to the agent or public record.
pub fn prepare(
    options: &RepairOptions,
) -> Result<(RepairContract, Vec<PrivateCase>, String, Redactor), EntryError> {
    if options.baseline_revision.len() != 40
        || !options
            .baseline_revision
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(EntryError::Invalid("baseline must be a full commit hash"));
    }
    let evaluation_bytes = fs::read(&options.evaluation)?;
    if evaluation_bytes.len() > 2 * 1024 * 1024 {
        return Err(EntryError::Invalid("evaluation exceeds bound"));
    }
    let report: EvaluationReport = serde_json::from_slice(&evaluation_bytes).map_err(invalid)?;
    report.check_version().map_err(invalid)?;
    let redactor = load_sidecar(
        &options.evaluation.with_file_name("redaction-sidecar.json"),
        &hash(&evaluation_bytes),
    )
    .map_err(invalid)?;
    let target_report = report
        .cases
        .iter()
        .find(|c| c.case_id == options.case_id)
        .ok_or(EntryError::Invalid("case absent from evaluation"))?;
    if !target_report.is_confirmed_wrong_plan() {
        return Err(EntryError::Invalid("case is not a reviewed wrong plan"));
    }
    let dataset = LoadedDataset::load(&options.dataset).map_err(invalid)?;
    if dataset.hash != report.dataset_hash {
        return Err(EntryError::Invalid("dataset hash differs"));
    }
    let generation = load_generation(
        &generation_path(&options.evaluation, &report.generation_id)?,
        &dataset,
    )
    .map_err(invalid)?;
    if generation.generation_hash != report.generation_hash
        || generation.candidates_hash != report.candidates_hash
        || generation.metadata_hash != report.metadata_hash
    {
        return Err(EntryError::Invalid("generation hashes differ"));
    }
    let review_bytes = fs::read(&options.review)?;
    let review = ReviewFile::parse(&review_bytes).map_err(invalid)?;
    if review.generation_id != generation.id
        || !review.decisions.iter().any(|d| {
            d.case_id == options.case_id
                && d.candidate_hash == target_report.candidate_hash
                && d.decision == ReviewDecision::Confirmed
        })
    {
        return Err(EntryError::Invalid("review is not bound to target"));
    }
    let source = fs::read(&options.dataset)?;
    let derived = derive_role_csv(&source).map_err(invalid)?;
    let target_csv = derived.bytes;
    let target_case = generation
        .cases
        .iter()
        .find(|c| c.case_id == options.case_id && c.candidate_hash == target_report.candidate_hash)
        .ok_or(EntryError::Invalid("candidate hash differs"))?;
    let mut private = Vec::new();
    let mut freeze = |case: &crate::intake::IntakeCase| -> Result<FrozenCase, EntryError> {
        let query = parse_role_query(&case.sql, &dataset.profile).map_err(invalid)?;
        let rows = SqlRunner::new(
            dataset.database_path(),
            &dataset.profile,
            Duration::from_secs(30),
        )
        .run(&case.sql)
        .map_err(invalid)?
        .rows;
        if rows != case.expected_rows {
            return Err(EntryError::Invalid(
                "recorded oracle differs from recomputation",
            ));
        }
        private.push(PrivateCase {
            csv: target_csv.clone(),
            prompt: case.prompt.clone(),
        });
        Ok(FrozenCase {
            fixture_sha256: hash(&target_csv),
            prompt_sha256: hash(&case.prompt),
            sql_sha256: hash(&case.sql),
            expected_source_rows: rows,
            expected_literal: query.literal,
        })
    };
    let target = freeze(target_case)?;
    let mut protected_cases = Vec::new();
    for case in &generation.cases {
        if case.case_id == options.case_id {
            continue;
        }
        if protected_cases.len() >= 64 {
            return Err(EntryError::Invalid("comparison batch exceeds bound"));
        }
        protected_cases.push(freeze(case)?);
    }
    // Provisional fields make the initial record schema-valid. They are never
    // executed: the investigator's reduction replaces them after reproduction.
    let reduction = SyntheticReduction {
        csv: "Id,Role\n1,Alpha\n2,Beta\n".into(),
        prompt: "show rows where Role = Alpha".into(),
        sql: "SELECT _row FROM data WHERE lower(\"Role\") = lower('Alpha') ORDER BY _row".into(),
    };
    let reduced = FrozenCase {
        fixture_sha256: hash(&reduction.csv),
        prompt_sha256: hash(&reduction.prompt),
        sql_sha256: hash(&reduction.sql),
        expected_source_rows: vec![1],
        expected_literal: "Alpha".into(),
    };
    let contract = RepairContract {
        schema_version: ATTEMPT_SCHEMA,
        settings: options.settings.clone(),
        baseline_revision: options.baseline_revision.clone(),
        evaluation_sha256: hash(&evaluation_bytes),
        generation_sha256: generation.generation_hash,
        review_sha256: hash(&review_bytes),
        oracle_sha256: hash(format!("{}:{}", dataset.hash, hash(&target_csv))),
        target_case_id: options.case_id.clone(),
        target,
        reduced,
        reduction,
        protected_cases,
        parent_attempt_id: None,
    };
    contract.validate()?;
    Ok((contract, private, target_report.prompt.clone(), redactor))
}

pub fn repair(
    model: DynModel<Completion>,
    options: &RepairOptions,
) -> Result<(Outcome, PathBuf), EntryError> {
    repair_with_progress(model, options, None)
}

pub fn repair_with_progress(
    model: DynModel<Completion>,
    options: &RepairOptions,
    progress: Option<RepairProgress>,
) -> Result<(Outcome, PathBuf), EntryError> {
    let (contract, cases, sanitized_prompt, redactor) = prepare(options)?;
    let mut record = AttemptRecord::reserve(&options.output, &contract)?;
    if let Some(progress) = &progress {
        record = record.with_progress(progress.clone());
        progress.message(&format!("repair attempt: {}", record.dir.display()));
    }
    let checkout = record.dir.join("checkout");
    let worker = LiveWorker::new(
        &options.repo,
        &record.dir,
        &contract.baseline_revision,
        Duration::from_secs(contract.settings.process_timeout_seconds),
        cases,
    );
    let worker = match worker {
        Ok(worker) => worker,
        Err(error) => {
            let reason = match error {
                BuildError::WorkerUnavailable(reason) => reason,
                _ => "confined worker setup failed",
            };
            record.fail_before_start(Outcome::EnvironmentFailure, reason)?;
            return Ok((Outcome::EnvironmentFailure, record.dir));
        }
    };
    let mut worker = SharedLiveWorker::new(worker, checkout.clone());
    let mut agents = RigRepairAgents::new(
        model,
        contract.settings.clone(),
        checkout,
        sanitized_prompt,
        redactor,
        worker.inner.clone(),
    );
    if let Some(progress) = progress {
        agents = agents.with_progress(progress);
    }
    let outcome = run_attempt(&mut record, &contract, &mut worker, &mut agents)?;
    Ok((outcome, record.dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_sibling_of_evaluation_directory() {
        let evaluation = Path::new("synthetic/out/evaluation-123/evaluation.json");
        assert_eq!(
            generation_path(evaluation, "generation-456").unwrap(),
            Path::new("synthetic/out/generation-456")
        );
    }

    #[test]
    fn default_timeouts_separate_provider_request_from_worker_process() {
        let settings = RepairOptions::default_settings();
        assert_eq!(settings.api_request_timeout_seconds, 600);
        assert_eq!(settings.process_timeout_seconds, 300);
    }
}
