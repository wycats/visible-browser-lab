//! Authoritative definitions for the Visible Browser Lab agent-facing MCP surface.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use rmcp::model::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tiktoken_rs::o200k_base_singleton;

pub const SERVER_INSTRUCTIONS: &str = "Start each browser task with start_session and retain its agent_session_id. Use only tab_id values owned by that session. Inspect an unfamiliar page with snapshot, then act through its element references. Use fill for one ordinary field. Use fill_form for two or more controls, including combined select and checkbox updates. Use type_text for contenteditable controls and insertion at an established caret. Use press_key for named keys or shortcuts after the relevant element or document has been selected by snapshot, click, fill, or type_text. Use wait_for for asynchronous state and screenshot for visual appearance. Use console and network for runtime diagnosis. Use help to select an operation in a specialized domain. Routine click, key, and pointer actions attach to the owned target, prepare the resolved element, and preserve the user's active application. Target activation, including CDP `Target.activateTarget`, is reserved for focus_tab and focus: true tab creation when the user asks to bring Chrome forward for manual inspection or handoff. CSS and evaluate are escape hatches only when snapshot and the named semantic tools cannot represent the required state; do not use them to verify a semantic action.";

pub const DOMAIN_OPERATIONS: &[(&str, &[&str])] = &[
    (
        "interact",
        &[
            "select_options",
            "set_checked",
            "hover",
            "drag",
            "drop",
            "upload_files",
            "handle_dialog",
            "scroll",
            "click_at",
        ],
    ),
    ("console", &["list", "get", "clear"]),
    ("network", &["list", "get", "clear"]),
    (
        "emulation",
        &[
            "set_viewport",
            "set_network",
            "set_cpu",
            "set_geolocation",
            "set_media",
            "set_user_agent",
            "set_headers",
            "reset",
        ],
    ),
    (
        "performance",
        &["start_trace", "stop_trace", "vitals", "analyze"],
    ),
    ("audit", &["run"]),
    (
        "memory",
        &[
            "capture",
            "summary",
            "classes",
            "node",
            "dominators",
            "retainers",
            "retaining_paths",
            "edges",
            "close",
        ],
    ),
    ("screencast", &["start", "stop", "status"]),
    (
        "artifacts",
        &["list", "metadata", "read", "export", "delete"],
    ),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub annotations: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogMeasurement {
    pub encoding: String,
    pub hybrid_tools: usize,
    pub baseline_tools: usize,
    pub domain_operations: usize,
    pub hybrid_tokens: usize,
    pub baseline_tokens: usize,
    pub ratio: f64,
    pub maximum_ratio: f64,
    pub passes: bool,
}

pub fn hybrid_catalog() -> Vec<ToolDefinition> {
    vec![
        tool(
            "start_session",
            "Start Session",
            "Start one lease-scoped browser session and optionally create its first background tab.",
            start_session_schema(),
            session_result(),
            false,
            false,
            false,
        ),
        tool(
            "list_tabs",
            "List Tabs",
            "List caller-owned tab leases, or request the read-only target inventory without foreign action handles.",
            list_tabs_schema(),
            list_tabs_result(),
            true,
            false,
            true,
        ),
        tool(
            "new_tab",
            "New Tab",
            "Create a background browser tab and lease it to the caller's session.",
            url_scope_schema(false),
            tab_result(),
            false,
            false,
            false,
        ),
        tool(
            "claim_tab",
            "Claim Tab",
            "Claim an unowned target, or perform an explicitly instructed ownership transfer.",
            claim_tab_schema(),
            tab_result(),
            false,
            false,
            false,
        ),
        tool(
            "release_tab",
            "Release Tab",
            "Release an owned tab lease while leaving its browser target open and claimable.",
            page_scope_schema(),
            boolean_result("released"),
            false,
            false,
            true,
        ),
        tool(
            "focus_tab",
            "Focus Tab",
            "Bring an owned tab and its browser window to the foreground for explicit user handoff or manual inspection.",
            page_scope_schema(),
            tab_result(),
            false,
            false,
            true,
        ),
        tool(
            "close_tab",
            "Close Tab",
            "Close an owned browser target and complete its lease.",
            page_scope_schema(),
            boolean_result("closed"),
            false,
            true,
            true,
        ),
        tool(
            "snapshot",
            "Snapshot",
            "Inspect the compact accessibility tree and obtain lease-scoped element references for semantic actions.",
            snapshot_schema(),
            snapshot_result(),
            true,
            false,
            true,
        ),
        tool(
            "navigate",
            "Navigate",
            "Navigate an owned tab by URL, history, or reload while preserving application focus.",
            navigate_schema(),
            action_result(),
            false,
            false,
            false,
        ),
        tool(
            "wait_for",
            "Wait For",
            "Wait for semantic text, element state, URL, page load, or JavaScript state in an owned tab.",
            wait_schema(),
            wait_result(),
            true,
            false,
            true,
        ),
        tool(
            "click",
            "Click",
            "Click one accessibility reference after ownership and actionability checks. Use CSS only as an explicit fallback.",
            click_schema(),
            action_result(),
            false,
            false,
            false,
        ),
        tool(
            "fill",
            "Fill",
            "Replace the value of one referenced editable control and return a compact accessibility observation.",
            fill_schema(),
            action_result(),
            false,
            false,
            false,
        ),
        tool(
            "fill_form",
            "Fill Form",
            "Fill two or more referenced form controls sequentially and report completed fields.",
            fill_form_schema(),
            form_result(),
            false,
            false,
            false,
        ),
        tool(
            "type_text",
            "Type Text",
            "Insert text at the selection of one referenced editable element while preserving application focus.",
            type_text_schema(),
            action_result(),
            false,
            false,
            false,
        ),
        tool(
            "press_key",
            "Press Key",
            "Dispatch one native key to the focused owned document after an explicit focus transition.",
            press_key_schema(),
            action_result(),
            false,
            false,
            false,
        ),
        tool(
            "screenshot",
            "Screenshot",
            "Capture an owned page or referenced element as a renderable image artifact.",
            screenshot_schema(),
            screenshot_result(),
            true,
            false,
            true,
        ),
        tool(
            "evaluate",
            "Evaluate",
            "Evaluate JavaScript for page state that semantic snapshots and diagnostics do not expose.",
            evaluate_schema(),
            evaluation_result(),
            true,
            false,
            true,
        ),
        domain_tool(
            "interact",
            "Specialized Interaction",
            "Perform select, checkbox, hover, drag, drop, upload, dialog, scroll, or coordinate interaction.",
            operations("interact"),
            false,
        ),
        domain_tool(
            "console",
            "Console Diagnostics",
            "List, inspect, or clear lease-scoped console diagnostics.",
            operations("console"),
            true,
        ),
        domain_tool(
            "network",
            "Network Diagnostics",
            "List, inspect, or clear lease-scoped network diagnostics.",
            operations("network"),
            true,
        ),
        domain_tool(
            "emulation",
            "Emulation",
            "Configure viewport, network, CPU, geolocation, media, user agent, or headers for an owned target.",
            operations("emulation"),
            false,
        ),
        domain_tool(
            "performance",
            "Performance",
            "Capture traces, read web vitals, and analyze broker-produced performance artifacts.",
            operations("performance"),
            false,
        ),
        domain_tool(
            "audit",
            "Audit",
            "Run accessibility, best-practices, SEO, or agentic-browsing audits against an owned tab.",
            operations("audit"),
            true,
        ),
        domain_tool(
            "memory",
            "Memory",
            "Capture and inspect bounded heap-snapshot artifacts for an owned tab.",
            operations("memory"),
            false,
        ),
        domain_tool(
            "screencast",
            "Screencast",
            "Start, stop, or inspect an owned-tab screencast recording.",
            operations("screencast"),
            false,
        ),
        domain_tool(
            "artifacts",
            "Artifacts",
            "List, inspect, read, export, or delete browser artifacts owned by the session.",
            operations("artifacts"),
            false,
        ),
        tool(
            "help",
            "Browser Tool Help",
            "Choose the preferred explicit tool or specialized-domain operation for a browser task.",
            help_schema(),
            help_result(),
            true,
            false,
            true,
        ),
    ]
}

pub fn baseline_catalog() -> Vec<ToolDefinition> {
    let hybrid = hybrid_catalog();
    let domains: BTreeMap<&str, &[&str]> = DOMAIN_OPERATIONS.iter().copied().collect();
    let mut baseline = Vec::new();
    for definition in hybrid {
        if let Some(ops) = domains.get(definition.name.as_str()) {
            for operation in *ops {
                let mut expanded = definition.clone();
                expanded.name = format!("{}_{}", definition.name, operation);
                expanded.title = format!("{}: {}", definition.title, humanize(operation));
                expanded.description = baseline_operation_description(
                    &definition.name,
                    operation,
                    &definition.description,
                );
                expanded.input_schema = operation_schema(&definition.name, operation);
                expanded.output_schema = operation_output_schema(&definition.name, operation);
                baseline.push(expanded);
            }
        } else {
            baseline.push(definition);
        }
    }
    baseline
}

pub fn catalog_measurement() -> Result<CatalogMeasurement> {
    let hybrid = hybrid_catalog();
    let baseline = baseline_catalog();
    let domain_operations = DOMAIN_OPERATIONS.iter().map(|(_, ops)| ops.len()).sum();
    if hybrid.len() != 27 || baseline.len() != 63 || domain_operations != 45 {
        bail!(
            "catalog shape drifted: hybrid={}, baseline={}, domain_operations={domain_operations}",
            hybrid.len(),
            baseline.len()
        );
    }
    let hybrid_json = serde_json::to_vec(&json!({ "tools": hybrid }))?;
    let baseline_json = serde_json::to_vec(&json!({ "tools": baseline }))?;
    let encoder = o200k_base_singleton();
    let hybrid_tokens = encoder
        .encode_with_special_tokens(std::str::from_utf8(&hybrid_json)?)
        .len();
    let baseline_tokens = encoder
        .encode_with_special_tokens(std::str::from_utf8(&baseline_json)?)
        .len();
    let ratio = hybrid_tokens as f64 / baseline_tokens as f64;
    Ok(CatalogMeasurement {
        encoding: "o200k_base".to_string(),
        hybrid_tools: 27,
        baseline_tools: 63,
        domain_operations,
        hybrid_tokens,
        baseline_tokens,
        ratio,
        maximum_ratio: 0.60,
        passes: ratio <= 0.60,
    })
}

#[allow(clippy::too_many_arguments)]
fn tool(
    name: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        input_schema,
        output_schema,
        annotations: json!({
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": true
        }),
    }
}

