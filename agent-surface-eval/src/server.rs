use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rmcp::{
    RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

use agent_surface_contract::{DOMAIN_OPERATIONS, SERVER_INSTRUCTIONS, hybrid_catalog};

use crate::{
    fixtures::{Fixture, fixtures},
    scoring::LoggedCall,
};

#[derive(Clone)]
pub struct EvaluationServer {
    fixture: Arc<Fixture>,
    log_path: Arc<PathBuf>,
    tools: Arc<Vec<Tool>>,
    calls: Arc<Mutex<Vec<LoggedCall>>>,
    log_lock: Arc<Mutex<()>>,
}

impl EvaluationServer {
    pub fn new(fixture_id: &str, log_path: PathBuf) -> Result<Self> {
        let fixture = fixtures()
            .into_iter()
            .find(|fixture| fixture.id == fixture_id)
            .with_context(|| format!("unknown evaluation fixture `{fixture_id}`"))?;
        let tools = hybrid_catalog()
            .into_iter()
            .map(|definition| {
                serde_json::from_value(serde_json::to_value(definition)?)
                    .context("catalog tool is not a valid MCP tool")
            })
            .collect::<Result<Vec<Tool>>>()?;
        Ok(Self {
            fixture: Arc::new(fixture),
            log_path: Arc::new(log_path),
            tools: Arc::new(tools),
            calls: Arc::new(Mutex::new(Vec::new())),
            log_lock: Arc::new(Mutex::new(())),
        })
    }

    fn append_call(&self, call: &LoggedCall) -> Result<()> {
        let _guard = self.log_lock.lock().expect("evaluation log lock poisoned");
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut record = serde_json::to_vec(call)?;
        record.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path.as_ref())?;
        file.write_all(&record)?;
        Ok(())
    }

    fn execute(&self, name: &str, arguments: Map<String, Value>) -> CallToolResult {
        let operation = arguments
            .get("operation")
            .and_then(Value::as_str)
            .map(str::to_string);
        let foreign_attempt =
            arguments.get("tab_id").and_then(Value::as_str) == Some("tab_foreign");
        let ownership_checked = requires_owned_tab(name);
        let ownership_refused = foreign_attempt && ownership_checked;
        let allowed_operations = DOMAIN_OPERATIONS
            .iter()
            .find_map(|(domain, operations)| (*domain == name).then_some(*operations));
        let operation_valid = allowed_operations.is_none_or(|allowed| {
            arguments
                .get("operation")
                .and_then(Value::as_str)
                .is_some_and(|operation| allowed.contains(&operation))
        });
        let call = LoggedCall {
            tool: name.to_string(),
            operation,
            css_fallback: contains_css(&Value::Object(arguments.clone())),
            evaluate_fallback: name == "evaluate",
            foreign_attempt,
            backend_action: ownership_checked && !ownership_refused && operation_valid,
            ownership_refused,
        };
        if let Err(error) = self.append_call(&call) {
            return CallToolResult::structured_error(
                json!({"code":"evaluation_log_error","message":error.to_string()}),
            );
        }
        let include_result = {
            let mut calls = self.calls.lock().expect("evaluation call state poisoned");
            calls.push(call);
            required_calls_complete(&self.fixture, &calls)
        };
        if let Some(allowed) = allowed_operations
            && !operation_valid
        {
            return CallToolResult::structured_error(json!({
                "code":"unsupported_operation",
                "message":format!("`{name}` requires one of: {}", allowed.join(", ")),
                "recovery":"help"
            }));
        }
        if ownership_refused {
            let error = json!({
                "code": "tab_not_owned",
                "message": "tab_id tab_foreign is not owned by this agent_session_id",
                "recovery": "Use a tab_id returned to this session."
            });
            return CallToolResult::structured_error(error);
        }
        if name != "start_session" && name != "help" {
            let session = arguments.get("agent_session_id").and_then(Value::as_str);
            if session.is_some_and(|session| session != "session_eval") {
                return CallToolResult::structured_error(
                    json!({"code":"unknown_session","message":"Use the agent_session_id returned by start_session."}),
                );
            }
        }
        CallToolResult::structured(tool_response(
            name,
            &arguments,
            &self.fixture,
            include_result,
        ))
    }
}

impl ServerHandler for EvaluationServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.as_ref().clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if !self.tools.iter().any(|tool| tool.name == request.name) {
            return Ok(CallToolResult::structured_error(
                json!({"code":"unsupported_operation","message":format!("unknown tool `{}`", request.name),"recovery":"help"}),
            ));
        }
        Ok(self.execute(request.name.as_ref(), request.arguments.unwrap_or_default()))
    }
}

