//! Non-publishing draft acceptance and independent semantic review.
use crate::{
    planner_tools::{ToolError, Toolbox},
    redaction::Redactor,
    submit_epic::{EpicSubmitter, MAX_SUBMISSIONS, ProposalEvidence, SubmitEpic},
};
use rig::tool::{PortableTool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftProposal {
    pub slug: String,
    pub markdown: String,
    pub evidence: ProposalEvidence,
}
pub fn draft_hash(draft: &DraftProposal) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(draft).expect("serializable draft"))
    )
}
#[derive(Default)]
struct DraftState {
    accepted: Option<DraftProposal>,
    attempts: usize,
    rejections: Vec<String>,
}
#[derive(Clone)]
pub struct SubmitDraft {
    submitter: EpicSubmitter,
    expected: Option<ProposalEvidence>,
    state: Arc<Mutex<DraftState>>,
}
fn record_rejection(rejections: &mut Vec<String>, error: ToolError) -> ToolError {
    if rejections.len() < MAX_SUBMISSIONS {
        rejections.push(error.0.chars().take(400).collect());
    }
    error
}
impl SubmitDraft {
    pub fn new(submitter: EpicSubmitter) -> Self {
        Self {
            submitter,
            expected: None,
            state: Arc::new(Mutex::new(DraftState::default())),
        }
    }
    pub fn with_evidence(submitter: EpicSubmitter, expected: ProposalEvidence) -> Self {
        Self {
            expected: Some(expected),
            ..Self::new(submitter)
        }
    }
    pub fn accepted(&self) -> Option<DraftProposal> {
        self.state.lock().unwrap().accepted.clone()
    }
    pub fn rejections(&self) -> Vec<String> {
        self.state.lock().unwrap().rejections.clone()
    }
}
impl PortableTool for SubmitDraft {
    const NAME: &'static str = "submit_draft";
    type Args = DraftProposal;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, e: ToolError) -> ToolExecutionError {
        ToolExecutionError::other(e.to_string()).with_code("harness_rejection")
    }
    fn description(&self) -> String {
        "Validate and accept an epic draft for independent review. This does not publish a file. Stop after acceptance.".into()
    }
    fn parameters(&self) -> Value {
        SubmitEpic(self.submitter.clone()).parameters()
    }
    async fn call(&self, draft: DraftProposal) -> Result<Value, ToolError> {
        let mut state = self.state.lock().unwrap();
        if state.accepted.is_some() {
            return Err(record_rejection(
                &mut state.rejections,
                ToolError("a draft was already accepted".into()),
            ));
        }
        if state.attempts >= MAX_SUBMISSIONS {
            return Err(record_rejection(
                &mut state.rejections,
                ToolError("draft attempt budget exhausted".into()),
            ));
        }
        state.attempts += 1;
        if let Some(expected) = &self.expected
            && !within_investigated_evidence(&draft.evidence, expected)
        {
            return Err(record_rejection(&mut state.rejections, ToolError("draft must retain investigation proposal kind and schema version 1, with nonempty unique motivating cases drawn only from the investigation".into())));
        }
        if let Err(error) =
            self.submitter
                .validate_proposal(&draft.slug, &draft.markdown, &draft.evidence)
        {
            return Err(record_rejection(
                &mut state.rejections,
                ToolError(error.to_string()),
            ));
        }
        let hash = draft_hash(&draft);
        state.accepted = Some(draft);
        Ok(json!({"accepted":true,"draft_hash":hash,"published":false}))
    }
}
/// Writers may choose a smaller independently useful increment, but cannot add evidence.
fn within_investigated_evidence(actual: &ProposalEvidence, expected: &ProposalEvidence) -> bool {
    let ids: std::collections::BTreeSet<_> = actual.motivating_case_ids.iter().collect();
    expected.schema_version == 1
        && actual.schema_version == 1
        && actual.kind == expected.kind
        && !ids.is_empty()
        && ids.len() == actual.motivating_case_ids.len()
        && ids
            .iter()
            .all(|id| expected.motivating_case_ids.contains(id))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Pass,
    Revise,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCitation {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub issue: String,
    pub evidence: Vec<ReviewCitation>,
    pub requested_correction: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub schema_version: u32,
    pub draft_hash: String,
    pub verdict: ReviewVerdict,
    pub findings: Vec<ReviewFinding>,
    pub summary: String,
}
#[derive(Default)]
struct ReviewState {
    accepted: Option<Review>,
    attempts: usize,
    rejections: Vec<String>,
}
#[derive(Clone)]
pub struct SubmitReview {
    hash: String,
    redactor: Redactor,
    toolbox: Toolbox,
    state: Arc<Mutex<ReviewState>>,
}
impl SubmitReview {
    pub fn new(draft: &DraftProposal, redactor: Redactor, toolbox: Toolbox) -> Self {
        Self {
            hash: draft_hash(draft),
            redactor,
            toolbox,
            state: Arc::new(Mutex::new(ReviewState::default())),
        }
    }
    pub fn accepted(&self) -> Option<Review> {
        self.state.lock().unwrap().accepted.clone()
    }
    pub fn rejections(&self) -> Vec<String> {
        self.state.lock().unwrap().rejections.clone()
    }
    fn validate_shape(&self, review: &Review) -> Result<(), ToolError> {
        let fail = |s: &str| Err(ToolError(s.into()));
        if review.schema_version != 1 || review.draft_hash != self.hash {
            return fail("review version or exact draft hash mismatch");
        }
        if (review.verdict == ReviewVerdict::Pass) != review.findings.is_empty()
            || review.findings.len() > 12
        {
            return fail("pass requires no findings; revise requires 1..12 blocking findings");
        }
        let safe = |s: &str| {
            !s.trim().is_empty() && s.len() <= 2000 && self.redactor.scan_prose(s).is_empty()
        };
        if !safe(&review.summary) {
            return fail("review summary must be bounded, nonempty, and safe");
        }
        for f in &review.findings {
            if !safe(&f.issue)
                || !safe(&f.requested_correction)
                || f.evidence.is_empty()
                || f.evidence.len() > 6
            {
                return fail(
                    "each finding needs bounded safe issue, correction, and 1..6 citations",
                );
            }
            for c in &f.evidence {
                if c.path.len() > 256
                    || !self.redactor.scan_prose(&c.path).is_empty()
                    || c.start_line == 0
                    || c.end_line < c.start_line
                    || c.end_line - c.start_line >= 100
                {
                    return fail("invalid review citation");
                }
            }
        }
        Ok(())
    }
}
impl PortableTool for SubmitReview {
    const NAME: &'static str = "submit_review";
    type Args = Review;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, e: ToolError) -> ToolExecutionError {
        ToolExecutionError::other(e.to_string()).with_code("harness_rejection")
    }
    fn description(&self) -> String {
        "Submit a semantic review bound to the exact draft hash. Pass has zero findings; revise has concrete blocking issues with verified repository citations and corrections. Stop after acceptance.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
        "schema_version":{"type":"integer","enum":[1]},"draft_hash":{"type":"string"},"verdict":{"type":"string","enum":["pass","revise"]},"summary":{"type":"string","maxLength":2000},
        "findings":{"type":"array","maxItems":12,"items":{"type":"object","properties":{"issue":{"type":"string","maxLength":2000},"requested_correction":{"type":"string","maxLength":2000},"evidence":{"type":"array","minItems":1,"maxItems":6,"items":{"type":"object","properties":{"path":{"type":"string","maxLength":256},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path","start_line","end_line"],"additionalProperties":false}}},"required":["issue","evidence","requested_correction"],"additionalProperties":false}}
    },"required":["schema_version","draft_hash","verdict","findings","summary"],"additionalProperties":false})
    }
    async fn call(&self, review: Review) -> Result<Value, ToolError> {
        // Serialize attempts including citation validation; acceptance is immutable.
        let mut state = self.state.lock().unwrap();
        if state.accepted.is_some() {
            return Err(record_rejection(
                &mut state.rejections,
                ToolError("a review was already accepted".into()),
            ));
        }
        if state.attempts >= MAX_SUBMISSIONS {
            return Err(record_rejection(
                &mut state.rejections,
                ToolError("review attempt budget exhausted".into()),
            ));
        }
        state.attempts += 1;
        if let Err(error) = self.validate_shape(&review) {
            return Err(record_rejection(&mut state.rejections, error));
        }
        for f in &review.findings {
            for c in &f.evidence {
                if let Err(error) =
                    self.toolbox
                        .validate_citation(&c.path, c.start_line, c.end_line)
                {
                    return Err(record_rejection(&mut state.rejections, error));
                }
            }
        }
        state.accepted = Some(review.clone());
        Ok(
            json!({"accepted":true,"draft_hash":self.hash,"verdict":review.verdict,"published":false}),
        )
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use crate::submit_epic::ProposalKind;
    fn evidence(ids: &[&str]) -> ProposalEvidence {
        ProposalEvidence {
            schema_version: 1,
            kind: ProposalKind::Implementation,
            motivating_case_ids: ids.iter().map(|id| (*id).into()).collect(),
        }
    }
    #[test]
    fn writer_can_narrow_or_reorder_investigated_cases() {
        let expected = evidence(&["case-a", "case-b", "case-c"]);
        assert!(within_investigated_evidence(
            &evidence(&["case-b"]),
            &expected
        ));
        assert!(within_investigated_evidence(
            &evidence(&["case-c", "case-a", "case-b"]),
            &expected
        ));
    }
    #[test]
    fn writer_cannot_expand_erase_duplicate_or_change_evidence_kind() {
        let expected = evidence(&["case-a", "case-b"]);
        for ids in [
            &["case-a", "case-new"][..],
            &[][..],
            &["case-a", "case-a"][..],
        ] {
            assert!(!within_investigated_evidence(&evidence(ids), &expected));
        }
        let mut changed = evidence(&["case-a"]);
        changed.kind = ProposalKind::Investigation;
        assert!(!within_investigated_evidence(&changed, &expected));
        changed.kind = ProposalKind::Implementation;
        changed.schema_version = 2;
        assert!(!within_investigated_evidence(&changed, &expected));
    }
}