fn domain_tool(
    name: &str,
    title: &str,
    description: &str,
    ops: &[&str],
    read_only: bool,
) -> ToolDefinition {
    let description = format!(
        "{description} Set `operation` to one of: {}.",
        ops.join(", ")
    );
    tool(
        name,
        title,
        &description,
        compact_domain_input_schema(name, ops),
        compact_domain_output_schema(name, ops),
        read_only,
        false,
        read_only,
    )
}

fn compact_domain_input_schema(domain: &str, operations: &[&str]) -> Value {
    let scope_name = if domain == "artifacts" { "s" } else { "p" };
    let scope = if domain == "artifacts" {
        object_schema(
            vec![("agent_session_id", string_schema())],
            &["agent_session_id"],
        )
    } else {
        page_scope_schema()
    };
    let variants = operations
        .iter()
        .map(|operation| {
            let mut properties =
                Map::from_iter([("operation".to_string(), const_string(operation))]);
            for (name, mut schema) in operation_fields(domain, operation) {
                replace_nested_schema(&mut schema, &element_target(), "#/$defs/e");
                replace_nested_schema(&mut schema, &observation_mode_schema(), "#/$defs/o");
                properties.insert(name.to_string(), schema);
            }
            let mut required = vec!["operation"];
            required.extend(operation_required(domain, operation));
            json!({
                "allOf": [
                    {"$ref": format!("#/$defs/{scope_name}")},
                    {"type":"object", "properties":properties, "required":required}
                ],
                "unevaluatedProperties": false
            })
        })
        .collect::<Vec<_>>();
    let variants = merge_equivalent_input_variants(variants);
    let mut definitions = Map::from_iter([(scope_name.to_string(), scope)]);
    if domain == "interact" {
        definitions.insert("e".to_string(), element_target());
        definitions.insert("o".to_string(), observation_mode_schema());
    }
    json!({"$defs": definitions, "oneOf": variants})
}

fn compact_domain_output_schema(domain: &str, operations: &[&str]) -> Value {
    if domain == "interact" {
        let mut fields = vec![("operation", enum_schema(operations))];
        fields.extend(page_action_properties());
        return object_schema(fields, &["operation", "document_revision", "observation"]);
    }
    if domain == "emulation" {
        return object_schema(
            vec![
                ("operation", enum_schema(operations)),
                ("effective", json!({})),
            ],
            &["operation", "effective"],
        );
    }
    if operations.len() == 1 {
        return operation_output_schema(domain, operations[0]);
    }
    let mut definitions = Map::new();
    let replacements = match domain {
        "console" => vec![
            ("a", artifact_summary_schema()),
            ("c", console_message_schema()),
        ],
        "network" => vec![
            ("a", artifact_summary_schema()),
            ("n", network_request_schema()),
        ],
        "performance" | "audit" | "memory" | "screencast" | "artifacts" => {
            vec![("a", artifact_summary_schema())]
        }
        _ => Vec::new(),
    };
    for (name, schema) in &replacements {
        definitions.insert((*name).to_string(), schema.clone());
    }
    let mut variants = operations
        .iter()
        .map(|operation| {
            let mut schema = operation_output_schema(domain, operation);
            for (name, definition) in &replacements {
                replace_nested_schema(&mut schema, definition, &format!("#/$defs/{name}"));
            }
            schema
        })
        .collect::<Vec<_>>();
    if domain == "memory" {
        variants.retain(|schema| {
            schema["properties"]["operation"]["const"] == "capture"
                || schema["properties"]["operation"]["const"] == "close"
        });
        variants.push(operation_result(
            "analysis",
            vec![
                ("artifact", json!({"$ref":"#/$defs/a"})),
                ("data", json!({})),
                ("next_cursor", string_schema()),
                ("truncated", json!({"type":"boolean"})),
            ],
            &["artifact", "data", "truncated"],
        ));
        variants.last_mut().expect("analysis variant")["properties"]["operation"] = enum_schema(&[
            "summary",
            "classes",
            "node",
            "dominators",
            "retainers",
            "retaining_paths",
            "edges",
        ]);
    }
    let mut schema = json!({"oneOf": variants});
    if !definitions.is_empty() {
        schema["$defs"] = Value::Object(definitions);
    }
    schema
}

