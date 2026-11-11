//! `arlineo fn` (`SPEC.md`, "fn").

use crate::error::Result;
use crate::model::{EdgeKind, Fact, FactKind, Node, NodeId, NodeKind};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;
use crate::util::glob::Glob;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Fact kinds listed under a function as its own facts.
const OWNED: [FactKind; 5] = [
    FactKind::Route,
    FactKind::ApiCall,
    FactKind::EnvVar,
    FactKind::SqlInCode,
    FactKind::TableAccess,
];

/// Call facts and `OWNED` facts grouped by their `owner` field, built once per command so
/// each shown function reads only its own facts.
type FactsByOwner<'a> = BTreeMap<String, Vec<&'a Fact>>;

fn facts_by_owner(view: &View) -> FactsByOwner<'_> {
    let mut by_owner: FactsByOwner<'_> = BTreeMap::new();
    for kind in std::iter::once(FactKind::Call).chain(OWNED) {
        for fact in view.fact_index.of_kind(&view.facts, kind) {
            if let Some(owner) = fact.data_text("owner") {
                by_owner.entry(owner).or_default().push(fact);
            }
        }
    }
    by_owner
}

/// Finds nodes among `kinds` whose key matches `name`.
///
/// An exact key match wins. Otherwise, when `name` contains `*`, it is a glob
/// over keys (`SPEC.md`, "Globs": `*` stops at `/`, `**` crosses it); otherwise the
/// last key segment (segments split on `::` and `.`) must equal `name`. The result is
/// sorted by key, then by node kind. `Error::Config` for an invalid glob.
pub(crate) fn find_nodes(view: &View, name: &str, kinds: &[NodeKind]) -> Result<Vec<NodeId>> {
    let mut ids: Vec<NodeId> = Vec::new();
    if kinds.is_empty() {
        return Ok(ids);
    }
    for kind in kinds {
        if let Some(id) = view.graph.lookup(*kind, name) {
            ids.push(id);
        }
    }
    if !ids.is_empty() {
        sort_ids(view, &mut ids);
        return Ok(ids);
    }
    let glob = if name.contains('*') {
        Some(Glob::new(name)?)
    } else {
        None
    };
    for (index, node) in view.graph.nodes().iter().enumerate() {
        if !kinds.contains(&node.kind) {
            continue;
        }
        let matched = match &glob {
            Some(glob) => glob.is_match(&node.key),
            None => last_segment(&node.key) == name,
        };
        if matched {
            if let Some(id) = NodeId::from_index(index) {
                ids.push(id);
            }
        }
    }
    sort_ids(view, &mut ids);
    Ok(ids)
}

/// Functions whose key or name matches `name` (exact key, `*` glob over keys, or plain
/// name): definition, callers, callees, and facts inside (calls, table accesses, SQL,
/// environment variables, API calls), at most `limit` functions.
pub fn run(view: &View, name: &str, limit: usize) -> Result<Report> {
    let ids = find_nodes(view, name, &[NodeKind::Fn, NodeKind::SqlFunction])?;
    let total = ids.len();
    let by_owner = facts_by_owner(view);
    let mut lines: Vec<String> = Vec::new();
    let mut functions: Vec<Value> = Vec::new();
    for id in ids.into_iter().take(limit) {
        let Some(node) = view.graph.node(id) else {
            continue;
        };
        let (item_lines, item) = function_report(view, id, node, &by_owner);
        lines.extend(item_lines);
        functions.push(item);
    }
    if functions.is_empty() {
        lines.push(format!("no functions found for {name}"));
    }
    let truncated = total > limit;
    if truncated {
        lines.push(format!(
            "... {} more (use --limit)",
            total.saturating_sub(limit)
        ));
    }
    let mut root = Map::new();
    root.insert("name".to_owned(), Value::String(name.to_owned()));
    root.insert("functions".to_owned(), Value::Array(functions));
    root.insert("truncated".to_owned(), Value::Bool(truncated));
    Ok(Report {
        command: "fn".to_owned(),
        lines,
        json: Value::Object(root),
        status: ReportStatus::Ok,
    })
}

