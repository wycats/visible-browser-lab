//! Read-only V8 heap-snapshot graph used by the MCP memory domain.

use std::collections::{BTreeMap, HashMap, VecDeque};

use petgraph::{
    Direction,
    algo::dominators::{Dominators, simple_fast},
    graph::{DiGraph, NodeIndex},
    visit::EdgeRef,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::leases::BrowserToolError;

#[derive(Debug, Deserialize)]
struct RawSnapshot {
    snapshot: SnapshotHeader,
    nodes: Vec<u64>,
    edges: Vec<u64>,
    strings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SnapshotHeader {
    meta: SnapshotMeta,
}

#[derive(Debug, Deserialize)]
struct SnapshotMeta {
    node_fields: Vec<String>,
    node_types: Vec<Value>,
    edge_fields: Vec<String>,
    edge_types: Vec<Value>,
}

#[derive(Debug, Clone)]
struct HeapNode {
    id: u64,
    node_type: String,
    name: String,
    self_size: u64,
    edge_count: usize,
}

#[derive(Debug, Clone)]
struct HeapEdge {
    edge_type: String,
    name: String,
}

pub struct HeapGraph {
    graph: DiGraph<HeapNode, HeapEdge>,
    by_id: HashMap<u64, NodeIndex>,
    dominators: Dominators<NodeIndex>,
    retained_sizes: Vec<u64>,
}

impl HeapGraph {
    pub fn parse(bytes: &[u8]) -> Result<Self, BrowserToolError> {
        let raw: RawSnapshot = serde_json::from_slice(bytes).map_err(|error| {
            BrowserToolError::artifact_error(format!("invalid V8 heap snapshot: {error}"))
        })?;
        let node_width = raw.snapshot.meta.node_fields.len();
        let edge_width = raw.snapshot.meta.edge_fields.len();
        if node_width == 0 || edge_width == 0 || !raw.nodes.len().is_multiple_of(node_width) {
            return Err(BrowserToolError::artifact_error(
                "heap snapshot has inconsistent node or edge metadata",
            ));
        }
        let node_field = |name: &str| {
            raw.snapshot
                .meta
                .node_fields
                .iter()
                .position(|field| field == name)
                .ok_or_else(|| {
                    BrowserToolError::artifact_error(format!(
                        "heap snapshot node metadata omits `{name}`"
                    ))
                })
        };
        let edge_field = |name: &str| {
            raw.snapshot
                .meta
                .edge_fields
                .iter()
                .position(|field| field == name)
                .ok_or_else(|| {
                    BrowserToolError::artifact_error(format!(
                        "heap snapshot edge metadata omits `{name}`"
                    ))
                })
        };
        let type_offset = node_field("type")?;
        let name_offset = node_field("name")?;
        let id_offset = node_field("id")?;
        let size_offset = node_field("self_size")?;
        let edge_count_offset = node_field("edge_count")?;
        let edge_type_offset = edge_field("type")?;
        let edge_name_offset = edge_field("name_or_index")?;
        let edge_target_offset = edge_field("to_node")?;
        let node_types = raw
            .snapshot
            .meta
            .node_types
            .get(type_offset)
            .ok_or_else(|| {
                BrowserToolError::artifact_error(
                    "heap snapshot node type metadata does not match node fields",
                )
            })?
            .as_array()
            .ok_or_else(|| BrowserToolError::artifact_error("heap node types are not an array"))?;
        let edge_types = raw
            .snapshot
            .meta
            .edge_types
            .get(edge_type_offset)
            .ok_or_else(|| {
                BrowserToolError::artifact_error(
                    "heap snapshot edge type metadata does not match edge fields",
                )
            })?
            .as_array()
            .ok_or_else(|| BrowserToolError::artifact_error("heap edge types are not an array"))?;

        let mut graph = DiGraph::new();
        let mut by_id = HashMap::new();
        for row in raw.nodes.chunks_exact(node_width) {
            let node_type = indexed_string(node_types, row[type_offset] as usize, "unknown");
            let name = raw
                .strings
                .get(row[name_offset] as usize)
                .cloned()
                .unwrap_or_default();
            let id = row[id_offset];
            let index = graph.add_node(HeapNode {
                id,
                node_type,
                name,
                self_size: row[size_offset],
                edge_count: row[edge_count_offset] as usize,
            });
            by_id.insert(id, index);
        }

        let mut edge_cursor = 0;
        for source in graph.node_indices().collect::<Vec<_>>() {
            let count = graph[source].edge_count;
            for _ in 0..count {
                let row = raw
                    .edges
                    .get(edge_cursor..edge_cursor + edge_width)
                    .ok_or_else(|| {
                        BrowserToolError::artifact_error("heap edge array is truncated")
                    })?;
                edge_cursor += edge_width;
                let target_ordinal = row[edge_target_offset] as usize / node_width;
                let target = NodeIndex::new(target_ordinal);
                if graph.node_weight(target).is_none() {
                    continue;
                }
                let edge_type =
                    indexed_string(edge_types, row[edge_type_offset] as usize, "unknown");
                let name = if matches!(edge_type.as_str(), "element" | "hidden") {
                    row[edge_name_offset].to_string()
                } else {
                    raw.strings
                        .get(row[edge_name_offset] as usize)
                        .cloned()
                        .unwrap_or_default()
                };
                graph.add_edge(source, target, HeapEdge { edge_type, name });
            }
        }
        if graph.node_count() == 0 {
            return Err(BrowserToolError::artifact_error(
                "heap snapshot contains no nodes",
            ));
        }
        let root = NodeIndex::new(0);
        let dominators = simple_fast(&graph, root);
        let mut retained_sizes = graph
            .node_indices()
            .map(|index| graph[index].self_size)
            .collect::<Vec<_>>();
        let mut depth_nodes = graph.node_indices().collect::<Vec<_>>();
        depth_nodes.sort_by_key(|node| std::cmp::Reverse(dominator_depth(&dominators, *node)));
        for node in depth_nodes {
            if node == root {
                continue;
            }
            if let Some(parent) = dominators.immediate_dominator(node) {
                retained_sizes[parent.index()] =
                    retained_sizes[parent.index()].saturating_add(retained_sizes[node.index()]);
            }
        }
        Ok(Self {
            graph,
            by_id,
            dominators,
            retained_sizes,
        })
    }

    pub fn summary(&self) -> Value {
        let total_self_size = self
            .graph
            .node_indices()
            .map(|node| self.graph[node].self_size)
            .sum::<u64>();
        json!({
            "node_count":self.graph.node_count(),
            "edge_count":self.graph.edge_count(),
            "total_self_size":total_self_size,
            "root":self.node_value(NodeIndex::new(0))
        })
    }

    pub fn classes(&self, class_filter: Option<&str>, min_retained: u64) -> Vec<Value> {
        let mut groups = BTreeMap::<String, (u64, u64, usize)>::new();
        for node in self.graph.node_indices() {
            let value = &self.graph[node];
            if class_filter.is_some_and(|filter| !value.name.contains(filter)) {
                continue;
            }
            let group = groups.entry(value.name.clone()).or_default();
            group.0 = group.0.saturating_add(value.self_size);
            group.1 = group.1.saturating_add(self.retained_sizes[node.index()]);
            group.2 += 1;
        }
        let mut groups = groups
            .into_iter()
            .filter(|(_, (_, retained, _))| *retained >= min_retained)
            .map(|(name, (self_size, retained_size, count))| {
                json!({"name":name,"count":count,"self_size":self_size,"retained_size":retained_size})
            })
            .collect::<Vec<_>>();
        groups.sort_by_key(|group| std::cmp::Reverse(group["retained_size"].as_u64().unwrap_or(0)));
        groups
    }

    pub fn node(&self, node_id: &str) -> Result<Value, BrowserToolError> {
        Ok(self.node_value(self.index(node_id)?))
    }

    pub fn dominators(&self, node_id: Option<&str>) -> Result<Vec<Value>, BrowserToolError> {
        if let Some(node_id) = node_id {
            let mut current = self.index(node_id)?;
            let mut result = Vec::new();
            while let Some(parent) = self.dominators.immediate_dominator(current) {
                result.push(self.node_value(parent));
                if parent == current || parent.index() == 0 {
                    break;
                }
                current = parent;
            }
            return Ok(result);
        }
        let mut nodes = self.graph.node_indices().collect::<Vec<_>>();
        nodes.sort_by_key(|node| std::cmp::Reverse(self.retained_sizes[node.index()]));
        Ok(nodes
            .into_iter()
            .map(|node| self.node_value(node))
            .collect())
    }

    pub fn retainers(&self, node_id: &str) -> Result<Vec<Value>, BrowserToolError> {
        let node = self.index(node_id)?;
        Ok(self
            .graph
            .edges_directed(node, Direction::Incoming)
            .map(|edge| self.edge_value(edge.source(), edge.target(), edge.weight()))
            .collect())
    }

    pub fn edges(&self, node_id: &str, incoming: bool) -> Result<Vec<Value>, BrowserToolError> {
        let node = self.index(node_id)?;
        let direction = if incoming {
            Direction::Incoming
        } else {
            Direction::Outgoing
        };
        Ok(self
            .graph
            .edges_directed(node, direction)
            .map(|edge| self.edge_value(edge.source(), edge.target(), edge.weight()))
            .collect())
    }

    pub fn retaining_paths(
        &self,
        node_id: &str,
        max_depth: usize,
        limit: usize,
    ) -> Result<Vec<Value>, BrowserToolError> {
        let start = self.index(node_id)?;
        let root = NodeIndex::new(0);
        let mut queue = VecDeque::from([(start, vec![start])]);
        let mut paths = Vec::new();
        while let Some((node, path)) = queue.pop_front() {
            if node == root {
                paths.push(Value::Array(
                    path.iter()
                        .rev()
                        .map(|node| self.node_value(*node))
                        .collect(),
                ));
                if paths.len() >= limit {
                    break;
                }
                continue;
            }
            if path.len() > max_depth {
                continue;
            }
            for parent in self.graph.neighbors_directed(node, Direction::Incoming) {
                if !path.contains(&parent) {
                    let mut next = path.clone();
                    next.push(parent);
                    queue.push_back((parent, next));
                }
            }
        }
        Ok(paths)
    }

    fn index(&self, node_id: &str) -> Result<NodeIndex, BrowserToolError> {
        let id = node_id
            .strip_prefix("node_")
            .unwrap_or(node_id)
            .parse::<u64>()
            .map_err(|_| {
                BrowserToolError::invalid_input(format!("invalid heap node `{node_id}`"))
            })?;
        self.by_id.get(&id).copied().ok_or_else(|| {
            BrowserToolError::invalid_input(format!("unknown heap node `{node_id}`"))
        })
    }

    fn node_value(&self, index: NodeIndex) -> Value {
        let node = &self.graph[index];
        json!({
            "node_id":format!("node_{}",node.id),
            "type":node.node_type,
            "name":node.name,
            "self_size":node.self_size,
            "retained_size":self.retained_sizes[index.index()],
            "edge_count":node.edge_count
        })
    }

    fn edge_value(&self, source: NodeIndex, target: NodeIndex, edge: &HeapEdge) -> Value {
        json!({
            "type":edge.edge_type,
            "name":edge.name,
            "from":format!("node_{}",self.graph[source].id),
            "to":format!("node_{}",self.graph[target].id)
        })
    }
}

fn indexed_string(values: &[Value], index: usize, fallback: &str) -> String {
    values
        .get(index)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn dominator_depth(dominators: &Dominators<NodeIndex>, node: NodeIndex) -> usize {
    dominators
        .dominators(node)
        .map(Iterator::count)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::leases::BrowserToolErrorCode;

    fn snapshot(node_types: Value, edge_types: Value, nodes: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "snapshot": {"meta": {
                "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                "node_types": node_types,
                "edge_fields": ["type", "name_or_index", "to_node"],
                "edge_types": edge_types
            }},
            "nodes": nodes,
            "edges": [],
            "strings": [""]
        }))
        .unwrap()
    }

    #[test]
    fn malformed_type_metadata_returns_artifact_error() {
        let bytes = snapshot(json!([]), json!([]), json!([0, 0, 1, 0, 0]));

        let error = HeapGraph::parse(&bytes).err().unwrap();

        assert_eq!(error.code, BrowserToolErrorCode::ArtifactError);
        assert!(error.message.contains("node type metadata"));
    }

    #[test]
    fn empty_snapshot_returns_artifact_error() {
        let bytes = snapshot(
            json!([["hidden"], "string", "number", "number", "number"]),
            json!([["context"], "string_or_number", "node"]),
            json!([]),
        );

        let error = HeapGraph::parse(&bytes).err().unwrap();

        assert_eq!(error.code, BrowserToolErrorCode::ArtifactError);
        assert!(error.message.contains("contains no nodes"));
    }
}