fn tool_response(
    name: &str,
    arguments: &Map<String, Value>,
    fixture: &Fixture,
    include_result: bool,
) -> Value {
    match name {
        "start_session" => json!({
            "agent_session_id": "session_eval",
            "mode": "explicit",
            "tab": tab_summary()
        }),
        "list_tabs"
            if arguments.get("scope").and_then(Value::as_str) == Some("global_readonly")
                || fixture.category == "ownership" =>
        {
            json!({
                "scope": "global_readonly",
                "groups": [
                    {"owner_display_id":"owner_eval","tabs":[global_tab(true)]},
                    {"owner_display_id":"owner_foreign","tabs":[global_tab(false)]}
                ]
            })
        }
        "list_tabs" => json!({"scope":"owned","tabs":[tab_summary()]}),
        "snapshot" => {
            let (tree, nodes) = fixture_snapshot(fixture);
            json!({
                "snapshot_id":"snapshot_eval_1",
                "document_revision":"doc_eval_1",
                "url":"https://example.test/fixture",
                "title":"Evaluation fixture",
                "tree":tree,
                "node_count":nodes.as_array().map_or(0, Vec::len),
                "truncated":false
            })
        }
        "help" => json!({
            "topic":arguments.get("topic").cloned().unwrap_or(json!("workflow")),
            "task":"Select the shortest named browser operation.",
            "preferred":{"tool":fixture.expected_tool,"operation":fixture.expected_operation,"reason":"This operation directly expresses the fixture task."},
            "neighbors":[],
            "example":{"tool":fixture.expected_tool,"arguments":{}},
            "result_schema":{"type":"object"},
            "errors":[]
        }),
        "new_tab" | "claim_tab" | "focus_tab" => json!({"tab":tab_summary()}),
        "navigate" => page_action_response(fixture, include_result),
        "release_tab" => json!({"released":true,"leave_visible":false}),
        "close_tab" => json!({"closed":true}),
        "wait_for" => {
            let mut response = page_action_response(fixture, include_result);
            response["matched"] = json!(true);
            response["elapsed_ms"] = json!(25);
            response
        }
        "fill_form" => {
            let mut response = page_action_response(fixture, include_result);
            response["completed_fields"] = json!(3);
            response["total_fields"] = json!(3);
            response
        }
        "click" | "fill" | "type_text" | "press_key" => {
            page_action_response(fixture, include_result)
        }
        "interact" => with_operation(page_action_response(fixture, include_result), arguments),
        "console" => console_response(arguments),
        "network" => network_response(arguments),
        "performance" => performance_response(arguments),
        "emulation" => {
            let effective = if include_result {
                json!({"summary":fixture.expected_result})
            } else {
                Value::Object(arguments.clone())
            };
            json!({"operation":arguments.get("operation"),"effective":effective})
        }
        "audit" => {
            json!({"operation":"run","scores":{"accessibility":0.82},"findings":[{"id":"button-name","category":"accessibility","title":"button-name: serious","description":"A button requires an accessible name."}],"reports":[]})
        }
        "memory" => match arguments.get("operation").and_then(Value::as_str) {
            Some("capture") => {
                json!({"operation":"capture","artifact":artifact("artifact_heap", "heap_snapshot", "application/x-chrome-heap-snapshot")})
            }
            Some("close") => json!({"operation":"close","closed":true}),
            operation => {
                json!({"operation":operation,"artifact":artifact("artifact_heap", "heap_snapshot", "application/x-chrome-heap-snapshot"),"data":{"path":["Window","cache","node_42"]},"truncated":false})
            }
        },
        "screencast" => match arguments.get("operation").and_then(Value::as_str) {
            Some("start") => {
                json!({"operation":"start","recording":true,"started_at_ms":1_782_449_108_000_u64})
            }
            Some("stop") => {
                json!({"operation":"stop","recording":false,"artifact":artifact("artifact_video", "screencast", "video/webm")})
            }
            _ => json!({"operation":"status","recording":false}),
        },
        "artifacts" => artifacts_response(arguments),
        "screenshot" => {
            json!({"artifact":artifact("artifact_image", "screenshot", "image/png"),"image":{"media_type":"image/png"},"width":1280,"height":720})
        }
        "evaluate" => json!({"value":fixture.expected_result}),
        _ => json!({"operation":arguments.get("operation")}),
    }
}

fn tab_summary() -> Value {
    json!({"tab_id":"tab_owned","target_id":"target_owned","title":"Evaluation fixture","url":"https://example.test/fixture","state":"active","focused":false,"created_at_ms":1_782_449_108_000_u64,"updated_at_ms":1_782_449_108_000_u64})
}

fn global_tab(owned: bool) -> Value {
    if owned {
        json!({"target_id":"target_owned","title":"Evaluation fixture","url":"https://example.test/fixture","owner_display_id":"owner_eval","owned_by_caller":true,"caller_tab_id":"tab_owned","claimable":false,"focused":false})
    } else {
        json!({"target_id":"target_foreign","title":"Foreign work","url":"https://foreign.example.test","owner_display_id":"owner_foreign","owned_by_caller":false,"claimable":false,"focused":false})
    }
}

