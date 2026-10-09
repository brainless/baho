//! Evidence handoff between the read-only investigation and epic writing stages.
use crate::{
    contracts::{EvaluationReport, EvidenceEligibility},
    planner_tools::{ToolError, Toolbox},
    redaction::Redactor,
};
use rig::tool::{PortableTool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

// Version 2 adds the investigator's complete proposal; version 1 had evidence only.
pub const INVESTIGATION_SCHEMA_VERSION: u32 = 2;
pub const INVESTIGATION_PROMPT: &str = include_str!("../prompts/planner-investigation.md");
const MAX_BYTES: usize = 32 * 1024;
const MAX_ATTEMPTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCitation {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptTrace {
    pub case_id: String,
    pub entry_path: String,
    pub trace: String,
    pub citations: Vec<EvidenceCitation>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPath {
    pub name: String,
    pub rationale: String,
    pub citations: Vec<EvidenceCitation>,
    pub tests: Vec<EvidenceCitation>,
}
/// The investigator owns the design decisions; the following agent edits this into an epic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalDraft {
    pub defect: String,
    pub desired_behavior: String,
    pub design: String,
    pub compatibility_obligations: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub fixture_strategy: String,
    pub uncertainties: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Investigation {
    pub schema_version: u32,
    pub proposal_kind: String,
    pub motivating_case_ids: Vec<String>,
    pub prompt_traces: Vec<PromptTrace>,
    pub relevant_tests: Vec<EvidenceCitation>,
    pub competing_interpretations: Vec<String>,
    pub chosen_scope: String,
    pub changed_entry_paths: Vec<EntryPath>,
    pub deferred_scope: Vec<String>,
    pub proposal: ProposalDraft,
}
#[derive(Default)]
struct State {
    accepted: Option<Investigation>,
    attempts: usize,
    rejections: Vec<Vec<String>>,
}
struct Inner {
    eligibility: BTreeMap<String, EvidenceEligibility>,
    redactor: Redactor,
    toolbox: Toolbox,
    state: Mutex<State>,
}
#[derive(Clone)]
pub struct SubmitInvestigation(Arc<Inner>);
impl SubmitInvestigation {
    pub fn new(report: &EvaluationReport, redactor: Redactor, toolbox: Toolbox) -> Self {
        Self(Arc::new(Inner {
            eligibility: report
                .cases
                .iter()
                .map(|c| (c.case_id.clone(), report.evidence_eligibility(c)))
                .collect(),
            redactor,
            toolbox,
            state: Mutex::new(State::default()),
        }))
    }
    pub fn accepted(&self) -> Option<Investigation> {
        self.0.state.lock().unwrap().accepted.clone()
    }
    pub fn rejections(&self) -> Vec<Vec<String>> {
        self.0.state.lock().unwrap().rejections.clone()
    }
    pub fn submit(&self, investigation: Investigation) -> Result<Value, ToolError> {
        let mut state = self.0.state.lock().unwrap();
        if state.accepted.is_some() {
            return Err(ToolError("investigation already accepted".into()));
        }
        if state.attempts >= MAX_ATTEMPTS {
            return Err(ToolError("investigation attempt budget exhausted".into()));
        }
        state.attempts += 1;
        let reasons = self.validate(&investigation);
        if !reasons.is_empty() {
            state.rejections.push(reasons.clone());
            return Err(ToolError(reasons.join("; ")));
        }
        state.accepted = Some(investigation);
        Ok(json!({"accepted":true,"next_stage":"draft_epic"}))
    }
    fn validate(&self, i: &Investigation) -> Vec<String> {
        let mut errors = Vec::new();
        let mut require = |ok: bool, message: &str| {
            if !ok {
                errors.push(message.to_owned());
            }
        };
        require(
            i.schema_version == INVESTIGATION_SCHEMA_VERSION,
            "unsupported investigation schema version; expected 2",
        );
        require(
            matches!(i.proposal_kind.as_str(), "implementation" | "investigation"),
            "proposal_kind must be implementation or investigation",
        );
        let encoded = serde_json::to_vec(i).unwrap_or_default();
        require(
            !encoded.is_empty() && encoded.len() <= MAX_BYTES,
            "investigation exceeds 32768 bytes",
        );
        if encoded.is_empty() || encoded.len() > MAX_BYTES {
            return errors;
        }
        let ids: BTreeSet<_> = i.motivating_case_ids.iter().collect();
        require(
            !ids.is_empty() && ids.len() <= 100 && ids.len() == i.motivating_case_ids.len(),
            "name 1 to 100 unique motivating case IDs",
        );
        for id in &ids {
            require(
                self.0.eligibility.contains_key(*id),
                "unknown motivating case ID",
            );
            require(
                i.proposal_kind != "implementation"
                    || self.0.eligibility.get(*id) != Some(&EvidenceEligibility::Observation),
                "observation-only cases cannot motivate implementation",
            );
            require(
                i.prompt_traces.iter().any(|t| &t.case_id == *id),
                "every motivating case requires a prompt trace",
            );
        }
        let text_ok = |s: &str| {
            !s.trim().is_empty()
                && s.len() <= 4096
                && !s.contains('\0')
                && self.0.redactor.scan_prose(s).is_empty()
        };
        require(
            text_ok(&i.chosen_scope),
            "chosen_scope must be nonempty, bounded, and redaction-safe",
        );
        require(
            [
                &i.proposal.defect,
                &i.proposal.desired_behavior,
                &i.proposal.design,
                &i.proposal.fixture_strategy,
            ]
            .iter()
            .all(|s| text_ok(s)),
            "proposal defect, desired behavior, design, and fixture strategy must be nonempty, bounded, and redaction-safe",
        );
        require(
            !i.proposal.acceptance_criteria.is_empty()
                && i.proposal.acceptance_criteria.len() <= 32
                && i.proposal.acceptance_criteria.iter().all(|s| text_ok(s)),
            "proposal needs 1 to 32 bounded, safe acceptance criteria",
        );
        for items in [
            &i.proposal.compatibility_obligations,
            &i.proposal.uncertainties,
        ] {
            require(
                items.len() <= 32 && items.iter().all(|s| text_ok(s)),
                "proposal compatibility obligations and uncertainties must be bounded and redaction-safe",
            );
        }
        require(
            !i.competing_interpretations.is_empty()
                && i.competing_interpretations.len() <= 16
                && i.competing_interpretations.iter().all(|s| text_ok(s)),
            "provide bounded, safe competing interpretations",
        );
        require(
            i.deferred_scope.len() <= 32 && i.deferred_scope.iter().all(|s| text_ok(s)),
            "deferred scope must be bounded and redaction-safe",
        );
        require(
            (i.proposal_kind == "investigation" || !i.changed_entry_paths.is_empty())
                && i.changed_entry_paths.len() <= 16,
            "identify the proposed entry paths",
        );
        let paths: BTreeSet<_> = i.changed_entry_paths.iter().map(|p| &p.name).collect();
        require(
            paths.len() == i.changed_entry_paths.len(),
            "entry path names must be unique",
        );
        require(!i.relevant_tests.is_empty(), "cite existing relevant tests");
        require(
            !i.prompt_traces.is_empty() && i.prompt_traces.len() <= 100,
            "provide 1 to 100 prompt traces",
        );
        for trace in &i.prompt_traces {
            require(
                ids.contains(&trace.case_id),
                "trace must identify a motivating case",
            );
            require(
                i.proposal_kind == "investigation" || paths.contains(&trace.entry_path),
                "trace must name a proposed entry path",
            );
            require(
                text_ok(&trace.trace) && text_ok(&trace.entry_path) && !trace.citations.is_empty(),
                "each trace needs safe prose and source citations",
            );
        }
        for path in &i.changed_entry_paths {
            require(
                text_ok(&path.name)
                    && text_ok(&path.rationale)
                    && !path.citations.is_empty()
                    && !path.tests.is_empty(),
                "each entry path needs safe rationale, source citations, and existing tests",
            );
        }
        drop(require);
        let citations = i
            .relevant_tests
            .iter()
            .chain(i.prompt_traces.iter().flat_map(|t| &t.citations))
            .chain(
                i.changed_entry_paths
                    .iter()
                    .flat_map(|p| p.citations.iter().chain(&p.tests)),
            );
        for citation in citations {
            if !text_ok(&citation.path)
                || self
                    .0
                    .toolbox
                    .validate_citation(&citation.path, citation.start_line, citation.end_line)
                    .is_err()
            {
                errors.push(
                    "citation must resolve to an existing allowlisted file and valid line range"
                        .into(),
                );
            }
        }
        errors.sort();
        errors.dedup();
        errors
    }
}
impl PortableTool for SubmitInvestigation {
    const NAME: &'static str = "submit_investigation";
    type Args = Investigation;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_code("harness_rejection")
    }
    fn description(&self) -> String {
        "Submit investigated evidence and a complete small proposal for an editor to format as an epic. No files are written. Correct rejected submissions; stop after acceptance.".into()
    }
    fn parameters(&self) -> Value {
        let citation = json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path","start_line","end_line"],"additionalProperties":false});
        let citations = json!({"type":"array","items":citation,"minItems":1,"maxItems":32});
        let strings =
            json!({"type":"array","items":{"type":"string","maxLength":4096},"maxItems":32});
        json!({"type":"object","properties":{
            "schema_version":{"type":"integer","enum":[INVESTIGATION_SCHEMA_VERSION]},"proposal_kind":{"type":"string","enum":["implementation","investigation"]},
            "motivating_case_ids":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":100},
            "prompt_traces":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"object","properties":{"case_id":{"type":"string"},"entry_path":{"type":"string"},"trace":{"type":"string","maxLength":4096},"citations":citations},"required":["case_id","entry_path","trace","citations"],"additionalProperties":false}},
            "relevant_tests":citations,"competing_interpretations":strings,"chosen_scope":{"type":"string","maxLength":4096},
            "changed_entry_paths":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"name":{"type":"string"},"rationale":{"type":"string","maxLength":4096},"citations":citations,"tests":citations},"required":["name","rationale","citations","tests"],"additionalProperties":false}},
            "deferred_scope":strings,
            "proposal":{"type":"object","properties":{
                "defect":{"type":"string","maxLength":4096},"desired_behavior":{"type":"string","maxLength":4096},"design":{"type":"string","maxLength":4096},
                "compatibility_obligations":strings,"acceptance_criteria":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"string","maxLength":4096}},
                "fixture_strategy":{"type":"string","maxLength":4096},"uncertainties":strings
            },"required":["defect","desired_behavior","design","compatibility_obligations","acceptance_criteria","fixture_strategy","uncertainties"],"additionalProperties":false}
        },"required":["schema_version","proposal_kind","motivating_case_ids","prompt_traces","relevant_tests","competing_interpretations","chosen_scope","changed_entry_paths","deferred_scope","proposal"],"additionalProperties":false})
    }
    async fn call(&self, args: Investigation) -> Result<Value, ToolError> {
        self.submit(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contracts::*, planner_tools::ToolLimits};
    fn fixture() -> (tempfile::TempDir, SubmitInvestigation, Investigation) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/example/src")).unwrap();
        std::fs::write(
            dir.path().join("crates/example/src/lib.rs"),
            "fn parse() {}\n#[test] fn compatibility() {}\n",
        )
        .unwrap();
        let report = EvaluationReport {
            schema_version: REPORT_SCHEMA_VERSION,
            evaluation_id: "evaluation-1".into(),
            generation_id: "generation-1".into(),
            generation_hash: String::new(),
            candidates_hash: String::new(),
            metadata_hash: String::new(),
            dataset_hash: String::new(),
            provenance: ToolProvenance::default(),
            aggregates: Aggregates::default(),
            limitations: vec![],
            notes: vec![],
            cases: vec![CaseReport {
                case_id: "generation-1-0001".into(),
                candidate_hash: String::new(),
                prompt: "Keep value".into(),
                review: ReviewStatus::Confirmed,
                review_rationale: None,
                observed: ObservedResult::Refused,
                plan_comparison: PlanComparison::Unavailable,
                diagnostic_codes: vec!["intent.refused".into()],
                row_counts: None,
                evidence: CaseEvidence::default(),
            }],
        };
        let redactor = Redactor::new(["Protected Value"]).unwrap();
        let toolbox =
            Toolbox::new(dir.path(), &report, redactor.clone(), ToolLimits::default()).unwrap();
        let citation = EvidenceCitation {
            path: "crates/example/src/lib.rs".into(),
            start_line: 1,
            end_line: 2,
        };
        let investigation = Investigation {
            schema_version: INVESTIGATION_SCHEMA_VERSION,
            proposal_kind: "implementation".into(),
            motivating_case_ids: vec!["generation-1-0001".into()],
            prompt_traces: vec![PromptTrace {
                case_id: "generation-1-0001".into(),
                entry_path: "explicit".into(),
                trace: "The explicit parser refuses this supported column.".into(),
                citations: vec![citation.clone()],
            }],
            relevant_tests: vec![citation.clone()],
            competing_interpretations: vec![
                "The token could instead be part of the literal.".into(),
            ],
            chosen_scope: "Recognize the single documented operator in the explicit path.".into(),
            changed_entry_paths: vec![EntryPath {
                name: "explicit".into(),
                rationale: "The motivating refusal enters this parser.".into(),
                citations: vec![citation.clone()],
                tests: vec![citation],
            }],
            deferred_scope: vec!["Other operators remain deferred.".into()],
            proposal: ProposalDraft {
                defect: "The documented explicit equality operator is refused.".into(),
                desired_behavior: "Accept this operator for a supported column.".into(),
                design: "Extend only the explicit equality token branch, preserving column binding.".into(),
                compatibility_obligations: vec!["Existing literal interpretation and quoted values must stay supported.".into()],
                acceptance_criteria: vec!["The motivating prompt produces the expected equality plan.".into()],
                fixture_strategy: "Use a synthetic two-row CSV and assert plan, source coordinates, and diagnostics.".into(),
                uncertainties: vec![],
            },
        };
        (
            dir,
            SubmitInvestigation::new(&report, redactor, toolbox),
            investigation,
        )
    }
    #[test]
    fn accepts_once_without_writing_and_preserves_handoff() {
        let (dir, tool, i) = fixture();
        assert!(tool.submit(i.clone()).is_ok());
        assert_eq!(tool.accepted(), Some(i.clone()));
        assert!(tool.submit(i).is_err());
        assert!(!dir.path().join("epics").exists());
    }
    #[test]
    fn rejects_invalid_evidence_then_allows_correction() {
        let (_dir, tool, i) = fixture();
        type Mutation = Box<dyn Fn(&mut Investigation)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|i| i.schema_version = 99),
            Box::new(|i| i.motivating_case_ids = vec!["unknown".into()]),
            Box::new(|i| i.prompt_traces.clear()),
            Box::new(|i| i.changed_entry_paths[0].tests.clear()),
            Box::new(|i| i.relevant_tests[0].end_line = 100),
            Box::new(|i| i.chosen_scope = "Protected Value".into()),
            Box::new(|i| i.changed_entry_paths[0].citations[0].path = ".env".into()),
        ];
        for mutate in mutations {
            let mut bad = i.clone();
            mutate(&mut bad);
            assert!(tool.submit(bad).is_err());
            assert!(tool.accepted().is_none());
        }
        assert_eq!(tool.rejections().len(), 7);
        assert!(tool.submit(i).is_ok());
    }
    #[test]
    fn exhausted_attempts_cannot_accept() {
        let (_dir, tool, mut i) = fixture();
        i.schema_version = 99;
        for _ in 0..MAX_ATTEMPTS {
            assert!(tool.submit(i.clone()).is_err());
        }
        i.schema_version = INVESTIGATION_SCHEMA_VERSION;
        assert!(
            tool.submit(i)
                .unwrap_err()
                .to_string()
                .contains("budget exhausted")
        );
    }
    #[test]
    fn additional_path_can_be_justified_by_invariant_without_invented_trace() {
        let (_dir, tool, mut i) = fixture();
        let mut second = i.changed_entry_paths[0].clone();
        second.name = "compact".into();
        second.rationale =
            "Existing compatibility tests establish equivalence with the explicit path.".into();
        i.changed_entry_paths.push(second);
        assert!(tool.submit(i).is_ok());
    }
    #[test]
    fn research_scope_does_not_require_invented_changes() {
        let (_dir, tool, mut i) = fixture();
        i.proposal_kind = "investigation".into();
        i.changed_entry_paths.clear();
        i.chosen_scope =
            "Establish intended interpretation with human review before proposing changes.".into();
        assert!(tool.submit(i).is_ok());
    }
    #[test]
    fn complete_proposal_is_required_and_old_schema_is_rejected() {
        let (_dir, tool, i) = fixture();
        let mut old = i.clone();
        old.schema_version = 1;
        assert!(tool.submit(old).is_err());
        let mut incomplete = i.clone();
        incomplete.proposal.acceptance_criteria.clear();
        assert!(tool.submit(incomplete).is_err());
        let mut unsafe_proposal = i.clone();
        unsafe_proposal.proposal.compatibility_obligations = vec!["Protected Value".into()];
        assert!(tool.submit(unsafe_proposal).is_err());
        let mut missing = serde_json::to_value(&i).unwrap();
        missing.as_object_mut().unwrap().remove("proposal");
        assert!(serde_json::from_value::<Investigation>(missing).is_err());
        let roundtrip: Investigation =
            serde_json::from_value(serde_json::to_value(&i).unwrap()).unwrap();
        assert_eq!(roundtrip, i);
        assert!(tool.submit(roundtrip).is_ok());
    }
}
