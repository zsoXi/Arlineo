//! `arlineo graph` (`SPEC.md`, "graph").

use std::collections::{HashSet, VecDeque};

use crate::error::Result;
use crate::model::{Confidence, EdgeKind, Node, NodeId, NodeKind};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;
use crate::query::fnq::find_nodes;
use serde_json::{Map, Value};

const MAX_PRINTED: usize = 2000;

/// Breadth-first walk from the nodes matching `from`: `up` levels of incoming edges and
/// `down` levels of outgoing edges, each step with edge kind and confidence.
pub fn run(view: &View, from: &str, up: u32, down: u32) -> Result<Report> {
    let roots = find_nodes(view, from, &NodeKind::ALL)?;
    let mut lines: Vec<String> = Vec::new();
    let mut root = Map::new();
    root.insert("from".to_owned(), Value::String(from.to_owned()));
    if roots.is_empty() {
        lines.push(format!("no nodes found for {from}"));
        root.insert("up".to_owned(), Value::Array(Vec::new()));
        root.insert("down".to_owned(), Value::Array(Vec::new()));
        return Ok(Report {
            command: "graph".to_owned(),
            lines,
            json: Value::Object(root),
            status: ReportStatus::Ok,
        });
    }
    let mut printed: usize = 0;
    let mut truncated = false;
    let (up_arena, up_roots) =
        build_direction(view, &roots, up, true, &mut printed, &mut truncated);
    let (down_arena, down_roots) =
        build_direction(view, &roots, down, false, &mut printed, &mut truncated);
    lines.push("up:".to_owned());
    push_forest_lines(view, &up_arena, &up_roots, &mut lines);
    lines.push("down:".to_owned());
    push_forest_lines(view, &down_arena, &down_roots, &mut lines);
    if truncated {
        lines.push("... truncated".to_owned());
    }
    root.insert("up".to_owned(), forest_json(view, &up_arena, &up_roots));
    root.insert(
        "down".to_owned(),
        forest_json(view, &down_arena, &down_roots),
    );
    root.insert("truncated".to_owned(), Value::Bool(truncated));
    Ok(Report {
        command: "graph".to_owned(),
        lines,
        json: Value::Object(root),
        status: ReportStatus::Ok,
    })
}

#[derive(Clone, Debug)]
struct TreeNode {
    id: NodeId,
    edge: Option<EdgeKind>,
    confidence: Option<Confidence>,
    level: u32,
    children: Vec<usize>,
}

fn build_direction(
    view: &View,
    roots: &[NodeId],
    max_depth: u32,
    go_up: bool,
    printed: &mut usize,
    truncated: &mut bool,
) -> (Vec<TreeNode>, Vec<usize>) {
    let mut arena: Vec<TreeNode> = Vec::new();
    let mut root_indices: Vec<usize> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for root in roots {
        if *printed >= MAX_PRINTED {
            *truncated = true;
            break;
        }
        if !visited.insert(*root) {
            continue;
        }
        let index = arena.len();
        arena.push(TreeNode {
            id: *root,
            edge: None,
            confidence: None,
            level: 0,
            children: Vec::new(),
        });
        root_indices.push(index);
        queue.push_back(index);
        *printed = printed.saturating_add(1);
    }
    while let Some(index) = queue.pop_front() {
        let Some((id, level)) = arena.get(index).map(|tree| (tree.id, tree.level)) else {
            continue;
        };
        if level >= max_depth {
            continue;
        }
        let Some(next_level) = level.checked_add(1) else {
            continue;
        };
        let mut steps: Vec<(NodeId, EdgeKind, Confidence, u32)> = Vec::new();
        if go_up {
            for edge in view.graph.incoming(id) {
                steps.push((edge.from, edge.kind, edge.confidence, edge.line));
            }
        } else {
            for edge in view.graph.outgoing(id) {
                steps.push((edge.to, edge.kind, edge.confidence, edge.line));
            }
        }
        steps.sort_by(|a, b| {
            node_key(view, a.0)
                .cmp(node_key(view, b.0))
                .then_with(|| a.1.as_str().cmp(b.1.as_str()))
                .then_with(|| a.3.cmp(&b.3))
                .then_with(|| a.2.cmp(&b.2))
        });
        for (neighbor, edge_kind, confidence, _line) in steps {
            if visited.contains(&neighbor) {
                continue;
            }
            if *printed >= MAX_PRINTED {
                *truncated = true;
                queue.clear();
                break;
            }
            visited.insert(neighbor);
            let child = arena.len();
            arena.push(TreeNode {
                id: neighbor,
                edge: Some(edge_kind),
                confidence: Some(confidence),
                level: next_level,
                children: Vec::new(),
            });
            if let Some(parent) = arena.get_mut(index) {
                parent.children.push(child);
            }
            queue.push_back(child);
            *printed = printed.saturating_add(1);
        }
    }
    (arena, root_indices)
}

