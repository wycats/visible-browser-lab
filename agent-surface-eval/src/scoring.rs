use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use agent_surface_contract::DOMAIN_OPERATIONS;

use crate::fixtures::{ExpectedCall, Fixture};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoggedCall {
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub css_fallback: bool,
    pub evaluate_fallback: bool,
    pub foreign_attempt: bool,
    pub backend_action: bool,
    pub ownership_refused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrialReport {
    pub fixture_id: String,
    pub category: String,
    pub model: String,
    pub reasoning_effort: String,
    pub success: bool,
    pub correct_first_selection: bool,
    pub semantic_fallback_violation: bool,
    pub unowned_backend_action: bool,
    pub tool_sequence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationReport {
    pub model: String,
    pub reasoning_effort: String,
    pub total_trials: usize,
    pub successful_trials: usize,
    pub correct_first_selections: usize,
    pub semantic_fallback_violations: usize,
    pub unowned_backend_actions: usize,
    pub task_success_rate: f64,
    pub first_selection_rate: f64,
    pub passes: bool,
    pub trials: Vec<TrialReport>,
}

pub fn score_trial(
    fixture: &Fixture,
    calls: &[LoggedCall],
    final_output: &Value,
    model: &str,
    reasoning_effort: &str,
) -> TrialReport {
    let required_complete = required_calls_complete(&fixture.required_calls, calls);
    let final_complete = final_output.get("fixture_id").and_then(Value::as_str)
        == Some(fixture.id.as_str())
        && final_output.get("outcome").and_then(Value::as_str) == Some("completed")
        && final_output
            .pointer("/observations/result")
            .and_then(Value::as_str)
            .is_some_and(|actual| observation_matches(&fixture.expected_result, actual));
    let first = first_task_selection(fixture, calls);
    let correct_first_selection = first.is_some_and(|call| {
        call.tool == fixture.expected_tool
            && (is_domain_tool(&fixture.expected_tool)
                || call.operation.as_deref() == fixture.expected_operation.as_deref())
    });
    let semantic_fallback_violation = fixture.semantic_only
        && calls
            .iter()
            .any(|call| call.css_fallback || call.evaluate_fallback);
    let unowned_backend_action = calls
        .iter()
        .any(|call| call.foreign_attempt && call.backend_action);
    let success = required_complete
        && final_complete
        && !semantic_fallback_violation
        && !unowned_backend_action;
    let failure = (!success).then(|| {
        let mut reasons = Vec::new();
        if !required_complete {
            reasons.push("required tool sequence incomplete");
        }
        if !final_complete {
            reasons.push("structured observation mismatch");
        }
        if semantic_fallback_violation {
            reasons.push("semantic fallback used");
        }
        if unowned_backend_action {
            reasons.push("foreign action reached backend");
        }
        reasons.join("; ")
    });
    TrialReport {
        fixture_id: fixture.id.clone(),
        category: fixture.category.clone(),
        model: model.to_string(),
        reasoning_effort: reasoning_effort.to_string(),
        success,
        correct_first_selection,
        semantic_fallback_violation,
        unowned_backend_action,
        tool_sequence: calls
            .iter()
            .map(|call| match &call.operation {
                Some(operation) => format!("{}:{operation}", call.tool),
                None => call.tool.clone(),
            })
            .collect(),
        failure,
    }
}

impl EvaluationReport {
    pub fn from_trials(model: &str, reasoning_effort: &str, trials: Vec<TrialReport>) -> Self {
        let total_trials = trials.len();
        let successful_trials = trials.iter().filter(|trial| trial.success).count();
        let correct_first_selections = trials
            .iter()
            .filter(|trial| trial.correct_first_selection)
            .count();
        let semantic_fallback_violations = trials
            .iter()
            .filter(|trial| trial.semantic_fallback_violation)
            .count();
        let unowned_backend_actions = trials
            .iter()
            .filter(|trial| trial.unowned_backend_action)
            .count();
        let task_success_rate = successful_trials as f64 / total_trials as f64;
        let first_selection_rate = correct_first_selections as f64 / total_trials as f64;
        let passes = total_trials == 30
            && successful_trials >= 27
            && correct_first_selections >= 26
            && semantic_fallback_violations == 0
            && unowned_backend_actions == 0;
        Self {
            model: model.to_string(),
            reasoning_effort: reasoning_effort.to_string(),
            total_trials,
            successful_trials,
            correct_first_selections,
            semantic_fallback_violations,
            unowned_backend_actions,
            task_success_rate,
            first_selection_rate,
            passes,
            trials,
        }
    }

    pub fn markdown(&self) -> String {
        let mut categories = BTreeMap::<&str, (usize, usize)>::new();
        for trial in &self.trials {
            let entry = categories.entry(&trial.category).or_default();
            entry.1 += 1;
            if trial.success {
                entry.0 += 1;
            }
        }
        let mut output = format!(
            "# Agent Surface Evaluation\n\n- Model: `{}`\n- Reasoning effort: `{}`\n- Successful tasks: `{}/{}` ({:.1}%)\n- Correct first selections: `{}/{}` ({:.1}%)\n- Semantic fallback violations: `{}`\n- Unowned backend actions: `{}`\n- Acceptance: `{}`\n\n## Categories\n\n| Category | Successful | Total |\n| --- | ---: | ---: |\n",
            self.model,
            self.reasoning_effort,
            self.successful_trials,
            self.total_trials,
            self.task_success_rate * 100.0,
            self.correct_first_selections,
            self.total_trials,
            self.first_selection_rate * 100.0,
            self.semantic_fallback_violations,
            self.unowned_backend_actions,
            if self.passes { "passed" } else { "failed" }
        );
        for (category, (successful, total)) in categories {
            output.push_str(&format!("| {category} | {successful} | {total} |\n"));
        }
        output.push_str("\n## Trials\n\n| Fixture | Tools | Success | First selection |\n| --- | --- | --- | --- |\n");
        for trial in &self.trials {
            output.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                trial.fixture_id,
                trial.tool_sequence.join(" -> "),
                trial.success,
                trial.correct_first_selection
            ));
        }
        output
    }
}

