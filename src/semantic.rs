use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::{
    leases::{AgentSessionId, BrowserToolError, TabId},
    protocol::{SnapshotDiff, SnapshotMode, SnapshotResult},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAxNode {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub child_ids: Vec<String>,
    pub backend_node_id: Option<i64>,
    pub frame_id: String,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub properties: Vec<(String, String)>,
    pub ignored: bool,
    pub bounds: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAxFrame {
    pub frame_id: String,
    pub parent_frame_id: Option<String>,
    pub loader_id: String,
    pub url: String,
    pub nodes: Vec<RawAxNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAxSnapshot {
    pub title: String,
    pub url: String,
    pub frames: Vec<RawAxFrame>,
}

impl RawAxSnapshot {
    pub fn document_revision(&self) -> Result<&str, BrowserToolError> {
        self.frames
            .iter()
            .find(|frame| frame.parent_frame_id.is_none())
            .map(|frame| frame.loader_id.as_str())
            .ok_or_else(|| {
                BrowserToolError::chrome_unavailable(
                    "accessibility snapshot omitted the root frame",
                )
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementReference {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub target_id: String,
    pub frame_id: String,
    pub document_revision: String,
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NodeKey {
    frame_id: String,
    document_revision: String,
    backend_node_id: i64,
}

#[derive(Debug, Default)]
struct TabReferences {
    agent_session_id: Option<AgentSessionId>,
    target_id: String,
    document_revision: String,
    by_ref: HashMap<String, ElementReference>,
    by_node: HashMap<NodeKey, String>,
    last_snapshot: Option<SnapshotResult>,
}

#[derive(Debug, Default)]
pub struct ElementReferenceRegistry {
    tabs: HashMap<TabId, TabReferences>,
    next_ref: u64,
}

pub struct SnapshotBuildContext<'a> {
    pub agent_session_id: &'a AgentSessionId,
    pub tab_id: &'a TabId,
    pub target_id: &'a str,
    pub mode: SnapshotMode,
    pub root_backend_node_id: Option<i64>,
    pub depth: usize,
    pub max_nodes: usize,
    pub include_hidden: bool,
    pub include_bounds: bool,
}

impl ElementReferenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn build_snapshot(
        &mut self,
        context: SnapshotBuildContext<'_>,
        raw: RawAxSnapshot,
    ) -> Result<(SnapshotResult, SnapshotDiff), BrowserToolError> {
        let SnapshotBuildContext {
            agent_session_id,
            tab_id,
            target_id,
            mode,
            root_backend_node_id,
            depth,
            max_nodes,
            include_hidden,
            include_bounds,
        } = context;
        let document_revision = raw.document_revision()?.to_string();
        if let Some(root_backend_node_id) = root_backend_node_id
            && !raw.frames.iter().any(|frame| {
                frame
                    .nodes
                    .iter()
                    .any(|node| node.backend_node_id == Some(root_backend_node_id))
            })
        {
            return Err(BrowserToolError::element_not_found(
                "snapshot root is absent from the accessibility tree",
            ));
        }
        let next_ref = &mut self.next_ref;
        let tab = self.tabs.entry(tab_id.clone()).or_default();
        if tab.agent_session_id.as_ref() != Some(agent_session_id)
            || tab.target_id != target_id
            || tab.document_revision != document_revision
        {
            tab.agent_session_id = Some(agent_session_id.clone());
            tab.target_id = target_id.to_string();
            tab.document_revision = document_revision.clone();
            tab.by_ref.clear();
            tab.by_node.clear();
            tab.last_snapshot = None;
        }

        let previous = tab.last_snapshot.clone();
        let mut formatter = SnapshotFormatter {
            tab,
            next_ref,
            agent_session_id,
            tab_id,
            target_id,
            document_revision: &document_revision,
            mode,
            root_backend_node_id,
            depth,
            max_nodes,
            include_hidden,
            include_bounds,
            node_count: 0,
            truncated: false,
            lines: vec![format!(
                "document {} url={} revision={}",
                quoted(&raw.title),
                quoted(&raw.url),
                quoted(&document_revision)
            )],
        };
        formatter.format_frames(&raw.frames);

        let snapshot = SnapshotResult {
            snapshot_id: prefixed_uuid("snapshot"),
            document_revision: document_revision.clone(),
            url: raw.url,
            title: raw.title,
            tree: formatter.lines.join("\n"),
            node_count: formatter.node_count,
            truncated: formatter.truncated,
        };
        let diff = snapshot_diff(previous.as_ref(), &snapshot);
        formatter.tab.last_snapshot = Some(snapshot.clone());
        Ok((snapshot, diff))
    }

    pub fn resolve(
        &self,
        agent_session_id: &AgentSessionId,
        tab_id: &TabId,
        reference: &str,
        document_revision: &str,
    ) -> Result<ElementReference, BrowserToolError> {
        let Some(tab) = self.tabs.get(tab_id) else {
            return Err(BrowserToolError::element_stale(reference));
        };
        let Some(element) = tab.by_ref.get(reference) else {
            return Err(BrowserToolError::element_stale(reference));
        };
        if &element.agent_session_id != agent_session_id
            || &element.tab_id != tab_id
            || element.document_revision != document_revision
        {
            return Err(BrowserToolError::element_stale(reference));
        }
        Ok(element.clone())
    }

    pub fn document_revision(&self, tab_id: &TabId) -> Option<String> {
        self.tabs
            .get(tab_id)
            .map(|tab| tab.document_revision.clone())
    }

    pub fn reference_for_backend_node(
        &self,
        agent_session_id: &AgentSessionId,
        tab_id: &TabId,
        backend_node_id: i64,
        document_revision: &str,
    ) -> Option<String> {
        let tab = self.tabs.get(tab_id)?;
        if tab.agent_session_id.as_ref() != Some(agent_session_id)
            || tab.document_revision != document_revision
        {
            return None;
        }
        tab.by_node
            .iter()
            .find(|(key, _)| {
                key.backend_node_id == backend_node_id && key.document_revision == document_revision
            })
            .map(|(_, reference)| reference.clone())
    }

    pub fn reset_tab(&mut self, tab_id: &TabId) {
        self.tabs.remove(tab_id);
    }

    pub fn reset_target(&mut self, target_id: &str) {
        self.tabs.retain(|_, tab| tab.target_id != target_id);
    }
}

struct SnapshotFormatter<'a> {
    tab: &'a mut TabReferences,
    next_ref: &'a mut u64,
    agent_session_id: &'a AgentSessionId,
    tab_id: &'a TabId,
    target_id: &'a str,
    document_revision: &'a str,
    mode: SnapshotMode,
    root_backend_node_id: Option<i64>,
    depth: usize,
    max_nodes: usize,
    include_hidden: bool,
    include_bounds: bool,
    node_count: usize,
    truncated: bool,
    lines: Vec<String>,
}

impl SnapshotFormatter<'_> {
    fn format_frames(&mut self, frames: &[RawAxFrame]) {
        if let Some(root_backend_node_id) = self.root_backend_node_id {
            for frame in frames {
                let Some(root) = frame
                    .nodes
                    .iter()
                    .find(|node| node.backend_node_id == Some(root_backend_node_id))
                else {
                    continue;
                };
                self.lines.push(format!(
                    "  frame id={} url={}",
                    quoted(&frame.frame_id),
                    quoted(&frame.url)
                ));
                let nodes = frame
                    .nodes
                    .iter()
                    .map(|node| (node.node_id.as_str(), node))
                    .collect::<HashMap<_, _>>();
                self.format_node(root, &nodes, 2, 0);
                return;
            }
            return;
        }
        for frame in frames {
            if self.truncated {
                break;
            }
            let label = if frame.parent_frame_id.is_none() {
                "main-frame"
            } else {
                "frame"
            };
            self.lines.push(format!(
                "  {label} id={} url={}",
                quoted(&frame.frame_id),
                quoted(&frame.url)
            ));
            self.format_frame(frame);
        }
    }

    fn format_frame(&mut self, frame: &RawAxFrame) {
        let nodes = frame
            .nodes
            .iter()
            .map(|node| (node.node_id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let known_ids = nodes.keys().copied().collect::<HashSet<_>>();
        let roots = frame
            .nodes
            .iter()
            .filter(|node| {
                node.parent_id
                    .as_deref()
                    .is_none_or(|parent| !known_ids.contains(parent))
            })
            .collect::<Vec<_>>();

        for root in roots {
            self.format_node(root, &nodes, 2, 0);
            if self.truncated {
                break;
            }
        }
    }

    fn format_node(
        &mut self,
        node: &RawAxNode,
        nodes: &HashMap<&str, &RawAxNode>,
        indent: usize,
        tree_depth: usize,
    ) {
        if self.truncated || tree_depth > self.depth {
            return;
        }

        let include = should_include(node, self.mode, self.include_hidden);
        let child_indent = if include { indent + 1 } else { indent };
        if include {
            if self.node_count >= self.max_nodes {
                self.truncated = true;
                self.lines
                    .push(format!("{}... snapshot truncated", "  ".repeat(indent)));
                return;
            }
            self.node_count += 1;
            let line = format_node(self, node, "  ".repeat(indent));
            self.lines.push(line);
        }

        for child_id in &node.child_ids {
            if let Some(child) = nodes.get(child_id.as_str()) {
                self.format_node(child, nodes, child_indent, tree_depth + 1);
            }
        }
    }

    fn reference_for(&mut self, node: &RawAxNode) -> Option<String> {
        let backend_node_id = node.backend_node_id?;
        let key = NodeKey {
            frame_id: node.frame_id.clone(),
            document_revision: self.document_revision.to_string(),
            backend_node_id,
        };
        if let Some(reference) = self.tab.by_node.get(&key) {
            return Some(reference.clone());
        }

        *self.next_ref += 1;
        let reference = format!("e_{}", base36(*self.next_ref));
        let element = ElementReference {
            agent_session_id: self.agent_session_id.clone(),
            tab_id: self.tab_id.clone(),
            target_id: self.target_id.to_string(),
            frame_id: node.frame_id.clone(),
            document_revision: self.document_revision.to_string(),
            backend_node_id,
            role: node.role.clone(),
            name: node.name.clone(),
        };
        self.tab.by_node.insert(key, reference.clone());
        self.tab.by_ref.insert(reference.clone(), element);
        Some(reference)
    }
}

fn format_node(formatter: &mut SnapshotFormatter<'_>, node: &RawAxNode, indent: String) -> String {
    let role = if node.role.is_empty() {
        "node"
    } else {
        node.role.as_str()
    };
    let mut parts = vec![format!("{indent}- {role}")];
    if !node.name.is_empty() {
        parts.push(quoted(&node.name));
    }
    if let Some(reference) = formatter.reference_for(node) {
        parts.push(format!("[ref={reference}]"));
    }
    if let Some(value) = &node.value
        && !value.is_empty()
        && value != &node.name
    {
        parts.push(format!("value={}", quoted(value)));
    }
    for (name, value) in &node.properties {
        if matches!(
            name.as_str(),
            "disabled" | "checked" | "expanded" | "selected" | "required"
        ) {
            parts.push(format!("[{name}={}]", compact(value)));
        }
    }
    if formatter.include_bounds
        && let Some(bounds) = &node.bounds
    {
        parts.push(format!("[bounds={bounds}]"));
    }
    parts.join(" ")
}

fn should_include(node: &RawAxNode, mode: SnapshotMode, include_hidden: bool) -> bool {
    if node.ignored && !include_hidden {
        return false;
    }
    let role = node.role.to_ascii_lowercase();
    let interactive = matches!(
        role.as_str(),
        "button"
            | "checkbox"
            | "combobox"
            | "link"
            | "listbox"
            | "menuitem"
            | "option"
            | "radio"
            | "searchbox"
            | "slider"
            | "spinbutton"
            | "switch"
            | "tab"
            | "textbox"
            | "treeitem"
    );
    match mode {
        SnapshotMode::Interactive => interactive,
        SnapshotMode::Meaningful => {
            interactive
                || !node.name.is_empty()
                || matches!(
                    role.as_str(),
                    "article"
                        | "banner"
                        | "cell"
                        | "dialog"
                        | "document"
                        | "figure"
                        | "form"
                        | "heading"
                        | "img"
                        | "list"
                        | "listitem"
                        | "main"
                        | "navigation"
                        | "region"
                        | "row"
                        | "table"
                )
        }
        SnapshotMode::Full => true,
    }
}

fn snapshot_diff(previous: Option<&SnapshotResult>, current: &SnapshotResult) -> SnapshotDiff {
    let previous_lines = previous
        .map(|snapshot| snapshot_line_counts(&snapshot.tree))
        .unwrap_or_default();
    let current_lines = snapshot_line_counts(&current.tree);
    let mut changes = Vec::new();
    for (line, previous_count) in &previous_lines {
        let removed = previous_count.saturating_sub(*current_lines.get(line).unwrap_or(&0));
        changes.extend(std::iter::repeat_n(format!("- {line}"), removed));
    }
    for (line, current_count) in &current_lines {
        let added = current_count.saturating_sub(*previous_lines.get(line).unwrap_or(&0));
        changes.extend(std::iter::repeat_n(format!("+ {line}"), added));
    }
    changes.sort();
    let changed_node_count = changes.len();
    let truncated = changes.len() > 100;
    changes.truncate(100);
    SnapshotDiff {
        base_snapshot_id: previous.map(|snapshot| snapshot.snapshot_id.clone()),
        snapshot_id: current.snapshot_id.clone(),
        document_revision: current.document_revision.clone(),
        changes: changes.join("\n"),
        changed_node_count,
        truncated,
    }
}

fn snapshot_line_counts(tree: &str) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for line in tree.lines() {
        *counts.entry(line).or_insert(0) += 1;
    }
    counts
}

fn quoted(value: &str) -> String {
    serde_json::to_string(&compact(value)).expect("string serialization cannot fail")
}

fn compact(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prefixed_uuid(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut output = Vec::new();
    loop {
        output.push(DIGITS[(value % 36) as usize] as char);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    output.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_snapshot(loader: &str, button_name: &str) -> RawAxSnapshot {
        RawAxSnapshot {
            title: "Fixture".to_string(),
            url: "https://example.test/".to_string(),
            frames: vec![RawAxFrame {
                frame_id: "frame-main".to_string(),
                parent_frame_id: None,
                loader_id: loader.to_string(),
                url: "https://example.test/".to_string(),
                nodes: vec![
                    RawAxNode {
                        node_id: "root".to_string(),
                        parent_id: None,
                        child_ids: vec!["button".to_string()],
                        backend_node_id: Some(1),
                        frame_id: "frame-main".to_string(),
                        role: "WebArea".to_string(),
                        name: "Fixture".to_string(),
                        value: None,
                        properties: Vec::new(),
                        ignored: false,
                        bounds: Some("0.0,0.0,800.0,600.0".to_string()),
                    },
                    RawAxNode {
                        node_id: "button".to_string(),
                        parent_id: Some("root".to_string()),
                        child_ids: Vec::new(),
                        backend_node_id: Some(2),
                        frame_id: "frame-main".to_string(),
                        role: "button".to_string(),
                        name: button_name.to_string(),
                        value: None,
                        properties: vec![("disabled".to_string(), "false".to_string())],
                        ignored: false,
                        bounds: Some("20.0,20.0,80.0,30.0".to_string()),
                    },
                ],
            }],
        }
    }

    fn context<'a>(session: &'a AgentSessionId, tab: &'a TabId) -> SnapshotBuildContext<'a> {
        SnapshotBuildContext {
            agent_session_id: session,
            tab_id: tab,
            target_id: "target-a",
            mode: SnapshotMode::Meaningful,
            root_backend_node_id: None,
            depth: 8,
            max_nodes: 500,
            include_hidden: false,
            include_bounds: false,
        }
    }

    #[test]
    fn formats_compact_tree_and_reuses_reference_for_same_document_node() {
        let session = AgentSessionId("session_a".to_string());
        let tab = TabId("tab_a".to_string());
        let mut registry = ElementReferenceRegistry::new();
        let (first, _) = registry
            .build_snapshot(context(&session, &tab), raw_snapshot("loader-a", "Submit"))
            .unwrap();
        assert!(first.tree.contains("button \"Submit\" [ref=e_2]"));

        let (second, _) = registry
            .build_snapshot(context(&session, &tab), raw_snapshot("loader-a", "Submit"))
            .unwrap();
        assert!(second.tree.contains("[ref=e_2]"));
    }

    #[test]
    fn navigation_invalidates_prior_references() {
        let session = AgentSessionId("session_a".to_string());
        let tab = TabId("tab_a".to_string());
        let mut registry = ElementReferenceRegistry::new();
        registry
            .build_snapshot(context(&session, &tab), raw_snapshot("loader-a", "Submit"))
            .unwrap();
        registry
            .build_snapshot(
                context(&session, &tab),
                raw_snapshot("loader-b", "Continue"),
            )
            .unwrap();

        let error = registry
            .resolve(&session, &tab, "e_2", "loader-b")
            .unwrap_err();
        assert_eq!(
            error.code,
            crate::leases::BrowserToolErrorCode::ElementStale
        );
    }

    #[test]
    fn references_are_bound_to_the_issuing_session() {
        let owner = AgentSessionId("session_owner".to_string());
        let foreign = AgentSessionId("session_foreign".to_string());
        let tab = TabId("tab_a".to_string());
        let mut registry = ElementReferenceRegistry::new();
        registry
            .build_snapshot(context(&owner, &tab), raw_snapshot("loader-a", "Submit"))
            .unwrap();

        let error = registry
            .resolve(&foreign, &tab, "e_2", "loader-a")
            .unwrap_err();
        assert_eq!(
            error.code,
            crate::leases::BrowserToolErrorCode::ElementStale
        );
    }

    #[test]
    fn snapshot_root_scopes_tree_and_includes_bounds_on_request() {
        let session = AgentSessionId("session_a".to_string());
        let tab = TabId("tab_a".to_string());
        let mut registry = ElementReferenceRegistry::new();
        let mut options = context(&session, &tab);
        options.root_backend_node_id = Some(2);
        options.include_bounds = true;

        let (snapshot, _) = registry
            .build_snapshot(options, raw_snapshot("loader-a", "Submit"))
            .unwrap();

        assert!(snapshot.tree.contains("button \"Submit\""));
        assert!(snapshot.tree.contains("[bounds=20.0,20.0,80.0,30.0]"));
        assert!(!snapshot.tree.contains("WebArea"));
    }

    #[test]
    fn hidden_accessibility_nodes_require_include_hidden() {
        let session = AgentSessionId("session_a".to_string());
        let tab = TabId("tab_a".to_string());
        let mut raw = raw_snapshot("loader-a", "Submit");
        raw.frames[0].nodes[0].child_ids.push("hidden".to_string());
        raw.frames[0].nodes.push(RawAxNode {
            node_id: "hidden".to_string(),
            parent_id: Some("root".to_string()),
            child_ids: Vec::new(),
            backend_node_id: Some(3),
            frame_id: "frame-main".to_string(),
            role: "button".to_string(),
            name: "Hidden action".to_string(),
            value: None,
            properties: Vec::new(),
            ignored: true,
            bounds: None,
        });
        let mut registry = ElementReferenceRegistry::new();

        let (without_hidden, _) = registry
            .build_snapshot(context(&session, &tab), raw.clone())
            .unwrap();
        let mut options = context(&session, &tab);
        options.include_hidden = true;
        let (with_hidden, _) = registry.build_snapshot(options, raw).unwrap();

        assert!(!without_hidden.tree.contains("Hidden action"));
        assert!(with_hidden.tree.contains("Hidden action"));
    }

    #[test]
    fn snapshot_diff_reports_semantic_changes() {
        let session = AgentSessionId("session_a".to_string());
        let tab = TabId("tab_a".to_string());
        let mut registry = ElementReferenceRegistry::new();
        registry
            .build_snapshot(context(&session, &tab), raw_snapshot("loader-a", "Submit"))
            .unwrap();
        let (_, diff) = registry
            .build_snapshot(context(&session, &tab), raw_snapshot("loader-a", "Saved"))
            .unwrap();
        assert!(diff.changes.contains("Submit"));
        assert!(diff.changes.contains("Saved"));
    }

    #[test]
    fn snapshot_diff_preserves_duplicate_line_counts() {
        let previous = SnapshotResult {
            snapshot_id: "snapshot_previous".to_string(),
            document_revision: "loader-a".to_string(),
            url: "https://example.test/".to_string(),
            title: "Fixture".to_string(),
            tree: "row\nrow".to_string(),
            node_count: 2,
            truncated: false,
        };
        let current = SnapshotResult {
            snapshot_id: "snapshot_current".to_string(),
            tree: "row".to_string(),
            node_count: 1,
            ..previous.clone()
        };

        let diff = snapshot_diff(Some(&previous), &current);

        assert_eq!(diff.changes, "- row");
        assert_eq!(diff.changed_node_count, 1);
    }
}