fn merge_equivalent_input_variants(variants: Vec<Value>) -> Vec<Value> {
    let mut groups: Vec<(Value, Vec<String>)> = Vec::new();
    for mut variant in variants {
        let operation = variant["allOf"][1]["properties"]["operation"]["const"]
            .as_str()
            .expect("operation discriminator")
            .to_string();
        variant["allOf"][1]["properties"]["operation"] = json!({"const":"$operation"});
        if let Some((_, operations)) = groups.iter_mut().find(|(existing, _)| *existing == variant)
        {
            operations.push(operation);
        } else {
            groups.push((variant, vec![operation]));
        }
    }
    groups
        .into_iter()
        .map(|(mut variant, operations)| {
            variant["allOf"][1]["properties"]["operation"] = if operations.len() == 1 {
                const_string(&operations[0])
            } else {
                enum_schema(&operations.iter().map(String::as_str).collect::<Vec<_>>())
            };
            variant
        })
        .collect()
}

fn replace_nested_schema(value: &mut Value, needle: &Value, reference: &str) {
    if value == needle {
        *value = json!({"$ref":reference});
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                if value == needle {
                    *value = json!({"$ref":reference});
                } else {
                    replace_nested_schema(value, needle, reference);
                }
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                if value == needle {
                    *value = json!({"$ref":reference});
                } else {
                    replace_nested_schema(value, needle, reference);
                }
            }
        }
        _ => {}
    }
}

fn operations(name: &str) -> &'static [&'static str] {
    DOMAIN_OPERATIONS
        .iter()
        .find(|(domain, _)| *domain == name)
        .map(|(_, ops)| *ops)
        .expect("known domain")
}

fn operation_schema(domain: &str, operation: &str) -> Value {
    let mut properties = page_scope_properties();
    if domain == "artifacts" {
        properties.remove("tab_id");
    }
    properties.insert("operation".to_string(), json!({ "const": operation }));
    for (name, schema) in operation_fields(domain, operation) {
        properties.insert(name.to_string(), schema);
    }
    let mut required = vec!["agent_session_id", "operation"];
    if domain != "artifacts" {
        required.insert(1, "tab_id");
    }
    required.extend(operation_required(domain, operation));
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn operation_fields(domain: &str, operation: &str) -> Vec<(&'static str, Value)> {
    let mut fields = match (domain, operation) {
        ("interact", "select_options") => {
            vec![("target", element_target()), ("values", string_array())]
        }
        ("interact", "set_checked") => vec![
            ("target", element_target()),
            ("checked", json!({"type":"boolean"})),
        ],
        ("interact", "hover") => vec![("target", element_target())],
        ("interact", "drag") => vec![
            ("source", element_target()),
            ("destination", element_target()),
        ],
        ("interact", "drop") => vec![
            ("target", element_target()),
            ("paths", string_array()),
            (
                "data",
                json!({"type":"object","additionalProperties":{"type":"string"}}),
            ),
        ],
        ("interact", "upload_files") => {
            vec![("target", element_target()), ("paths", string_array())]
        }
        ("interact", "handle_dialog") => vec![
            ("action", enum_schema(&["accept", "dismiss"])),
            ("prompt_text", string_schema()),
        ],
        ("interact", "scroll") => vec![
            ("target", element_target()),
            ("delta_x", number_schema()),
            ("delta_y", number_schema()),
        ],
        ("interact", "click_at") => vec![
            ("x", number_schema()),
            ("y", number_schema()),
            ("button", enum_schema(&["left", "middle", "right"])),
            ("count", json!({"type":"integer","enum":[1,2]})),
            ("modifiers", modifier_array()),
        ],
        ("console", "list") => vec![
            ("since", integer_schema()),
            (
                "levels",
                enum_array(&["verbose", "debug", "info", "warning", "error"]),
            ),
            ("limit", integer_schema()),
        ],
        ("console", "get") => vec![("message_id", string_schema())],
        ("network", "list") => vec![
            ("since", integer_schema()),
            ("url_pattern", string_schema()),
            ("status_min", integer_schema()),
            ("status_max", integer_schema()),
            ("resource_types", string_array()),
            ("include_static", json!({"type":"boolean"})),
            ("limit", integer_schema()),
        ],
        ("network", "get") => vec![
            ("request_id", string_schema()),
            ("include_request_body", json!({"type":"boolean"})),
            ("include_response_body", json!({"type":"boolean"})),
            ("body_limit_bytes", integer_schema()),
        ],
        ("emulation", "set_viewport") => vec![
            ("width", integer_schema()),
            ("height", integer_schema()),
            ("device_scale_factor", number_schema()),
            ("mobile", json!({"type":"boolean"})),
            ("touch", json!({"type":"boolean"})),
            ("orientation", enum_schema(&["portrait", "landscape"])),
        ],
        ("emulation", "set_network") => vec![
            (
                "preset",
                enum_schema(&["offline", "slow_3g", "fast_3g", "slow_4g", "none"]),
            ),
            ("offline", json!({"type":"boolean"})),
            ("latency_ms", number_schema()),
            ("download_bytes_per_second", number_schema()),
            ("upload_bytes_per_second", number_schema()),
        ],
        ("emulation", "set_cpu") => vec![("slowdown", number_schema())],
        ("emulation", "set_geolocation") => vec![
            ("latitude", number_schema()),
            ("longitude", number_schema()),
            ("accuracy_meters", number_schema()),
        ],
        ("emulation", "set_media") => vec![
            ("media", enum_schema(&["screen", "print"])),
            (
                "color_scheme",
                enum_schema(&["light", "dark", "no_preference"]),
            ),
            ("reduced_motion", enum_schema(&["reduce", "no_preference"])),
        ],
        ("emulation", "set_user_agent") => vec![
            ("user_agent", string_schema()),
            ("platform", string_schema()),
            ("accept_language", string_schema()),
        ],
        ("emulation", "set_headers") => vec![(
            "headers",
            json!({"type":"object","additionalProperties":{"type":"string"}}),
        )],
        ("performance", "start_trace") => vec![
            ("reload", json!({"type":"boolean"})),
            ("screenshots", json!({"type":"boolean"})),
            ("categories", string_array()),
        ],
        ("performance", "vitals") => vec![("since_navigation", json!({"type":"boolean"}))],
        ("performance", "analyze") => vec![
            ("artifact_id", string_schema()),
            ("insight", string_schema()),
            ("max_findings", integer_schema()),
        ],
        ("audit", "run") => vec![
            (
                "categories",
                enum_array(&["accessibility", "seo", "best_practices", "agentic_browsing"]),
            ),
            ("mode", enum_schema(&["navigation", "snapshot"])),
            ("device", enum_schema(&["desktop", "mobile"])),
        ],
        ("memory", "capture") => vec![],
        ("memory", "summary") => vec![("artifact_id", string_schema())],
        ("memory", "classes") => vec![
            ("artifact_id", string_schema()),
            ("class_name", string_schema()),
            ("min_retained_bytes", integer_schema()),
            ("cursor", string_schema()),
            ("limit", integer_schema()),
        ],
        ("memory", "node") | ("memory", "retainers") => vec![
            ("artifact_id", string_schema()),
            ("node_id", string_schema()),
            ("cursor", string_schema()),
            ("limit", integer_schema()),
        ],
        ("memory", "dominators") => vec![
            ("artifact_id", string_schema()),
            ("node_id", string_schema()),
            ("cursor", string_schema()),
            ("limit", integer_schema()),
        ],
        ("memory", "retaining_paths") => vec![
            ("artifact_id", string_schema()),
            ("node_id", string_schema()),
            ("max_depth", integer_schema()),
            ("limit", integer_schema()),
        ],
        ("memory", "edges") => vec![
            ("artifact_id", string_schema()),
            ("node_id", string_schema()),
            ("direction", enum_schema(&["incoming", "outgoing"])),
            ("cursor", string_schema()),
            ("limit", integer_schema()),
        ],
        ("memory", "close") => vec![("artifact_id", string_schema())],
        ("screencast", "start") => vec![
            ("fps", integer_schema()),
            ("quality", integer_schema()),
            ("max_duration_ms", integer_schema()),
        ],
        ("artifacts", "list") => vec![
            ("tab_id", string_schema()),
            ("kinds", string_array()),
            ("cursor", string_schema()),
            ("limit", integer_schema()),
        ],
        ("artifacts", "metadata") | ("artifacts", "delete") => {
            vec![("artifact_id", string_schema())]
        }
        ("artifacts", "read") => vec![
            ("artifact_id", string_schema()),
            ("offset", integer_schema()),
            ("length", integer_schema()),
        ],
        ("artifacts", "export") => vec![
            ("artifact_id", string_schema()),
            ("path", string_schema()),
            ("overwrite", json!({"type":"boolean"})),
        ],
        _ => vec![],
    };
    if domain == "interact" {
        if !matches!(operation, "handle_dialog" | "scroll" | "click_at") {
            fields.push(("timeout_ms", integer_schema()));
        }
        fields.push(("observe", observation_mode_schema()));
    }
    fields
}

fn operation_required(domain: &str, operation: &str) -> Vec<&'static str> {
    match (domain, operation) {
        ("interact", "select_options") => vec!["target", "values"],
        ("interact", "set_checked") => vec!["target", "checked"],
        ("interact", "hover") => vec!["target"],
        ("interact", "drag") => vec!["source", "destination"],
        ("interact", "drop") => vec!["target"],
        ("interact", "upload_files") => vec!["target", "paths"],
        ("interact", "handle_dialog") => vec!["action"],
        ("interact", "scroll") => vec!["delta_y"],
        ("interact", "click_at") => vec!["x", "y"],
        ("console", "get") => vec!["message_id"],
        ("network", "get") => vec!["request_id"],
        ("emulation", "set_viewport") => vec!["width", "height"],
        ("emulation", "set_cpu") => vec!["slowdown"],
        ("emulation", "set_geolocation") => vec!["latitude", "longitude"],
        ("emulation", "set_user_agent") => vec!["user_agent"],
        ("emulation", "set_headers") => vec!["headers"],
        ("performance", "analyze") => vec!["artifact_id"],
        ("memory", "capture") => vec![],
        ("memory", "summary") | ("memory", "classes") | ("memory", "close") => {
            vec!["artifact_id"]
        }
        ("memory", "dominators") => vec!["artifact_id"],
        ("memory", "node")
        | ("memory", "retainers")
        | ("memory", "retaining_paths")
        | ("memory", "edges") => vec!["artifact_id", "node_id"],
        ("artifacts", "metadata") | ("artifacts", "read") | ("artifacts", "delete") => {
            vec!["artifact_id"]
        }
        ("artifacts", "export") => vec!["artifact_id", "path"],
        _ => vec![],
    }
}