fn function_report(
    view: &View,
    id: NodeId,
    node: &Node,
    by_owner: &FactsByOwner<'_>,
) -> (Vec<String>, Value) {
    let (is_pub, is_async, def_file, def_line) = definition_info(view, node);
    let mut lines: Vec<String> = Vec::new();
    let mut header = match &def_file {
        Some(file) => format!("{} {} {}:{}", node.kind.as_str(), node.key, file, def_line),
        None => format!("{} {} line {}", node.kind.as_str(), node.key, def_line),
    };
    if is_pub {
        header.push_str(" pub");
    }
    if is_async {
        header.push_str(" async");
    }
    lines.push(header);

    let (caller_lines, callers) = callers(view, id);
    lines.extend(caller_lines);
    let own_facts: &[&Fact] = by_owner.get(&node.key).map(Vec::as_slice).unwrap_or(&[]);
    let (callee_lines, callees) = callees(view, id, own_facts);
    lines.extend(callee_lines);
    let (fact_lines, facts) = owned_facts(own_facts);
    lines.extend(fact_lines);

    let mut item = Map::new();
    item.insert(
        "kind".to_owned(),
        Value::String(node.kind.as_str().to_owned()),
    );
    item.insert("key".to_owned(), Value::String(node.key.clone()));
    item.insert("file".to_owned(), file_value(def_file.as_deref()));
    item.insert("line".to_owned(), Value::from(def_line));
    item.insert("is_pub".to_owned(), Value::Bool(is_pub));
    item.insert("is_async".to_owned(), Value::Bool(is_async));
    item.insert("callers".to_owned(), Value::Array(callers));
    item.insert("callees".to_owned(), Value::Array(callees));
    item.insert("facts".to_owned(), Value::Array(facts));
    (lines, Value::Object(item))
}

fn definition_info(view: &View, node: &Node) -> (bool, bool, Option<String>, u32) {
    let fact_kind = match node.kind {
        NodeKind::SqlFunction => FactKind::SqlFunction,
        _ => FactKind::FnDef,
    };
    let fact = view
        .fact_index
        .of_kind(&view.facts, fact_kind)
        .find(|fact| fact.key == node.key);
    match fact {
        Some(fact) => (
            bool_field(fact, "is_pub"),
            bool_field(fact, "is_async"),
            Some(fact.file.as_str().to_owned()),
            fact.line,
        ),
        None => (
            false,
            false,
            node.file.as_ref().map(|file| file.as_str().to_owned()),
            node.line,
        ),
    }
}

fn bool_field(fact: &Fact, field: &str) -> bool {
    match fact.data.get(field) {
        Some(Value::Bool(value)) => *value,
        _ => false,
    }
}

fn callers(view: &View, id: NodeId) -> (Vec<String>, Vec<Value>) {
    let mut edges: Vec<&crate::model::Edge> = view
        .graph
        .incoming(id)
        .filter(|edge| matches!(edge.kind, EdgeKind::Calls | EdgeKind::Handles))
        .collect();
    edges.sort_by(|a, b| {
        node_key(view, a.from)
            .cmp(node_key(view, b.from))
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.confidence.cmp(&b.confidence))
            .then_with(|| a.to.raw().cmp(&b.to.raw()))
    });
    let mut lines: Vec<String> = vec!["  callers:".to_owned()];
    let mut items: Vec<Value> = Vec::new();
    for edge in edges {
        let Some(source) = view.graph.node(edge.from) else {
            continue;
        };
        let location = match &source.file {
            Some(file) => format!("{}:{}", file, edge.line),
            None => format!("line {}", edge.line),
        };
        lines.push(format!(
            "    {} {} {} {}",
            source.key,
            location,
            edge.kind.as_str(),
            edge.confidence.as_str()
        ));
        items.push(edge_item(
            source,
            edge.kind,
            edge.line,
            edge.confidence.as_str(),
            false,
        ));
    }
    if items.is_empty() {
        lines.push("    (none)".to_owned());
    }
    (lines, items)
}