fn page_action_response(fixture: &Fixture, terminal: bool) -> Value {
    let changes = if terminal {
        fixture.expected_result.as_str()
    } else {
        "Page state updated."
    };
    json!({
        "document_revision":"doc_eval_2",
        "observation":{
            "mode":"diff",
            "diff":{
                "base_snapshot_id":"snapshot_eval_1",
                "snapshot_id":"snapshot_eval_2",
                "document_revision":"doc_eval_2",
                "changes":changes,
                "changed_node_count":1,
                "truncated":false
            }
        }
    })
}

fn with_operation(mut response: Value, arguments: &Map<String, Value>) -> Value {
    response["operation"] = arguments.get("operation").cloned().unwrap_or(Value::Null);
    response
}

fn console_message() -> Value {
    json!({
        "message_id":"msg_7",
        "sequence":7,
        "level":"error",
        "text":"TypeError: saveRecord is not a function",
        "source":{"url":"app.js","line":42,"column":9}
    })
}

fn console_response(arguments: &Map<String, Value>) -> Value {
    match arguments.get("operation").and_then(Value::as_str) {
        Some("list") => {
            json!({"operation":"list","messages":[console_message()],"next_since":7,"truncated":false})
        }
        Some("get") => json!({"operation":"get","message":console_message()}),
        _ => json!({"operation":"clear","cleared":true}),
    }
}

fn network_request() -> Value {
    json!({
        "request_id":"req_9",
        "sequence":9,
        "url":"https://example.test/api/save",
        "method":"POST",
        "resource_type":"Fetch",
        "status":500,
        "mime_type":"application/json",
        "failed":true,
        "error_text":"Internal Server Error"
    })
}

fn network_response(arguments: &Map<String, Value>) -> Value {
    match arguments.get("operation").and_then(Value::as_str) {
        Some("list") => {
            json!({"operation":"list","requests":[network_request()],"next_since":9,"truncated":false})
        }
        Some("get") => {
            json!({"operation":"get","request":network_request(),"request_headers":{"content-type":"application/json"},"response_headers":{"content-type":"application/json"},"response_body":"{\"error\":\"validation_failed\"}"})
        }
        _ => json!({"operation":"clear","cleared":true}),
    }
}

fn performance_response(arguments: &Map<String, Value>) -> Value {
    match arguments.get("operation").and_then(Value::as_str) {
        Some("start_trace") => json!({"operation":"start_trace","recording":true}),
        Some("stop_trace") => {
            json!({"operation":"stop_trace","recording":false,"artifact":artifact("artifact_trace", "trace", "application/json")})
        }
        Some("vitals") => json!({"operation":"vitals","metrics":{"LCP":1200,"CLS":0.02}}),
        _ => {
            json!({"operation":"analyze","artifact":artifact("artifact_trace", "trace", "application/json"),"findings":[{"name":"long_task","severity":"warning","summary":"Long main-thread task: 240ms"}]})
        }
    }
}

fn artifacts_response(arguments: &Map<String, Value>) -> Value {
    let artifact = artifact("artifact_video", "screencast", "video/webm");
    match arguments.get("operation").and_then(Value::as_str) {
        Some("list") => json!({"operation":"list","artifacts":[artifact]}),
        Some("metadata") => json!({"operation":"metadata","artifact":artifact}),
        Some("read") => {
            json!({"operation":"read","artifact":artifact,"offset":0,"data_base64":"","eof":true})
        }
        Some("export") => {
            json!({"operation":"export","artifact":artifact,"path":"workspace/results/demo.webm"})
        }
        _ => json!({"operation":"delete","deleted":true}),
    }
}

fn artifact(id: &str, kind: &str, media_type: &str) -> Value {
    json!({
        "artifact_id":id,
        "kind":kind,
        "media_type":media_type,
        "size_bytes":1024,
        "sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "created_at_ms":1_782_449_108_000_u64,
        "retention":"session"
    })
}