fn page_scope_properties() -> Map<String, Value> {
    Map::from_iter([
        ("agent_session_id".to_string(), string_schema()),
        ("tab_id".to_string(), string_schema()),
    ])
}

fn page_scope_schema() -> Value {
    json!({"type":"object","properties":page_scope_properties(),"required":["agent_session_id","tab_id"],"additionalProperties":false})
}

fn start_session_schema() -> Value {
    object_schema(
        vec![
            ("label", string_schema()),
            ("start_url", string_schema()),
            ("focus", json!({"type":"boolean"})),
        ],
        &[],
    )
}

fn list_tabs_schema() -> Value {
    object_schema(
        vec![
            ("agent_session_id", string_schema()),
            ("scope", enum_schema(&["owned", "global_readonly"])),
        ],
        &["agent_session_id"],
    )
}

fn url_scope_schema(url_required: bool) -> Value {
    let mut props = page_scope_properties();
    props.remove("tab_id");
    props.insert("url".to_string(), string_schema());
    props.insert("focus".to_string(), json!({"type":"boolean"}));
    let required = if url_required {
        vec!["agent_session_id", "url"]
    } else {
        vec!["agent_session_id"]
    };
    json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
}

fn claim_tab_schema() -> Value {
    object_schema(
        vec![
            ("agent_session_id", string_schema()),
            ("target_id", string_schema()),
            ("takeover", json!({"type":"boolean"})),
            ("user_instruction", string_schema()),
        ],
        &["agent_session_id", "target_id"],
    )
}