fn callees(view: &View, id: NodeId, own_facts: &[&Fact]) -> (Vec<String>, Vec<Value>) {
    let mut edges: Vec<&crate::model::Edge> = view
        .graph
        .outgoing(id)
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    edges.sort_by(|a, b| {
        node_key(view, a.to)
            .cmp(node_key(view, b.to))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.confidence.cmp(&b.confidence))
            .then_with(|| a.from.raw().cmp(&b.from.raw()))
    });
    let mut lines: Vec<String> = vec!["  callees:".to_owned()];
    let mut items: Vec<Value> = Vec::new();
    for edge in edges {
        let Some(target) = view.graph.node(edge.to) else {
            continue;
        };
        lines.push(format!(
            "    {} {} {} {} at line {}",
            target.key,
            node_location(target),
            edge.kind.as_str(),
            edge.confidence.as_str(),
            edge.line
        ));
        items.push(edge_item(
            target,
            edge.kind,
            edge.line,
            edge.confidence.as_str(),
            false,
        ));
    }
    // Call facts of this owner that produced no calls edge at their line.
    let mut unresolved: Vec<&Fact> = own_facts
        .iter()
        .copied()
        .filter(|fact| fact.kind == FactKind::Call)
        .filter(|fact| {
            !view
                .graph
                .outgoing(id)
                .any(|edge| edge.kind == EdgeKind::Calls && edge.line == fact.line)
        })
        .collect();
    unresolved.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.key.cmp(&b.key))
    });
    for fact in unresolved {
        let callee = match fact.data_text("callee") {
            Some(text) => text,
            None => fact.key.clone(),
        };
        lines.push(format!(
            "    {} {}:{} unresolved",
            callee, fact.file, fact.line
        ));
        let mut item = Map::new();
        item.insert("kind".to_owned(), Value::String("call".to_owned()));
        item.insert("key".to_owned(), Value::String(callee));
        item.insert(
            "file".to_owned(),
            Value::String(fact.file.as_str().to_owned()),
        );
        item.insert("line".to_owned(), Value::from(fact.line));
        item.insert(
            "confidence".to_owned(),
            Value::String(fact.confidence.as_str().to_owned()),
        );
        item.insert("edge".to_owned(), Value::String("calls".to_owned()));
        item.insert("unresolved".to_owned(), Value::Bool(true));
        items.push(Value::Object(item));
    }
    if items.is_empty() {
        lines.push("    (none)".to_owned());
    }
    (lines, items)
}

fn owned_facts(own_facts: &[&Fact]) -> (Vec<String>, Vec<Value>) {
    let mut facts: Vec<&Fact> = own_facts
        .iter()
        .copied()
        .filter(|fact| OWNED.contains(&fact.kind))
        .collect();
    facts.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut lines: Vec<String> = vec!["  facts:".to_owned()];
    let mut items: Vec<Value> = Vec::new();
    for fact in facts {
        lines.push(format!(
            "    {} {} {}:{} {}",
            fact.kind.as_str(),
            fact.key,
            fact.file,
            fact.line,
            fact.confidence.as_str()
        ));
        let mut item = Map::new();
        item.insert(
            "kind".to_owned(),
            Value::String(fact.kind.as_str().to_owned()),
        );
        item.insert("key".to_owned(), Value::String(fact.key.clone()));
        item.insert(
            "file".to_owned(),
            Value::String(fact.file.as_str().to_owned()),
        );
        item.insert("line".to_owned(), Value::from(fact.line));
        item.insert(
            "confidence".to_owned(),
            Value::String(fact.confidence.as_str().to_owned()),
        );
        items.push(Value::Object(item));
    }
    if items.is_empty() {
        lines.push("    (none)".to_owned());
    }
    (lines, items)
}

fn edge_item(node: &Node, edge: EdgeKind, line: u32, confidence: &str, unresolved: bool) -> Value {
    let mut item = Map::new();
    item.insert(
        "kind".to_owned(),
        Value::String(node.kind.as_str().to_owned()),
    );
    item.insert("key".to_owned(), Value::String(node.key.clone()));
    item.insert(
        "file".to_owned(),
        file_value(node.file.as_ref().map(|file| file.as_str())),
    );
    item.insert("line".to_owned(), Value::from(line));
    item.insert(
        "confidence".to_owned(),
        Value::String(confidence.to_owned()),
    );
    item.insert("edge".to_owned(), Value::String(edge.as_str().to_owned()));
    item.insert("unresolved".to_owned(), Value::Bool(unresolved));
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

fn file_value(file: Option<&str>) -> Value {
    match file {
        Some(file) => Value::String(file.to_owned()),
        None => Value::Null,
    }
}

fn sort_ids(view: &View, ids: &mut [NodeId]) {
    ids.sort_by(|a, b| {
        let left = view.graph.node(*a);
        let right = view.graph.node(*b);
        match (left, right) {
            (Some(left), Some(right)) => left
                .key
                .cmp(&right.key)
                .then_with(|| kind_rank(left.kind).cmp(&kind_rank(right.kind)))
                .then_with(|| a.raw().cmp(&b.raw())),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.raw().cmp(&b.raw()),
        }
    });
}

fn kind_rank(kind: NodeKind) -> usize {
    let mut rank: usize = 0;
    for candidate in NodeKind::ALL {
        if candidate == kind {
            return rank;
        }
        rank = rank.saturating_add(1);
    }
    rank
}

/// The last segment of a key or handler text: after the last `::` or `.`.
pub(crate) fn last_segment(key: &str) -> &str {
    match key.rsplit([':', '.']).next() {
        Some(segment) => segment,
        None => key,
    }
}