fn push_forest_lines(view: &View, arena: &[TreeNode], roots: &[usize], lines: &mut Vec<String>) {
    // Iterative pre-order traversal; children were added in sorted order.
    let mut stack: Vec<usize> = Vec::new();
    for root in roots.iter().rev() {
        stack.push(*root);
    }
    while let Some(index) = stack.pop() {
        let Some(tree) = arena.get(index) else {
            continue;
        };
        let Some(node) = view.graph.node(tree.id) else {
            continue;
        };
        let indent_levels = match usize::try_from(tree.level) {
            Ok(value) => value.min(MAX_PRINTED),
            Err(_) => MAX_PRINTED,
        };
        let indent = "  ".repeat(indent_levels);
        let suffix = match (tree.edge, tree.confidence) {
            (Some(edge), Some(confidence)) => {
                format!(" {} {}", edge.as_str(), confidence.as_str())
            }
            _ => String::new(),
        };
        lines.push(format!(
            "{}{} {} {}{}",
            indent,
            node.kind.as_str(),
            node.key,
            node_location(node),
            suffix
        ));
        for child in tree.children.iter().rev() {
            stack.push(*child);
        }
    }
}

fn forest_json(view: &View, arena: &[TreeNode], roots: &[usize]) -> Value {
    let mut slots: Vec<Option<Value>> = Vec::new();
    for _tree in arena {
        slots.push(None);
    }
    // Children always follow their parent in the arena, so building in
    // reverse order assembles every subtree before its parent.
    let mut indices: Vec<usize> = (0..arena.len()).collect();
    indices.reverse();
    for index in indices {
        let Some(tree) = arena.get(index) else {
            continue;
        };
        let mut children: Vec<Value> = Vec::new();
        for child in &tree.children {
            if let Some(slot) = slots.get_mut(*child) {
                if let Some(value) = slot.take() {
                    children.push(value);
                }
            }
        }
        let Some(node) = view.graph.node(tree.id) else {
            continue;
        };
        let mut item = Map::new();
        item.insert("node".to_owned(), node_item(node));
        item.insert(
            "edge".to_owned(),
            match tree.edge {
                Some(edge) => Value::String(edge.as_str().to_owned()),
                None => Value::Null,
            },
        );
        item.insert(
            "confidence".to_owned(),
            match tree.confidence {
                Some(confidence) => Value::String(confidence.as_str().to_owned()),
                None => Value::Null,
            },
        );
        item.insert("children".to_owned(), Value::Array(children));
        if let Some(slot) = slots.get_mut(index) {
            *slot = Some(Value::Object(item));
        }
    }
    let mut forest: Vec<Value> = Vec::new();
    for root in roots {
        if let Some(slot) = slots.get_mut(*root) {
            if let Some(value) = slot.take() {
                forest.push(value);
            }
        }
    }
    Value::Array(forest)
}

fn node_item(node: &Node) -> Value {
    let mut item = Map::new();
    item.insert(
        "kind".to_owned(),
        Value::String(node.kind.as_str().to_owned()),
    );
    item.insert("key".to_owned(), Value::String(node.key.clone()));
    item.insert(
        "file".to_owned(),
        match &node.file {
            Some(file) => Value::String(file.as_str().to_owned()),
            None => Value::Null,
        },
    );
    item.insert("line".to_owned(), Value::from(node.line));
    Value::Object(item)
}

fn node_location(node: &Node) -> String {
    match &node.file {
        Some(file) => format!("{}:{}", file, node.line),
        None => format!("line {}", node.line),
    }
}

fn node_key(view: &View, id: NodeId) -> &str {
    match view.graph.node(id) {
        Some(node) => node.key.as_str(),
        None => "",
    }
}