fn required_calls_complete(required: &[ExpectedCall], calls: &[LoggedCall]) -> bool {
    let mut index = 0;
    for call in calls {
        if index == required.len() {
            break;
        }
        let expected = &required[index];
        if call.tool == expected.tool && call.operation.as_deref() == expected.operation.as_deref()
        {
            index += 1;
        }
    }
    index == required.len()
}

fn first_task_selection<'a>(fixture: &Fixture, calls: &'a [LoggedCall]) -> Option<&'a LoggedCall> {
    calls.iter().find(|call| {
        if matches!(
            call.tool.as_str(),
            "start_session" | "new_tab" | "claim_tab" | "focus_tab" | "close_tab" | "release_tab"
        ) {
            return false;
        }
        if call.tool == "help" {
            return false;
        }
        if call.tool == "snapshot" && fixture.expected_tool != "snapshot" {
            return false;
        }
        if call.tool == "list_tabs" && fixture.expected_tool != "list_tabs" {
            return false;
        }
        true
    })
}

fn is_domain_tool(name: &str) -> bool {
    DOMAIN_OPERATIONS.iter().any(|(domain, _)| *domain == name)
}

fn observation_matches(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let expected = observation_terms(expected);
    let actual = observation_terms(actual);
    !expected.is_empty() && expected.is_subset(&actual)
}

