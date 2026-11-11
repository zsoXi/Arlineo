//! `arlineo table` (`SPEC.md`, "table").

use std::collections::BTreeMap;

use crate::error::Result;
use crate::extract::sql::table_key;
use crate::model::{EdgeKind, Fact, FactKind, NodeKind};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;

/// Everything about one table: definitions, columns, grants, triggers, and who reads and
/// writes it.
pub fn run(view: &View, table: &str) -> Result<Report> {
    let key = table_key(table);
    let table_facts: Vec<&Fact> = view
        .fact_index
        .of_kind(&view.facts, FactKind::Table)
        .filter(|fact| fact.key == key)
        .collect();
    let columns = collect_columns(view, &key);
    let grants: Vec<&Fact> = view
        .fact_index
        .of_kind(&view.facts, FactKind::Grant)
        .filter(|fact| data_is(fact, "object", &key))
        .collect();
    let triggers: Vec<&Fact> = view
        .fact_index
        .of_kind(&view.facts, FactKind::Trigger)
        .filter(|fact| data_is(fact, "table", &key) || has_key_prefix(fact, &key))
        .collect();
    let (readers, writers) = collect_accesses(view, &key);
    let found = !table_facts.is_empty()
        || !columns.is_empty()
        || !grants.is_empty()
        || !triggers.is_empty()
        || !readers.is_empty()
        || !writers.is_empty()
        || view.graph.lookup(NodeKind::Table, &key).is_some();
    if !found {
        return Ok(Report {
            command: "table".to_owned(),
            lines: vec![format!("no table {key}")],
            json: serde_json::json!({
                "key": key,
                "found": false,
                "table_facts": [],
                "columns": [],
                "grants": [],
                "triggers": [],
                "readers": [],
                "writers": [],
            }),
            status: ReportStatus::Ok,
        });
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push("table".to_owned());
    for fact in &table_facts {
        lines.push(fact_line(fact));
    }
    lines.push("columns".to_owned());
    let mut column_items: Vec<serde_json::Value> = Vec::new();
    for (column_key, acc) in &columns {
        let latest = acc.latest;
        let sql_type = match acc.typed {
            Some(fact) => fact.data_text("sql_type"),
            None => None,
        };
        let action = latest.data_text("action");
        let mut line = format!(
            "{}:{} column {} {}",
            latest.file,
            latest.line,
            column_key,
            text_or(action.clone(), "")
        );
        if let Some(sql_type) = &sql_type {
            line.push(' ');
            line.push_str(sql_type);
        }
        lines.push(line);
        column_items.push(serde_json::json!({
            "kind": latest.kind.as_str(),
            "key": column_key,
            "file": latest.file.as_str(),
            "line": latest.line,
            "confidence": latest.confidence.as_str(),
            "table": key,
            "column": column_name(latest, &key),
            "action": action,
            "sql_type": sql_type,
        }));
    }
    lines.push("grants".to_owned());
    for fact in &grants {
        lines.push(fact_line(fact));
    }
    lines.push("triggers".to_owned());
    for fact in &triggers {
        lines.push(fact_line(fact));
    }
    lines.push("readers".to_owned());
    for entry in &readers {
        lines.push(entry.line());
    }
    lines.push("writers".to_owned());
    for entry in &writers {
        lines.push(entry.line());
    }
    Ok(Report {
        command: "table".to_owned(),
        lines,
        json: serde_json::json!({
            "key": key,
            "found": true,
            "table_facts": table_facts.iter().map(|fact| fact_item(fact)).collect::<Vec<_>>(),
            "columns": column_items,
            "grants": grants.iter().map(|fact| fact_item(fact)).collect::<Vec<_>>(),
            "triggers": triggers.iter().map(|fact| fact_item(fact)).collect::<Vec<_>>(),
            "readers": readers.iter().map(|entry| entry.item(&key)).collect::<Vec<_>>(),
            "writers": writers.iter().map(|entry| entry.item(&key)).collect::<Vec<_>>(),
        }),
        status: ReportStatus::Ok,
    })
}

/// Latest facts for one column key.
struct ColumnAcc<'a> {
    latest: &'a Fact,
    /// Latest fact for the column that carries a type, if any.
    typed: Option<&'a Fact>,
}

/// One line per column key, in key order, keeping the latest fact by file then line.
fn collect_columns<'a>(view: &'a View, key: &str) -> BTreeMap<String, ColumnAcc<'a>> {
    let mut columns: BTreeMap<String, ColumnAcc<'a>> = BTreeMap::new();
    for fact in view.fact_index.of_kind(&view.facts, FactKind::Column) {
        let belongs = match fact.data_text("table") {
            Some(table) => table == key,
            None => has_key_prefix(fact, key),
        };
        if !belongs {
            continue;
        }
        match columns.get_mut(&fact.key) {
            Some(acc) => {
                if (fact.file.as_str(), fact.line) >= (acc.latest.file.as_str(), acc.latest.line) {
                    acc.latest = fact;
                }
                if fact.data_text("sql_type").is_some() {
                    let replace = match acc.typed {
                        Some(current) => {
                            (fact.file.as_str(), fact.line) >= (current.file.as_str(), current.line)
                        }
                        None => true,
                    };
                    if replace {
                        acc.typed = Some(fact);
                    }
                }
            }
            None => {
                let typed = if fact.data_text("sql_type").is_some() {
                    Some(fact)
                } else {
                    None
                };
                columns.insert(
                    fact.key.clone(),
                    ColumnAcc {
                        latest: fact,
                        typed,
                    },
                );
            }
        }
    }
    columns
}

/// A reader or writer of a table, from a `table_access` fact or a graph edge.
struct AccessEntry {
    owner: String,
    op: String,
    file: Option<String>,
    line: u32,
    confidence: String,
}