fn fixture_snapshot(fixture: &Fixture) -> (String, Value) {
    let nodes = match fixture.id.as_str() {
        "form-fill-multiple" => vec![
            ("e1", "heading", "Checkout"),
            ("e2", "textbox", "Name"),
            ("e3", "textbox", "Street"),
            ("e4", "textbox", "Postal code"),
        ],
        "form-select-checkbox" => vec![
            ("e1", "heading", "Preferences"),
            ("e2", "combobox", "Plan"),
            ("e3", "checkbox", "Email reports"),
        ],
        "form-contenteditable" => vec![
            ("e1", "heading", "Composer"),
            (
                "e2",
                "textbox",
                "Message (contenteditable, focused, caret after current text: Draft)",
            ),
        ],
        "wait-text-appearance" => vec![
            ("e1", "heading", "Deployment"),
            ("e2", "status", "Deployment pending"),
        ],
        "wait-element-disappearance" => vec![
            ("e1", "heading", "Import"),
            ("e2", "progressbar", "Progress"),
        ],
        "frame-fill" => vec![
            ("e1", "heading", "Checkout"),
            ("e2", "iframe", "Payment"),
            ("e3", "textbox", "Billing email"),
        ],
        "nested-frame-click" => vec![
            ("e1", "heading", "Order"),
            ("e2", "iframe", "Payment"),
            ("e3", "iframe", "Confirmation"),
            ("e4", "button", "Confirm order"),
        ],
        "file-upload" => vec![
            ("e1", "heading", "Profile"),
            ("e2", "button", "Avatar file"),
        ],
        "file-drop" => vec![
            ("e1", "heading", "Import"),
            ("e2", "region", "CSV drop zone"),
        ],
        _ => vec![
            ("e1", "heading", "Dashboard"),
            ("e2", "navigation", "Primary navigation"),
            ("e3", "textbox", "Email"),
            ("e4", "button", "Confirm order"),
            ("e5", "status", "Build complete"),
        ],
    };
    let tree = nodes
        .iter()
        .map(|(reference, role, name)| format!("- {role} `{name}` [ref={reference}]"))
        .collect::<Vec<_>>()
        .join("\n");
    let nodes = Value::Array(
        nodes
            .into_iter()
            .map(|(reference, role, name)| json!({"ref":reference,"role":role,"name":name}))
            .collect(),
    );
    (tree, nodes)
}

fn requires_owned_tab(name: &str) -> bool {
    !matches!(
        name,
        "start_session" | "list_tabs" | "new_tab" | "claim_tab" | "help" | "artifacts"
    )
}

fn contains_css(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key("css") || map.values().any(contains_css),
        Value::Array(values) => values.iter().any(contains_css),
        _ => false,
    }
}

fn required_calls_complete(fixture: &Fixture, calls: &[LoggedCall]) -> bool {
    let mut index = 0;
    for call in calls {
        if index == fixture.required_calls.len() {
            break;
        }
        let expected = &fixture.required_calls[index];
        if call.tool == expected.tool && call.operation.as_deref() == expected.operation.as_deref()
        {
            index += 1;
        }
    }
    index == fixture.required_calls.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn foreign_actions_are_logged_and_refused_before_backend_execution() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("calls.jsonl");
        let server =
            EvaluationServer::new("ownership-foreign-action-refusal", log.clone()).unwrap();
        let result = server.execute(
            "click",
            Map::from_iter([
                ("agent_session_id".to_string(), json!("session_eval")),
                ("tab_id".to_string(), json!("tab_foreign")),
                ("target".to_string(), json!({"ref":"e1"})),
            ]),
        );
        assert_eq!(result.is_error, Some(true));
        let call: LoggedCall =
            serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
        assert!(call.ownership_refused);
        assert!(!call.backend_action);
    }

    #[test]
    fn undeclared_domain_operations_return_structured_recovery() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("calls.jsonl");
        let server = EvaluationServer::new("console-error-diagnosis", log.clone()).unwrap();
        let result = server.execute(
            "console",
            Map::from_iter([
                ("agent_session_id".to_string(), json!("session_eval")),
                ("tab_id".to_string(), json!("tab_owned")),
                ("operation".to_string(), json!("inspect")),
            ]),
        );
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str),
            Some("unsupported_operation")
        );
        let call: LoggedCall =
            serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
        assert!(!call.backend_action);
    }

    #[test]
    fn concurrent_log_appends_remain_complete_jsonl_records() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("calls.jsonl");
        let server = EvaluationServer::new("console-error-diagnosis", log.clone()).unwrap();
        const WORKERS: usize = 8;
        const RECORDS_PER_WORKER: usize = 25;

        std::thread::scope(|scope| {
            for worker in 0..WORKERS {
                let server = server.clone();
                scope.spawn(move || {
                    for record in 0..RECORDS_PER_WORKER {
                        server
                            .append_call(&LoggedCall {
                                tool: format!("worker-{worker}-record-{record}"),
                                operation: None,
                                css_fallback: false,
                                evaluate_fallback: false,
                                foreign_attempt: false,
                                backend_action: true,
                                ownership_refused: false,
                            })
                            .unwrap();
                    }
                });
            }
        });

        let contents = std::fs::read_to_string(log).unwrap();
        let records = contents
            .lines()
            .map(|line| serde_json::from_str::<LoggedCall>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), WORKERS * RECORDS_PER_WORKER);
        assert_eq!(
            records
                .iter()
                .map(|record| record.tool.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            WORKERS * RECORDS_PER_WORKER
        );
    }
}