fn snapshot_schema() -> Value {
    let mut props = page_scope_properties();
    props.extend(Map::from_iter([
        (
            "mode".to_string(),
            enum_schema(&["interactive", "meaningful", "full"]),
        ),
        ("root".to_string(), element_target()),
        ("depth".to_string(), integer_schema()),
        ("max_nodes".to_string(), integer_schema()),
        ("include_hidden".to_string(), json!({"type":"boolean"})),
        ("include_bounds".to_string(), json!({"type":"boolean"})),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id"],"additionalProperties":false})
}

fn navigate_schema() -> Value {
    let mut props = page_scope_properties();
    props.extend(Map::from_iter([
        (
            "action".to_string(),
            enum_schema(&["url", "back", "forward", "reload"]),
        ),
        ("url".to_string(), string_schema()),
        (
            "wait_until".to_string(),
            enum_schema(&["none", "dom_content_loaded", "load", "network_idle"]),
        ),
        ("timeout_ms".to_string(), integer_schema()),
        ("ignore_cache".to_string(), json!({"type":"boolean"})),
        (
            "before_unload".to_string(),
            enum_schema(&["accept", "dismiss"]),
        ),
        ("init_script".to_string(), string_schema()),
        ("observe".to_string(), observation_mode_schema()),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","action"],"additionalProperties":false})
}

fn wait_schema() -> Value {
    let mut props = page_scope_properties();
    props.extend(Map::from_iter([
        (
            "condition".to_string(),
            json!({"oneOf":[
                object_schema(vec![("kind", const_string("delay")), ("duration_ms", integer_schema())], &["kind", "duration_ms"]),
                object_schema(vec![("kind", const_string("text")), ("text", string_schema()), ("state", enum_schema(&["visible", "hidden"]))], &["kind", "text"]),
                object_schema(vec![("kind", const_string("element")), ("target", element_target()), ("state", enum_schema(&["attached", "detached", "visible", "hidden", "enabled", "disabled", "editable", "checked", "unchecked"]))], &["kind", "target", "state"]),
                object_schema(vec![("kind", const_string("url")), ("value", string_schema()), ("match", enum_schema(&["exact", "substring", "regex"]))], &["kind", "value"]),
                object_schema(vec![("kind", const_string("load")), ("state", enum_schema(&["dom_content_loaded", "load", "network_idle"]))], &["kind", "state"]),
                object_schema(vec![("kind", const_string("expression")), ("expression", string_schema())], &["kind", "expression"])
            ]}),
        ),
        ("timeout_ms".to_string(), integer_schema()),
        ("observe".to_string(), observation_mode_schema()),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","condition"],"additionalProperties":false})
}

fn base_element_action_schema() -> Map<String, Value> {
    let mut props = page_scope_properties();
    props.insert("target".to_string(), element_target());
    props.insert("timeout_ms".to_string(), integer_schema());
    props.insert("observe".to_string(), observation_mode_schema());
    props
}

fn click_schema() -> Value {
    let mut props = base_element_action_schema();
    props.extend(Map::from_iter([
        (
            "button".to_string(),
            enum_schema(&["left", "middle", "right"]),
        ),
        ("count".to_string(), json!({"type":"integer","enum":[1,2]})),
        ("modifiers".to_string(), modifier_array()),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","target"],"additionalProperties":false})
}

fn fill_schema() -> Value {
    let mut props = base_element_action_schema();
    props.insert("value".to_string(), json!({"type":"string"}));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","target","value"],"additionalProperties":false})
}

fn fill_form_schema() -> Value {
    let mut props = page_scope_properties();
    props.insert("fields".to_string(), json!({"type":"array","minItems":2,"items":{"oneOf":[
        object_schema(vec![("target", element_target()), ("kind", const_string("text")), ("value", json!({"type":"string"}))], &["target", "kind", "value"]),
        object_schema(vec![("target", element_target()), ("kind", const_string("select")), ("values", string_array())], &["target", "kind", "values"]),
        object_schema(vec![("target", element_target()), ("kind", const_string("checked")), ("checked", json!({"type":"boolean"}))], &["target", "kind", "checked"])
    ]}}));
    props.insert("timeout_ms".to_string(), integer_schema());
    props.insert("observe".to_string(), observation_mode_schema());
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","fields"],"additionalProperties":false})
}

fn type_text_schema() -> Value {
    let mut props = base_element_action_schema();
    props.insert("text".to_string(), json!({"type":"string"}));
    props.insert("delay_ms".to_string(), integer_schema());
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","target","text"],"additionalProperties":false})
}

fn press_key_schema() -> Value {
    let mut props = page_scope_properties();
    props.extend(Map::from_iter([
        ("key".to_string(), string_schema()),
        ("target".to_string(), element_target()),
        ("modifiers".to_string(), modifier_array()),
        ("timeout_ms".to_string(), integer_schema()),
        ("observe".to_string(), observation_mode_schema()),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","key"],"additionalProperties":false})
}

fn screenshot_schema() -> Value {
    let mut props = page_scope_properties();
    props.extend(Map::from_iter([
        ("target".to_string(), element_target()),
        ("full_page".to_string(), json!({"type":"boolean"})),
        ("format".to_string(), enum_schema(&["png", "jpeg", "webp"])),
        ("quality".to_string(), integer_schema()),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id"],"additionalProperties":false})
}

fn evaluate_schema() -> Value {
    let mut props = page_scope_properties();
    props.extend(Map::from_iter([
        ("source".to_string(), string_schema()),
        ("mode".to_string(), enum_schema(&["expression", "function"])),
        ("args".to_string(), json!({"type":"array","items":{}})),
        ("target".to_string(), element_target()),
        ("await_promise".to_string(), json!({"type":"boolean"})),
    ]));
    json!({"type":"object","properties":props,"required":["agent_session_id","tab_id","source"],"additionalProperties":false})
}

fn help_schema() -> Value {
    object_schema(
        vec![
            (
                "topic",
                enum_schema(&[
                    "workflow",
                    "tabs",
                    "snapshot",
                    "interaction",
                    "navigation",
                    "diagnostics",
                    "emulation",
                    "performance",
                    "audit",
                    "memory",
                    "screencast",
                    "artifacts",
                    "errors",
                ]),
            ),
            ("operation", string_schema()),
        ],
        &["topic"],
    )
}

fn operation_output_schema(domain: &str, operation: &str) -> Value {
    match domain {
        "interact" => {
            let mut fields = vec![("operation", const_string(operation))];
            fields.extend(page_action_properties());
            object_schema(fields, &["operation", "document_revision", "observation"])
        }
        "console" => match operation {
            "list" => operation_result(
                operation,
                vec![
                    ("messages", array_schema(console_message_schema())),
                    ("next_since", integer_schema()),
                    ("truncated", json!({"type":"boolean"})),
                    ("artifact", artifact_summary_schema()),
                ],
                &["messages", "next_since", "truncated"],
            ),
            "get" => operation_result(
                operation,
                vec![("message", console_message_schema())],
                &["message"],
            ),
            "clear" => operation_result(
                operation,
                vec![("cleared", json!({"const":true}))],
                &["cleared"],
            ),
            _ => unreachable!("known console operation"),
        },
        "network" => match operation {
            "list" => operation_result(
                operation,
                vec![
                    ("requests", array_schema(network_request_schema())),
                    ("next_since", integer_schema()),
                    ("truncated", json!({"type":"boolean"})),
                    ("artifact", artifact_summary_schema()),
                ],
                &["requests", "next_since", "truncated"],
            ),
            "get" => operation_result(
                operation,
                vec![
                    ("request", network_request_schema()),
                    ("request_headers", string_record_schema()),
                    ("response_headers", string_record_schema()),
                    ("request_body", json!({"type":"string"})),
                    ("response_body", json!({"type":"string"})),
                    ("body_artifact", artifact_summary_schema()),
                    ("timing", number_record_schema()),
                    ("initiator", json!({})),
                ],
                &["request", "request_headers"],
            ),
            "clear" => operation_result(
                operation,
                vec![("cleared", json!({"const":true}))],
                &["cleared"],
            ),
            _ => unreachable!("known network operation"),
        },
        "emulation" => operation_result(operation, vec![("effective", json!({}))], &["effective"]),
        "performance" => match operation {
            "start_trace" => operation_result(
                operation,
                vec![("recording", json!({"const":true}))],
                &["recording"],
            ),
            "stop_trace" => operation_result(
                operation,
                vec![
                    ("recording", json!({"const":false})),
                    ("artifact", artifact_summary_schema()),
                ],
                &["recording", "artifact"],
            ),
            "vitals" => operation_result(
                operation,
                vec![(
                    "metrics",
                    json!({"type":"object","additionalProperties":{"type":["number","null"]}}),
                )],
                &["metrics"],
            ),
            "analyze" => operation_result(
                operation,
                vec![
                    ("artifact", artifact_summary_schema()),
                    (
                        "findings",
                        array_schema(object_schema(
                            vec![
                                ("name", string_schema()),
                                ("severity", enum_schema(&["info", "warning", "error"])),
                                ("summary", string_schema()),
                                ("evidence", json!({})),
                            ],
                            &["name", "severity", "summary"],
                        )),
                    ),
                ],
                &["artifact", "findings"],
            ),
            _ => unreachable!("known performance operation"),
        },
        "audit" => operation_result(
            operation,
            vec![
                (
                    "scores",
                    json!({"type":"object","additionalProperties":{"type":["number","null"]}}),
                ),
                (
                    "findings",
                    array_schema(object_schema(
                        vec![
                            ("id", string_schema()),
                            ("category", string_schema()),
                            ("title", string_schema()),
                            ("description", string_schema()),
                            ("refs", string_array()),
                        ],
                        &["id", "category", "title", "description"],
                    )),
                ),
                ("reports", array_schema(artifact_summary_schema())),
            ],
            &["scores", "findings", "reports"],
        ),
        "memory" => match operation {
            "capture" => operation_result(
                operation,
                vec![("artifact", artifact_summary_schema())],
                &["artifact"],
            ),
            "close" => operation_result(
                operation,
                vec![("closed", json!({"const":true}))],
                &["closed"],
            ),
            _ => operation_result(
                operation,
                vec![
                    ("artifact", artifact_summary_schema()),
                    ("data", json!({})),
                    ("next_cursor", string_schema()),
                    ("truncated", json!({"type":"boolean"})),
                ],
                &["artifact", "data", "truncated"],
            ),
        },
        "screencast" => match operation {
            "start" => operation_result(
                operation,
                vec![
                    ("recording", json!({"const":true})),
                    ("started_at_ms", integer_schema()),
                ],
                &["recording", "started_at_ms"],
            ),
            "stop" => operation_result(
                operation,
                vec![
                    ("recording", json!({"const":false})),
                    ("artifact", artifact_summary_schema()),
                ],
                &["recording", "artifact"],
            ),
            "status" => operation_result(
                operation,
                vec![
                    ("recording", json!({"type":"boolean"})),
                    ("started_at_ms", integer_schema()),
                ],
                &["recording"],
            ),
            _ => unreachable!("known screencast operation"),
        },
        "artifacts" => match operation {
            "list" => operation_result(
                operation,
                vec![
                    ("artifacts", array_schema(artifact_summary_schema())),
                    ("next_cursor", string_schema()),
                ],
                &["artifacts"],
            ),
            "metadata" => operation_result(
                operation,
                vec![("artifact", artifact_summary_schema())],
                &["artifact"],
            ),
            "read" => operation_result(
                operation,
                vec![
                    ("artifact", artifact_summary_schema()),
                    ("offset", integer_schema()),
                    ("data_base64", json!({"type":"string"})),
                    ("eof", json!({"type":"boolean"})),
                ],
                &["artifact", "offset", "data_base64", "eof"],
            ),
            "export" => operation_result(
                operation,
                vec![
                    ("artifact", artifact_summary_schema()),
                    ("path", string_schema()),
                ],
                &["artifact", "path"],
            ),
            "delete" => operation_result(
                operation,
                vec![("deleted", json!({"const":true}))],
                &["deleted"],
            ),
            _ => unreachable!("known artifacts operation"),
        },
        _ => unreachable!("known domain"),
    }
}

fn operation_result(operation: &str, mut fields: Vec<(&str, Value)>, required: &[&str]) -> Value {
    fields.insert(0, ("operation", const_string(operation)));
    let mut all_required = vec!["operation"];
    all_required.extend_from_slice(required);
    object_schema(fields, &all_required)
}

fn page_action_properties() -> Vec<(&'static str, Value)> {
    vec![
        ("document_revision", string_schema()),
        ("observation", observation_schema()),
    ]
}

fn observation_schema() -> Value {
    json!({"oneOf":[
        object_schema(vec![("mode", const_string("none"))], &["mode"]),
        object_schema(vec![("mode", const_string("diff")), ("diff", snapshot_diff_schema())], &["mode", "diff"]),
        object_schema(vec![("mode", const_string("snapshot")), ("snapshot", snapshot_result())], &["mode", "snapshot"])
    ]})
}

fn snapshot_diff_schema() -> Value {
    object_schema(
        vec![
            ("base_snapshot_id", string_schema()),
            ("snapshot_id", string_schema()),
            ("document_revision", string_schema()),
            ("changes", json!({"type":"string"})),
            ("changed_node_count", integer_schema()),
            ("truncated", json!({"type":"boolean"})),
        ],
        &[
            "snapshot_id",
            "document_revision",
            "changes",
            "changed_node_count",
            "truncated",
        ],
    )
}

fn owned_tab_schema() -> Value {
    object_schema(
        vec![
            ("tab_id", string_schema()),
            ("target_id", string_schema()),
            ("title", json!({"type":"string"})),
            ("url", json!({"type":"string"})),
            ("state", enum_schema(&["active", "missing"])),
            ("focused", json!({"type":"boolean"})),
            ("created_at_ms", integer_schema()),
            ("updated_at_ms", integer_schema()),
        ],
        &[
            "tab_id",
            "target_id",
            "title",
            "url",
            "state",
            "focused",
            "created_at_ms",
            "updated_at_ms",
        ],
    )
}

fn global_tab_schema() -> Value {
    object_schema(
        vec![
            ("target_id", string_schema()),
            ("title", json!({"type":"string"})),
            ("url", json!({"type":"string"})),
            ("owner_display_id", string_schema()),
            ("owner_label", string_schema()),
            ("owned_by_caller", json!({"type":"boolean"})),
            ("caller_tab_id", string_schema()),
            ("claimable", json!({"type":"boolean"})),
            ("focused", json!({"type":"boolean"})),
        ],
        &[
            "target_id",
            "title",
            "url",
            "owned_by_caller",
            "claimable",
            "focused",
        ],
    )
}

fn artifact_summary_schema() -> Value {
    object_schema(
        vec![
            ("artifact_id", string_schema()),
            (
                "kind",
                enum_schema(&[
                    "screenshot",
                    "console",
                    "network",
                    "trace",
                    "audit",
                    "heap_snapshot",
                    "screencast",
                    "evaluation",
                ]),
            ),
            ("media_type", string_schema()),
            ("size_bytes", integer_schema()),
            ("sha256", string_schema()),
            ("created_at_ms", integer_schema()),
            ("retention", const_string("session")),
        ],
        &[
            "artifact_id",
            "kind",
            "media_type",
            "size_bytes",
            "sha256",
            "created_at_ms",
            "retention",
        ],
    )
}

fn console_message_schema() -> Value {
    object_schema(
        vec![
            ("message_id", string_schema()),
            ("sequence", integer_schema()),
            (
                "level",
                enum_schema(&["verbose", "debug", "info", "warning", "error"]),
            ),
            ("text", json!({"type":"string"})),
            ("timestamp_ms", integer_schema()),
            (
                "source",
                object_schema(
                    vec![
                        ("url", json!({"type":"string"})),
                        ("line", integer_schema()),
                        ("column", integer_schema()),
                    ],
                    &[],
                ),
            ),
            ("stack", string_array()),
            ("arguments", array_schema(json!({}))),
        ],
        &["message_id", "sequence", "level", "text"],
    )
}

fn network_request_schema() -> Value {
    object_schema(
        vec![
            ("request_id", string_schema()),
            ("sequence", integer_schema()),
            ("url", json!({"type":"string"})),
            ("method", string_schema()),
            ("resource_type", string_schema()),
            ("status", integer_schema()),
            ("mime_type", string_schema()),
            ("failed", json!({"type":"boolean"})),
            ("error_text", json!({"type":"string"})),
            ("started_at_ms", integer_schema()),
            ("duration_ms", number_schema()),
        ],
        &["request_id", "sequence", "url", "method"],
    )
}

fn string_record_schema() -> Value {
    json!({"type":"object","additionalProperties":{"type":"string"}})
}

fn number_record_schema() -> Value {
    json!({"type":"object","additionalProperties":{"type":"number"}})
}

fn session_result() -> Value {
    object_schema(
        vec![
            ("agent_session_id", string_schema()),
            ("tab", owned_tab_schema()),
        ],
        &["agent_session_id"],
    )
}
fn tab_result() -> Value {
    object_schema(vec![("tab", owned_tab_schema())], &["tab"])
}
fn action_result() -> Value {
    object_schema(
        page_action_properties().into_iter().collect(),
        &["document_revision", "observation"],
    )
}

fn boolean_result(field: &str) -> Value {
    object_schema(vec![(field, json!({"const":true}))], &[field])
}

fn list_tabs_result() -> Value {
    json!({"oneOf":[
        object_schema(
            vec![("scope", const_string("owned")), ("tabs", array_schema(owned_tab_schema()))],
            &["scope", "tabs"]
        ),
        object_schema(
            vec![
                ("scope", const_string("global_readonly")),
                ("groups", array_schema(object_schema(
                    vec![
                        ("owner_display_id", string_schema()),
                        ("owner_label", string_schema()),
                        ("tabs", array_schema(global_tab_schema())),
                    ],
                    &["tabs"]
                )))
            ],
            &["scope", "groups"]
        )
    ]})
}

fn snapshot_result() -> Value {
    object_schema(
        vec![
            ("snapshot_id", string_schema()),
            ("document_revision", string_schema()),
            ("url", json!({"type":"string"})),
            ("title", json!({"type":"string"})),
            ("tree", json!({"type":"string"})),
            ("node_count", integer_schema()),
            ("truncated", json!({"type":"boolean"})),
        ],
        &[
            "snapshot_id",
            "document_revision",
            "url",
            "title",
            "tree",
            "node_count",
            "truncated",
        ],
    )
}

fn wait_result() -> Value {
    object_schema(
        vec![
            ("matched", json!({"const":true})),
            ("elapsed_ms", integer_schema()),
            ("document_revision", string_schema()),
            ("observation", observation_schema()),
        ],
        &["matched", "elapsed_ms", "document_revision", "observation"],
    )
}

fn form_result() -> Value {
    object_schema(
        vec![
            ("completed_fields", integer_schema()),
            ("total_fields", integer_schema()),
            ("document_revision", string_schema()),
            ("observation", observation_schema()),
        ],
        &[
            "completed_fields",
            "total_fields",
            "document_revision",
            "observation",
        ],
    )
}

fn screenshot_result() -> Value {
    object_schema(
        vec![
            ("artifact", artifact_summary_schema()),
            (
                "image",
                object_schema(
                    vec![(
                        "media_type",
                        enum_schema(&["image/png", "image/jpeg", "image/webp"]),
                    )],
                    &["media_type"],
                ),
            ),
            ("width", integer_schema()),
            ("height", integer_schema()),
        ],
        &["artifact", "image", "width", "height"],
    )
}

fn evaluation_result() -> Value {
    object_schema(
        vec![
            ("value", json!({})),
            ("preview", json!({"type":"string"})),
            ("artifact", artifact_summary_schema()),
        ],
        &[],
    )
}

fn help_result() -> Value {
    let tool_choice = object_schema(
        vec![
            ("tool", string_schema()),
            ("operation", string_schema()),
            ("reason", string_schema()),
        ],
        &["tool", "reason"],
    );
    object_schema(
        vec![
            ("topic", string_schema()),
            ("operation", string_schema()),
            ("task", string_schema()),
            ("preferred", tool_choice),
            (
                "neighbors",
                array_schema(object_schema(
                    vec![
                        ("tool", string_schema()),
                        ("operation", string_schema()),
                        ("use_when", string_schema()),
                    ],
                    &["tool", "use_when"],
                )),
            ),
            (
                "example",
                object_schema(
                    vec![("tool", string_schema()), ("arguments", json!({}))],
                    &["tool", "arguments"],
                ),
            ),
            ("result_schema", json!({})),
            (
                "errors",
                array_schema(object_schema(
                    vec![("code", string_schema()), ("recovery", string_schema())],
                    &["code", "recovery"],
                )),
            ),
        ],
        &[
            "topic",
            "task",
            "preferred",
            "neighbors",
            "example",
            "result_schema",
            "errors",
        ],
    )
}

fn object_schema(properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let properties = properties
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect::<Map<_, _>>();
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn element_target() -> Value {
    json!({"oneOf":[
        object_schema(vec![("ref", string_schema())], &["ref"]),
        object_schema(vec![("css", string_schema()), ("frame_ref", string_schema())], &["css"])
    ]})
}
fn string_schema() -> Value {
    json!({"type":"string","minLength":1})
}
fn string_array() -> Value {
    json!({"type":"array","items":{"type":"string"}})
}
fn enum_array(values: &[&str]) -> Value {
    json!({"type":"array","items":enum_schema(values)})
}
fn modifier_array() -> Value {
    enum_array(&["alt", "control", "meta", "shift"])
}
fn observation_mode_schema() -> Value {
    enum_schema(&["none", "diff", "snapshot"])
}
fn const_string(value: &str) -> Value {
    json!({"type":"string","const":value})
}
fn array_schema(items: Value) -> Value {
    json!({"type":"array","items":items})
}
fn integer_schema() -> Value {
    json!({"type":"integer"})
}
fn number_schema() -> Value {
    json!({"type":"number"})
}
fn enum_schema(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn humanize(value: &str) -> String {
    value
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn baseline_operation_description(domain: &str, operation: &str, domain_summary: &str) -> String {
    let purpose = match (domain, operation) {
        ("interact", "select_options") => {
            "Select one or more values in a referenced select control and report the resulting page observation."
        }
        ("interact", "set_checked") => {
            "Set a referenced checkbox or radio control to the requested checked state and report the resulting page observation."
        }
        ("interact", "hover") => {
            "Move the browser pointer over a referenced element after target preparation and actionability checks."
        }
        ("interact", "drag") => {
            "Drag one referenced element to another using browser pointer input and report the resulting page observation."
        }
        ("interact", "drop") => {
            "Drop workspace files or string data on a referenced target and report the resulting page observation."
        }
        ("interact", "upload_files") => {
            "Set workspace files on a referenced file input and report the resulting page observation."
        }
        ("interact", "handle_dialog") => {
            "Accept or dismiss the currently open JavaScript dialog, optionally supplying prompt text."
        }
        ("interact", "scroll") => {
            "Scroll the owned page or a referenced element by the requested horizontal and vertical deltas."
        }
        ("interact", "click_at") => {
            "Dispatch a browser pointer click at explicit page coordinates after target preparation."
        }
        ("console", "list") => {
            "List bounded lease-scoped console messages using sequence, severity, and limit filters."
        }
        ("console", "get") => {
            "Retrieve one lease-scoped console message with arguments, source location, and stack details."
        }
        ("console", "clear") => {
            "Clear the lease-scoped console diagnostic buffer and confirm the completed state transition."
        }
        ("network", "list") => {
            "List bounded lease-scoped network requests using sequence, URL, resource, status, and static-resource filters."
        }
        ("network", "get") => {
            "Retrieve one request with bounded headers, bodies, timing, initiator, and failure details."
        }
        ("network", "clear") => {
            "Clear the lease-scoped network diagnostic buffer and confirm the completed state transition."
        }
        ("emulation", "set_viewport") => {
            "Set viewport dimensions, scale, mobile, touch, and orientation values and return the effective configuration."
        }
        ("emulation", "set_network") => {
            "Apply a named or explicit network condition and return the effective configuration."
        }
        ("emulation", "set_cpu") => {
            "Set the CPU slowdown factor for the owned target and return the effective configuration."
        }
        ("emulation", "set_geolocation") => {
            "Set latitude, longitude, and accuracy for the owned target and return the effective configuration."
        }
        ("emulation", "set_media") => {
            "Set media type, color scheme, and reduced-motion preferences and return the effective configuration."
        }
        ("emulation", "set_user_agent") => {
            "Set user-agent, platform, and language values and return the effective configuration."
        }
        ("emulation", "set_headers") => {
            "Set the request-header override for the owned target and return the effective configuration."
        }
        ("emulation", "reset") => {
            "Reset all emulation state on the owned target and return the effective configuration."
        }
        ("performance", "start_trace") => {
            "Start an owned-target performance trace with optional reload, screenshots, and category selection."
        }
        ("performance", "stop_trace") => {
            "Stop the active performance trace and return its immutable browser artifact metadata."
        }
        ("performance", "vitals") => {
            "Read the owned page's current web-vitals metrics without starting a trace."
        }
        ("performance", "analyze") => {
            "Analyze a broker-produced trace artifact with bounded parameters and return structured findings."
        }
        ("audit", "run") => {
            "Run selected accessibility, SEO, best-practices, or agentic-browsing checks and return scores, findings, and reports."
        }
        ("memory", "capture") => {
            "Capture an owned-tab heap snapshot and return its immutable artifact metadata."
        }
        ("memory", "summary") => "Return a bounded summary of one owned heap-snapshot artifact.",
        ("memory", "classes") => {
            "List bounded heap classes using class-name, retained-size, cursor, and limit filters."
        }
        ("memory", "node") => "Inspect one heap node in an owned heap-snapshot artifact.",
        ("memory", "dominators") => {
            "List bounded dominator information for an owned heap-snapshot artifact or node."
        }
        ("memory", "retainers") => {
            "List bounded retainers for one heap node in an owned heap-snapshot artifact."
        }
        ("memory", "retaining_paths") => {
            "Find bounded retaining paths for one heap node with an optional depth limit."
        }
        ("memory", "edges") => "List bounded incoming or outgoing edges for one heap node.",
        ("memory", "close") => {
            "Close an owned heap-snapshot artifact and release its analysis resources."
        }
        ("screencast", "start") => {
            "Start an owned-tab silent WebM screencast with bounded frame rate, quality, and duration settings."
        }
        ("screencast", "stop") => {
            "Stop the active screencast and return its immutable video artifact metadata."
        }
        ("screencast", "status") => {
            "Read whether the owned tab has an active screencast and when it started."
        }
        ("artifacts", "list") => {
            "List bounded session-owned browser artifacts using tab, kind, cursor, and limit filters."
        }
        ("artifacts", "metadata") => {
            "Read immutable metadata for one session-owned browser artifact."
        }
        ("artifacts", "read") => {
            "Read one bounded base64 segment from a session-owned browser artifact."
        }
        ("artifacts", "export") => {
            "Export a session-owned browser artifact to a workspace-relative path."
        }
        ("artifacts", "delete") => {
            "Delete one session-owned browser artifact and confirm the completed state transition."
        }
        _ => unreachable!("known domain operation"),
    };
    let scope = if domain == "artifacts" {
        "Supply the caller's `agent_session_id`; artifact ownership is verified before the operation."
    } else {
        "Supply the caller's `agent_session_id` and owned `tab_id`; tab ownership is verified before browser access."
    };
    format!(
        "{domain_summary} {purpose} {scope} Inspect the structured result before selecting the next browser operation."
    )
}

pub fn validate_catalog_contract() -> Result<()> {
    let hybrid = hybrid_catalog();
    let baseline = baseline_catalog();
    let names = hybrid
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    if hybrid.len() != 27 || names.len() != 27 || baseline.len() != 63 {
        bail!(
            "catalog shape drifted: hybrid={}, unique={}, baseline={}",
            hybrid.len(),
            names.len(),
            baseline.len()
        );
    }
    if serde_json::to_vec(&hybrid)? != serde_json::to_vec(&hybrid_catalog())? {
        bail!("catalog serialization is not deterministic");
    }
    for tool in &hybrid {
        if tool.description.is_empty()
            || !tool.input_schema.is_object()
            || !tool.output_schema.is_object()
            || !tool.annotations.is_object()
        {
            bail!("tool `{}` has incomplete MCP metadata", tool.name);
        }
        serde_json::from_value::<Tool>(serde_json::to_value(tool)?).map_err(|error| {
            anyhow::anyhow!("tool `{}` is not valid MCP metadata: {error}", tool.name)
        })?;
        for schema in [&tool.input_schema, &tool.output_schema] {
            let definitions = schema
                .get("$defs")
                .and_then(Value::as_object)
                .map(|definitions| definitions.keys().cloned().collect())
                .unwrap_or_default();
            let references = local_references(schema);
            if references != definitions {
                bail!(
                    "tool `{}` contains unresolved or unused local definitions",
                    tool.name
                );
            }
        }
    }
    for (domain, operations) in DOMAIN_OPERATIONS {
        let tool = hybrid
            .iter()
            .find(|tool| tool.name == *domain)
            .ok_or_else(|| anyhow::anyhow!("domain tool `{domain}` is missing"))?;
        let expected = operations
            .iter()
            .map(|operation| (*operation).to_string())
            .collect::<BTreeSet<_>>();
        if operation_discriminators(&tool.input_schema) != expected
            || operation_discriminators(&tool.output_schema) != expected
        {
            bail!("domain tool `{domain}` does not cover its declared operations");
        }
    }
    let measurement = catalog_measurement()?;
    if !measurement.passes {
        bail!(
            "hybrid catalog token ratio {:.4} exceeds {:.2}",
            measurement.ratio,
            measurement.maximum_ratio
        );
    }
    Ok(())
}

fn operation_discriminators(schema: &Value) -> BTreeSet<String> {
    let mut operations = BTreeSet::new();
    collect_operation_discriminators(schema, &mut operations);
    operations
}

fn collect_operation_discriminators(value: &Value, operations: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(discriminator) = object
                .get("properties")
                .and_then(|properties| properties.get("operation"))
            {
                if let Some(operation) = discriminator.get("const").and_then(Value::as_str) {
                    operations.insert(operation.to_string());
                }
                if let Some(values) = discriminator.get("enum").and_then(Value::as_array) {
                    operations.extend(values.iter().filter_map(Value::as_str).map(str::to_string));
                }
            }
            for child in object.values() {
                collect_operation_discriminators(child, operations);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_operation_discriminators(child, operations);
            }
        }
        _ => {}
    }
}

fn local_references(schema: &Value) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    collect_local_references(schema, &mut references);
    references
}

fn collect_local_references(value: &Value, references: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && let Some(name) = reference.strip_prefix("#/$defs/")
            {
                references.insert(name.to_string());
            }
            for (name, child) in object {
                if name != "$defs" {
                    collect_local_references(child, references);
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_local_references(child, references);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_the_rfc_shape_and_unique_names() {
        let hybrid = hybrid_catalog();
        let baseline = baseline_catalog();
        assert_eq!(hybrid.len(), 27);
        assert_eq!(baseline.len(), 63);
        let names = hybrid
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), 27);
        assert_eq!(
            DOMAIN_OPERATIONS
                .iter()
                .map(|(_, ops)| ops.len())
                .sum::<usize>(),
            45
        );
    }

    #[test]
    fn catalog_serialization_is_deterministic_and_below_budget() {
        let first = serde_json::to_vec(&hybrid_catalog()).unwrap();
        let second = serde_json::to_vec(&hybrid_catalog()).unwrap();
        assert_eq!(first, second);
        let measurement = catalog_measurement().unwrap();
        assert!(measurement.passes, "{measurement:?}");
    }

    #[test]
    fn every_tool_has_complete_mcp_metadata() {
        for tool in hybrid_catalog() {
            assert!(!tool.description.is_empty());
            assert!(tool.input_schema.is_object());
            assert!(tool.output_schema.is_object());
            assert!(tool.annotations.is_object());
        }
    }

    #[test]
    fn every_domain_schema_advertises_each_operation_in_inputs_and_outputs() {
        let catalog = hybrid_catalog();
        for (domain, operations) in DOMAIN_OPERATIONS {
            let tool = catalog
                .iter()
                .find(|tool| tool.name == *domain)
                .expect("domain tool");
            let expected = operations
                .iter()
                .map(|operation| (*operation).to_string())
                .collect::<BTreeSet<_>>();
            assert_eq!(operation_discriminators(&tool.input_schema), expected);
            assert_eq!(operation_discriminators(&tool.output_schema), expected);
        }
    }

    #[test]
    fn every_local_schema_definition_is_referenced_and_resolves() {
        for tool in hybrid_catalog() {
            for schema in [&tool.input_schema, &tool.output_schema] {
                let definitions = schema
                    .get("$defs")
                    .and_then(Value::as_object)
                    .map(|definitions| definitions.keys().cloned().collect())
                    .unwrap_or_default();
                let references = local_references(schema);
                assert_eq!(
                    references, definitions,
                    "{} contains unresolved or unused local definitions",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn callable_catalog_validation_enforces_the_complete_contract() {
        validate_catalog_contract().unwrap();
    }
}