impl AccessEntry {
    /// Text line: `file:line`, owner, op and confidence, without a location when unknown.
    fn line(&self) -> String {
        match &self.file {
            Some(file) => format!(
                "{}:{} {} {} {}",
                file, self.line, self.owner, self.op, self.confidence
            ),
            None => format!("{} {} {}", self.owner, self.op, self.confidence),
        }
    }

    /// JSON item for this access.
    fn item(&self, table: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "table_access",
            "key": self.owner,
            "owner": self.owner,
            "table": table,
            "op": self.op,
            "file": self.file,
            "line": self.line,
            "confidence": self.confidence,
        })
    }
}

/// Readers and writers from `table_access` facts and incoming graph edges, deduplicated
/// by owner and op and sorted by owner, op, file and line.
fn collect_accesses(view: &View, key: &str) -> (Vec<AccessEntry>, Vec<AccessEntry>) {
    let mut readers: Vec<AccessEntry> = Vec::new();
    let mut writers: Vec<AccessEntry> = Vec::new();
    for fact in view.fact_index.of_kind(&view.facts, FactKind::TableAccess) {
        let belongs = match fact.data_text("table") {
            Some(table) => table == key,
            None => fact.key == key,
        };
        if !belongs {
            continue;
        }
        let (Some(owner), Some(op)) = (fact.data_text("owner"), fact.data_text("op")) else {
            continue;
        };
        let entry = AccessEntry {
            owner,
            op,
            file: Some(fact.file.as_str().to_owned()),
            line: fact.line,
            confidence: fact.confidence.as_str().to_owned(),
        };
        match entry.op.as_str() {
            "select" | "select_for_update" => push_unique(&mut readers, entry),
            "insert" | "update" | "delete" | "truncate" => push_unique(&mut writers, entry),
            _ => {}
        }
    }
    if let Some(table_id) = view.graph.lookup(NodeKind::Table, key) {
        let mut graph_entries: Vec<(bool, AccessEntry)> = Vec::new();
        for edge in view.graph.incoming(table_id) {
            let is_read = match edge.kind {
                EdgeKind::Reads => true,
                EdgeKind::Writes => false,
                _ => continue,
            };
            let Some(node) = view.graph.node(edge.from) else {
                continue;
            };
            graph_entries.push((
                is_read,
                AccessEntry {
                    owner: node.key.clone(),
                    op: if is_read {
                        "reads".to_owned()
                    } else {
                        "writes".to_owned()
                    },
                    file: node.file.as_ref().map(|file| file.as_str().to_owned()),
                    line: if edge.line == 0 { node.line } else { edge.line },
                    confidence: edge.confidence.as_str().to_owned(),
                },
            ));
        }
        graph_entries.sort_by(|a, b| {
            a.1.owner
                .cmp(&b.1.owner)
                .then(a.1.op.cmp(&b.1.op))
                .then(a.1.file.cmp(&b.1.file))
                .then(a.1.line.cmp(&b.1.line))
        });
        for (is_read, entry) in graph_entries {
            let list = if is_read { &mut readers } else { &mut writers };
            // A graph edge repeats the access its fact already reported for this owner.
            if !list.iter().any(|existing| existing.owner == entry.owner) {
                list.push(entry);
            }
        }
    }
    sort_accesses(&mut readers);
    sort_accesses(&mut writers);
    (readers, writers)
}

/// Adds an entry unless the same owner already has the same op.
fn push_unique(list: &mut Vec<AccessEntry>, entry: AccessEntry) {
    if !list
        .iter()
        .any(|existing| existing.owner == entry.owner && existing.op == entry.op)
    {
        list.push(entry);
    }
}

/// Sorts accesses by owner, op, file and line.
fn sort_accesses(list: &mut [AccessEntry]) {
    list.sort_by(|a, b| {
        a.owner
            .cmp(&b.owner)
            .then(a.op.cmp(&b.op))
            .then(a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
    });
}

/// Whether a string data field equals `want`.
fn data_is(fact: &Fact, field: &str, want: &str) -> bool {
    match fact.data_text(field) {
        Some(value) => value == want,
        None => false,
    }
}

/// Whether the fact key is `<key>.<something>`.
fn has_key_prefix(fact: &Fact, key: &str) -> bool {
    let mut prefix = key.to_owned();
    prefix.push('.');
    fact.key.starts_with(&prefix)
}

/// Column name from the latest fact's data or from its key.
fn column_name(fact: &Fact, key: &str) -> Option<String> {
    if let Some(column) = fact.data_text("column") {
        return Some(column);
    }
    let mut prefix = key.to_owned();
    prefix.push('.');
    fact.key.strip_prefix(&prefix).map(|name| name.to_owned())
}

/// The text when present, else an empty string.
fn text_or(value: Option<String>, fallback: &str) -> String {
    match value {
        Some(value) => value,
        None => fallback.to_owned(),
    }
}

/// Text line for a fact (`SPEC.md`, "Commands" and "Output").
fn fact_line(fact: &Fact) -> String {
    format!(
        "{}:{} {} {} {}",
        fact.file,
        fact.line,
        fact.kind.as_str(),
        fact.key,
        serde_json::Value::Object(fact.data.clone())
    )
}

/// JSON item for a fact (`SPEC.md`, "Output").
fn fact_item(fact: &Fact) -> serde_json::Value {
    serde_json::json!({
        "kind": fact.kind.as_str(),
        "key": fact.key,
        "file": fact.file.as_str(),
        "line": fact.line,
        "confidence": fact.confidence.as_str(),
        "data": serde_json::Value::Object(fact.data.clone()),
    })
}
