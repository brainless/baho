//! Safe, live presentation of repair events. Persisted event files remain the
//! complete record; this projection never prints prompts, source, or tool args.
use std::sync::Arc;

use serde_json::Value;

use crate::repair::Stage;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

#[derive(Clone)]
pub struct RepairProgress {
    verbosity: Verbosity,
    write: Arc<dyn Fn(&str) + Send + Sync>,
}

impl RepairProgress {
    pub fn new(verbosity: Verbosity, write: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            verbosity,
            write: Arc::new(write),
        }
    }

    pub fn message(&self, message: &str) {
        if self.verbosity != Verbosity::Quiet {
            (self.write)(message);
        }
    }

    pub fn stage(&self, stage: Stage, name: &str, detail: &str) {
        if self.verbosity == Verbosity::Quiet {
            return;
        }
        // Only these trusted, fixed event names are projected to the terminal.
        let label = match name {
            "attempt_reserved" => "attempt reserved",
            "baseline_started" => "building baseline",
            "baseline_checks_started" => "checking baseline",
            "target_probe_started" => "probing target case",
            "investigation_started" => "investigating mismatch",
            "reduction_frozen" => "synthetic reduction frozen",
            "regression_frozen" => "testing synthetic regression on baseline",
            "protected_baseline_started" => "probing protected cases on baseline",
            "coding_started" => "coding candidate",
            "checks_started" => "building and checking candidate",
            "candidate_probes_started" => "probing candidate cases",
            "suite_started" => "running fixed suite",
            "review_started" => "reviewing candidate",
            "attempt_finished" => "attempt finished",
            "cleanup_failed" => "checkout cleanup failed",
            _ => return,
        };
        let suffix = if matches!(name, "suite_started" | "attempt_finished") {
            format!(" ({detail})")
        } else {
            String::new()
        };
        (self.write)(&format!("repair [{stage:?}]: {label}{suffix}"));
    }

    pub fn agent(&self, stage: &str, name: &str, detail: &Value, model_call: u32) {
        if self.verbosity != Verbosity::Verbose {
            return;
        }
        let label = match name {
            "agent_started" => "agent started".to_owned(),
            "model_call_started" => format!("model call {model_call} started"),
            "model_call_finished" => format!("model call {model_call} finished"),
            "tool_call_started" => format!("tool {} started", safe_tool(detail)),
            "tool_call_finished" => format!("tool {} finished", safe_tool(detail)),
            "final_answer_requested" => "final answer requested".to_owned(),
            "agent_finished" => format!("agent finished ({})", safe_cause(detail)),
            _ => return,
        };
        (self.write)(&format!("repair [{stage}]: {label}"));
    }
}

fn safe_tool(detail: &Value) -> &str {
    match detail.get("tool").and_then(Value::as_str) {
        Some("outline") => "outline",
        Some("read_lines") => "read_lines",
        Some("search_text") => "search_text",
        Some("probe") => "probe",
        Some("run_checks") => "run_checks",
        _ => "unknown",
    }
}

fn safe_cause(detail: &Value) -> &str {
    match detail.get("cause").and_then(Value::as_str) {
        Some("completed") => "completed",
        Some("completed_after_budget") => "completed after budget",
        Some("insufficient_evidence") => "insufficient evidence",
        _ => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn progress_levels_only_print_safe_fields() {
        for (level, expected) in [
            (Verbosity::Quiet, 0),
            (Verbosity::Normal, 1),
            (Verbosity::Verbose, 3),
        ] {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let capture = lines.clone();
            let progress = RepairProgress::new(level, move |line| {
                capture.lock().unwrap().push(line.to_owned())
            });
            progress.stage(Stage::Baseline, "baseline_started", "private text");
            progress.agent(
                "investigation",
                "tool_call_started",
                &serde_json::json!({"tool":"search_text","args":"private text"}),
                1,
            );
            progress.agent(
                "investigation",
                "model_call_started",
                &serde_json::json!({"prompt":"private text"}),
                2,
            );
            let result = lines.lock().unwrap();
            assert_eq!(result.len(), expected);
            assert!(!result.join("\n").contains("private text"));
        }
    }
}
