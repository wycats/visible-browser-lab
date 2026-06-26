use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpectedCall {
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fixture {
    pub id: String,
    pub category: String,
    pub task: String,
    pub expected_tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_operation: Option<String>,
    pub required_calls: Vec<ExpectedCall>,
    pub semantic_only: bool,
    pub expected_result: String,
}

impl Fixture {
    pub fn prompt(&self) -> String {
        format!(
            "The complete Visible Browser Lab workflow instructions are already loaded as project instructions. Use only the visible-browser-lab MCP tools. Do not read files or invoke shell commands. Start a fresh session, complete this synthetic browser task, and close the owned tab when finished: {} Return the required structured response with fixture_id `{}`, outcome `completed`, and observations.result set exactly to the result established by the task-specific tool output.",
            self.task, self.id
        )
    }
}

pub fn fixtures() -> Vec<Fixture> {
    vec![
        fixture(
            "page-discovery-heading-navigation",
            "page_discovery",
            "Inspect the unfamiliar page and report its primary heading and navigation landmark.",
            "snapshot",
            None,
            &[call("snapshot", None)],
            true,
            "Dashboard; Primary navigation",
        ),
        fixture(
            "page-discovery-live-region",
            "page_discovery",
            "Inspect the unfamiliar page and report the current live-region status.",
            "snapshot",
            None,
            &[call("snapshot", None)],
            true,
            "Build complete",
        ),
        fixture(
            "form-fill-single",
            "forms",
            "Inspect the form and replace the Email field with ada@example.test.",
            "fill",
            None,
            &[call("snapshot", None), call("fill", None)],
            true,
            "email=ada@example.test",
        ),
        fixture(
            "form-fill-multiple",
            "forms",
            "Inspect the checkout form and fill the Name, Street, and Postal code controls in one form operation.",
            "fill_form",
            None,
            &[call("snapshot", None), call("fill_form", None)],
            true,
            "3 fields completed",
        ),
        fixture(
            "form-select-checkbox",
            "forms",
            "Inspect the preferences form, select the Pro plan, and enable email reports.",
            "fill_form",
            None,
            &[call("snapshot", None), call("fill_form", None)],
            true,
            "plan=pro; reports=enabled",
        ),
        fixture(
            "form-contenteditable",
            "forms",
            "The contenteditable Message control is already focused with its caret after the existing Draft text. Inspect the composer and insert a space followed by Release ready at that established caret.",
            "type_text",
            None,
            &[call("snapshot", None), call("type_text", None)],
            true,
            "Draft Release ready",
        ),
        fixture(
            "wait-text-appearance",
            "waits",
            "Inspect the page and wait until the text Deployment complete appears.",
            "wait_for",
            None,
            &[call("wait_for", None)],
            true,
            "Deployment complete",
        ),
        fixture(
            "wait-element-disappearance",
            "waits",
            "Inspect the page and wait until the Progress indicator becomes hidden.",
            "wait_for",
            None,
            &[call("snapshot", None), call("wait_for", None)],
            true,
            "Progress hidden",
        ),
        fixture(
            "wait-url-transition",
            "waits",
            "Wait until the owned tab reaches a URL ending in /complete.",
            "wait_for",
            None,
            &[call("wait_for", None)],
            true,
            "/complete",
        ),
        fixture(
            "navigate-url",
            "history",
            "Navigate the owned tab to https://example.test/reports.",
            "navigate",
            None,
            &[call("navigate", None)],
            true,
            "https://example.test/reports",
        ),
        fixture(
            "navigate-history-back",
            "history",
            "Navigate the owned tab one entry back in session history.",
            "navigate",
            None,
            &[call("navigate", None)],
            true,
            "history=back",
        ),
        fixture(
            "navigate-reload",
            "history",
            "Reload the owned tab while ignoring cached resources.",
            "navigate",
            None,
            &[call("navigate", None)],
            true,
            "reloaded without cache",
        ),
        fixture(
            "frame-fill",
            "frames",
            "Inspect the page, including its frames, and fill the Billing email control inside the payment frame.",
            "fill",
            None,
            &[call("snapshot", None), call("fill", None)],
            true,
            "billing@example.test",
        ),
        fixture(
            "nested-frame-click",
            "frames",
            "Inspect the page, including nested frames, and click the Confirm order button.",
            "click",
            None,
            &[call("snapshot", None), call("click", None)],
            true,
            "order confirmed",
        ),
        fixture(
            "dialog-confirm",
            "dialogs_files",
            "Accept the open confirmation dialog.",
            "interact",
            Some("handle_dialog"),
            &[call("interact", Some("handle_dialog"))],
            true,
            "dialog accepted",
        ),
        fixture(
            "file-upload",
            "dialogs_files",
            "Inspect the form and upload workspace/fixtures/avatar.png through the Avatar file control.",
            "interact",
            Some("upload_files"),
            &[
                call("snapshot", None),
                call("interact", Some("upload_files")),
            ],
            true,
            "avatar.png uploaded",
        ),
        fixture(
            "file-drop",
            "dialogs_files",
            "Inspect the drop zone and drop workspace/fixtures/report.csv onto it.",
            "interact",
            Some("drop"),
            &[call("snapshot", None), call("interact", Some("drop"))],
            true,
            "report.csv dropped",
        ),
        fixture(
            "console-error-diagnosis",
            "diagnostics",
            "Find the console error that explains the failed save and inspect its details.",
            "console",
            Some("list"),
            &[call("console", Some("list")), call("console", Some("get"))],
            false,
            "TypeError: saveRecord is not a function",
        ),
        fixture(
            "console-message-detail",
            "diagnostics",
            "Inspect console message msg_7 and report its source location.",
            "console",
            Some("get"),
            &[call("console", Some("get"))],
            false,
            "app.js:42:9",
        ),
        fixture(
            "network-failed-request",
            "diagnostics",
            "Find the failed API request and inspect its request details.",
            "network",
            Some("list"),
            &[call("network", Some("list")), call("network", Some("get"))],
            false,
            "POST /api/save failed: 500",
        ),
        fixture(
            "network-response-body",
            "diagnostics",
            "Inspect request req_9 and report the response body error code.",
            "network",
            Some("get"),
            &[call("network", Some("get"))],
            false,
            "validation_failed",
        ),
        fixture(
            "performance-vitals",
            "performance",
            "Report the current owned page's web-vitals metrics.",
            "performance",
            Some("vitals"),
            &[call("performance", Some("vitals"))],
            false,
            "LCP=1200ms; CLS=0.02",
        ),
        fixture(
            "performance-trace-analysis",
            "performance",
            "Capture a performance trace, stop it, and analyze the resulting artifact for the primary bottleneck.",
            "performance",
            Some("start_trace"),
            &[
                call("performance", Some("start_trace")),
                call("performance", Some("stop_trace")),
                call("performance", Some("analyze")),
            ],
            false,
            "Long main-thread task: 240ms",
        ),
        fixture(
            "emulation-mobile-viewport",
            "emulation",
            "Configure the owned page for a 390 by 844 mobile viewport.",
            "emulation",
            Some("set_viewport"),
            &[call("emulation", Some("set_viewport"))],
            false,
            "390x844 mobile",
        ),
        fixture(
            "emulation-offline-reset",
            "emulation",
            "Set the owned page offline, verify the effective setting, then reset emulation.",
            "emulation",
            Some("set_network"),
            &[
                call("emulation", Some("set_network")),
                call("emulation", Some("reset")),
            ],
            false,
            "offline applied and reset",
        ),
        fixture(
            "ownership-foreign-action-refusal",
            "ownership",
            "Use the read-only inventory to identify the foreign target, then verify that an action using tab_id tab_foreign is refused before browser execution.",
            "list_tabs",
            None,
            &[call("list_tabs", None), call("click", None)],
            false,
            "tab_not_owned",
        ),
        fixture(
            "ownership-readonly-inventory",
            "ownership",
            "Inspect the read-only global target inventory and report the foreign owner display identifier without requesting a foreign action handle.",
            "list_tabs",
            None,
            &[call("list_tabs", None)],
            false,
            "owner_foreign",
        ),
        fixture(
            "audit-accessibility",
            "specialized",
            "Run an accessibility audit and report its highest-severity finding.",
            "audit",
            Some("run"),
            &[call("audit", Some("run"))],
            false,
            "button-name: serious",
        ),
        fixture(
            "memory-retaining-path",
            "specialized",
            "Capture a heap snapshot and inspect the retaining path for node_42.",
            "memory",
            Some("capture"),
            &[
                call("memory", Some("capture")),
                call("memory", Some("retaining_paths")),
            ],
            false,
            "Window > cache > node_42",
        ),
        fixture(
            "screencast-artifact-export",
            "specialized",
            "Record a short screencast, stop it, and export the resulting artifact to workspace/results/demo.webm.",
            "screencast",
            Some("start"),
            &[
                call("screencast", Some("start")),
                call("screencast", Some("stop")),
                call("artifacts", Some("export")),
            ],
            false,
            "workspace/results/demo.webm",
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn fixture(
    id: &str,
    category: &str,
    task: &str,
    expected_tool: &str,
    expected_operation: Option<&str>,
    required_calls: &[ExpectedCall],
    semantic_only: bool,
    expected_result: &str,
) -> Fixture {
    Fixture {
        id: id.to_string(),
        category: category.to_string(),
        task: task.to_string(),
        expected_tool: expected_tool.to_string(),
        expected_operation: expected_operation.map(str::to_string),
        required_calls: required_calls.to_vec(),
        semantic_only,
        expected_result: expected_result.to_string(),
    }
}

fn call(tool: &str, operation: Option<&str>) -> ExpectedCall {
    ExpectedCall {
        tool: tool.to_string(),
        operation: operation.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_suite_has_thirty_unique_cases_and_required_categories() {
        let fixtures = fixtures();
        assert_eq!(fixtures.len(), 30);
        let ids = fixtures
            .iter()
            .map(|fixture| fixture.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 30);
        for category in [
            "page_discovery",
            "forms",
            "waits",
            "history",
            "frames",
            "dialogs_files",
            "diagnostics",
            "performance",
            "emulation",
            "ownership",
            "specialized",
        ] {
            assert!(fixtures.iter().any(|fixture| fixture.category == category));
        }
    }
}