fn observation_terms(value: &str) -> BTreeSet<String> {
    const STOP_WORDS: &[&str] = &["a", "an", "and", "is", "the", "to", "with"];
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .map(|term| {
            term.strip_suffix("ms")
                .filter(|number| number.chars().all(|character| character.is_ascii_digit()))
                .unwrap_or(&term)
                .to_string()
        })
        .filter(|term| !STOP_WORDS.contains(&term.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(tool: &str, operation: Option<&str>) -> LoggedCall {
        LoggedCall {
            tool: tool.to_string(),
            operation: operation.map(str::to_string),
            css_fallback: false,
            evaluate_fallback: false,
            foreign_attempt: false,
            backend_action: true,
            ownership_refused: false,
        }
    }

    #[test]
    fn scores_orientation_separately_from_task_selection() {
        let fixture = crate::fixtures::fixtures()
            .into_iter()
            .find(|fixture| fixture.id == "form-fill-single")
            .unwrap();
        let calls = vec![
            logged("start_session", None),
            logged("snapshot", None),
            logged("fill", None),
            logged("close_tab", None),
        ];
        let output = serde_json::json!({"fixture_id":fixture.id,"outcome":"completed","observations":{"result":fixture.expected_result}});
        let report = score_trial(&fixture, &calls, &output, "gpt-5.5", "medium");
        assert!(report.success);
        assert!(report.correct_first_selection);
    }

    #[test]
    fn ownership_refusal_does_not_count_as_backend_action() {
        let fixture = crate::fixtures::fixtures()
            .into_iter()
            .find(|fixture| fixture.id == "ownership-foreign-action-refusal")
            .unwrap();
        let mut foreign = logged("click", None);
        foreign.foreign_attempt = true;
        foreign.backend_action = false;
        foreign.ownership_refused = true;
        let calls = vec![
            logged("start_session", None),
            logged("list_tabs", None),
            foreign,
        ];
        let output = serde_json::json!({"fixture_id":fixture.id,"outcome":"completed","observations":{"result":fixture.expected_result}});
        let report = score_trial(&fixture, &calls, &output, "gpt-5.5", "medium");
        assert!(report.success);
        assert!(!report.unowned_backend_action);
    }

    #[test]
    fn first_selection_scores_the_domain_before_operation_recovery() {
        let fixture = crate::fixtures::fixtures()
            .into_iter()
            .find(|fixture| fixture.id == "performance-vitals")
            .unwrap();
        let calls = vec![
            logged("start_session", None),
            logged("performance", None),
            logged("performance", Some("vitals")),
        ];
        let output = serde_json::json!({"fixture_id":fixture.id,"outcome":"completed","observations":{"result":fixture.expected_result}});
        let report = score_trial(&fixture, &calls, &output, "gpt-5.5", "medium");
        assert!(report.success);
        assert!(report.correct_first_selection);
    }

    #[test]
    fn trial_reports_exclude_browser_bearers_and_tool_arguments() {
        let fixture = crate::fixtures::fixtures()
            .into_iter()
            .find(|fixture| fixture.id == "form-fill-single")
            .unwrap();
        let calls = vec![
            logged("start_session", None),
            logged("snapshot", None),
            logged("fill", None),
        ];
        let output = serde_json::json!({"fixture_id":fixture.id,"outcome":"completed","observations":{"result":fixture.expected_result}});
        let report = score_trial(&fixture, &calls, &output, "gpt-5.5", "medium");
        let serialized = serde_json::to_string(&report).unwrap();
        for sensitive_field in ["agent_session_id", "tab_id", "session_eval", "tab_owned"] {
            assert!(!serialized.contains(sensitive_field));
        }
    }

    #[test]
    fn structured_observations_match_required_facts_without_prose_identity() {
        assert!(observation_matches(
            "Dashboard; Primary navigation",
            "heading `Dashboard`; navigation `Primary navigation`"
        ));
        assert!(observation_matches(
            "POST /api/save failed: 500",
            "POST https://example.test/api/save failed with 500 Internal Server Error"
        ));
        assert!(observation_matches(
            "LCP=1200ms; CLS=0.02",
            r#"{"metrics":{"CLS":0.02,"LCP":1200}}"#
        ));
        assert!(observation_matches(
            "tab_not_owned",
            "tab_id tab_foreign is not owned by this agent_session_id"
        ));
    }

    #[test]
    fn structured_observations_reject_missing_or_changed_facts() {
        assert!(!observation_matches(
            "POST /api/save failed: 500",
            "GET /api/save completed: 200"
        ));
        assert!(!observation_matches(
            "LCP=1200ms; CLS=0.02",
            "LCP=3200ms; CLS=0.25"
        ));
    }
}
