//! `arlineo impact` (`SPEC.md`, "impact").

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::Result;
use crate::model::{Confidence, EdgeKind, Node, NodeId, NodeKind};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;
use crate::query::fnq::find_nodes;
use serde_json::{Map, Value};

/// What a change to `target` can reach: transitive callers up to `depth`, routes handled by
/// them, API calls reaching those routes, and tables they write, grouped by the weakest
/// confidence on the path.
pub fn run(view: &View, target: &str, depth: u32) -> Result<Report> {
    let starts = find_nodes(view, target, &NodeKind::ALL)?;
    let reached = walk_backwards(view, &starts, depth);
    let mut lines: Vec<String> = Vec::new();
    let mut start_items: Vec<Value> = Vec::new();
    for id in &starts {
        let Some(node) = view.graph.node(*id) else {
            continue;
        };
        lines.push(format!(
            "target {} {} {}",
            node.kind.as_str(),
            node.key,
            node_location(node)
        ));
        start_items.push(node_item(node, None, None));
    }
    if starts.is_empty() {
        lines.push(format!("no nodes found for {target}"));
    }
    let (group_lines, groups, flat) = grouped(view, &reached);
    lines.extend(group_lines);
    if !starts.is_empty() && reached.is_empty() {
        lines.push("no reachable nodes".to_owned());
    }
    let mut root = Map::new();
    root.insert("target".to_owned(), Value::String(target.to_owned()));
    root.insert("starts".to_owned(), Value::Array(start_items));
    root.insert("groups".to_owned(), Value::Array(groups));
    root.insert("reached".to_owned(), Value::Array(flat));
    Ok(Report {
        command: "impact".to_owned(),
        lines,
        json: Value::Object(root),
        status: ReportStatus::Ok,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reached {
    confidence: Confidence,
    depth: u32,
}

/// Breadth-first walk over incoming edges. Each node keeps every `(confidence, depth)` label
/// that no other label beats on both, so a stronger but deeper path cannot hide a weaker,
/// shallower one that still has depth left; the node is reported with its strongest label.
fn walk_backwards(view: &View, starts: &[NodeId], depth: u32) -> HashMap<NodeId, Reached> {
    let mut start_set: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, Confidence, u32)> = VecDeque::new();
    for id in starts {
        if start_set.insert(*id) {
            queue.push_back((*id, Confidence::Exact, 0));
        }
    }
    let mut labels: HashMap<NodeId, Vec<(Confidence, u32)>> = HashMap::new();
    while let Some((id, confidence, level)) = queue.pop_front() {
        // Drop stale queue entries whose label was superseded.
        let current = labels
            .get(&id)
            .is_some_and(|list| list.contains(&(confidence, level)));
        if !start_set.contains(&id) && !current {
            continue;
        }
        if level >= depth {
            continue;
        }
        let Some(next_level) = level.checked_add(1) else {
            continue;
        };
        let mut edges: Vec<&crate::model::Edge> = view
            .graph
            .incoming(id)
            .filter(|edge| {
                matches!(
                    edge.kind,
                    EdgeKind::Calls | EdgeKind::Handles | EdgeKind::Requests | EdgeKind::Writes
                )
            })
            .collect();
        edges.sort_by(|a, b| {
            node_key(view, a.from)
                .cmp(node_key(view, b.from))
                .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.confidence.cmp(&b.confidence))
        });
        for edge in edges {
            let neighbor = edge.from;
            if start_set.contains(&neighbor) {
                continue;
            }
            let candidate = (confidence.weakest(edge.confidence), next_level);
            let list = labels.entry(neighbor).or_default();
            if list
                .iter()
                .any(|(c, d)| *c <= candidate.0 && *d <= candidate.1)
            {
                continue;
            }
            list.retain(|(c, d)| !(candidate.0 <= *c && candidate.1 <= *d));
            list.push(candidate);
            queue.push_back((neighbor, candidate.0, candidate.1));
        }
    }
    // Confidence orders strongest first, so the smallest label is the strongest path,
    // the shallowest one on a tie.
    labels
        .into_iter()
        .filter_map(|(id, list)| {
            list.into_iter()
                .min()
                .map(|(confidence, depth)| (id, Reached { confidence, depth }))
        })
        .collect()
}

fn grouped(
    view: &View,
    reached: &HashMap<NodeId, Reached>,
) -> (Vec<String>, Vec<Value>, Vec<Value>) {
    let mut entries: Vec<(NodeId, Reached)> =
        reached.iter().map(|(id, value)| (*id, *value)).collect();
    entries.sort_by(|a, b| {
        a.1.confidence
            .cmp(&b.1.confidence)
            .then_with(|| node_key(view, a.0).cmp(node_key(view, b.0)))
            .then_with(|| kind_rank(node_kind(view, a.0)).cmp(&kind_rank(node_kind(view, b.0))))
            .then_with(|| a.1.depth.cmp(&b.1.depth))
            .then_with(|| a.0.raw().cmp(&b.0.raw()))
    });
    let mut lines: Vec<String> = Vec::new();
    let mut groups: Vec<Value> = Vec::new();
    let mut flat: Vec<Value> = Vec::new();
    for confidence in [
        Confidence::Exact,
        Confidence::UniqueName,
        Confidence::Heuristic,
        Confidence::Guessed,
    ] {
        let mut group_items: Vec<Value> = Vec::new();
        for (id, value) in entries
            .iter()
            .filter(|(_, value)| value.confidence == confidence)
        {
            let Some(node) = view.graph.node(*id) else {
                continue;
            };
            lines.push(format!(
                "{} {} {} {} depth {}",
                confidence.as_str(),
                node.kind.as_str(),
                node.key,
                node_location(node),
                value.depth
            ));
            let item = node_item(node, Some(confidence), Some(value.depth));
            group_items.push(item.clone());
            flat.push(item);
        }
        if group_items.is_empty() {
            continue;
        }
        // Group header precedes its members in text; rebuild the lines for
        // this group in header-first order.
        let member_count = group_items.len();
        let first_member = lines.len().saturating_sub(member_count);
        let members: Vec<String> = lines.split_off(first_member);
        lines.push(format!("{}:", confidence.as_str()));
        for member in members {
            // Drop the leading confidence word now carried by the header.
            match member.split_once(' ') {
                Some((_, rest)) => lines.push(format!("  {rest}")),
                None => lines.push(format!("  {member}")),
            }
        }
        let mut group = Map::new();
        group.insert(
            "confidence".to_owned(),
            Value::String(confidence.as_str().to_owned()),
        );
        group.insert("nodes".to_owned(), Value::Array(group_items));
        groups.push(Value::Object(group));
    }
    (lines, groups, flat)
}

fn node_item(node: &Node, confidence: Option<Confidence>, depth: Option<u32>) -> Value {
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
    if let Some(confidence) = confidence {
        item.insert(
            "confidence".to_owned(),
            Value::String(confidence.as_str().to_owned()),
        );
    }
    if let Some(depth) = depth {
        item.insert("depth".to_owned(), Value::from(depth));
    }
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

fn node_kind(view: &View, id: NodeId) -> Option<NodeKind> {
    view.graph.node(id).map(|node| node.kind)
}

fn kind_rank(kind: Option<NodeKind>) -> usize {
    let mut rank: usize = 0;
    for candidate in NodeKind::ALL {
        if Some(candidate) == kind {
            return rank;
        }
        rank = rank.saturating_add(1);
    }
    rank
}
