//! `arlineo find` (`SPEC.md`, "find").

use crate::error::Result;
use crate::model::{Fact, FactKind};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;
use crate::util::glob::Glob;

/// Facts of `kind` (all kinds when `None`) matching every `field=glob` condition, at most
/// `limit` of them, in facts order.
pub fn run(
    view: &View,
    kind: Option<FactKind>,
    conditions: &[(String, String)],
    limit: usize,
) -> Result<Report> {
    let mut compiled: Vec<(&str, Glob)> = Vec::new();
    for (field, pattern) in conditions {
        compiled.push((field.as_str(), Glob::new(pattern)?));
    }
    let mut matching: Vec<&Fact> = Vec::new();
    match kind {
        Some(kind) => {
            for fact in view.fact_index.of_kind(&view.facts, kind) {
                if matches_all(fact, &compiled) {
                    matching.push(fact);
                }
            }
        }
        None => {
            for fact in &view.facts {
                if matches_all(fact, &compiled) {
                    matching.push(fact);
                }
            }
        }
    }
    let total = matching.len();
    let mut lines: Vec<String> = Vec::new();
    let mut items: Vec<serde_json::Value> = Vec::new();
    for fact in matching.iter().take(limit) {
        lines.push(format!(
            "{}:{} {} {} {}",
            fact.file,
            fact.line,
            fact.kind.as_str(),
            fact.key,
            serde_json::Value::Object(fact.data.clone())
        ));
        items.push(serde_json::json!({
            "kind": fact.kind.as_str(),
            "key": fact.key,
            "file": fact.file.as_str(),
            "line": fact.line,
            "confidence": fact.confidence.as_str(),
            "data": serde_json::Value::Object(fact.data.clone()),
        }));
    }
    let shown = items.len();
    let truncated = total > shown;
    if truncated {
        lines.push(format!(
            "... {} more (use --limit)",
            total.saturating_sub(shown)
        ));
    }
    Ok(Report {
        command: "find".to_owned(),
        lines,
        json: serde_json::json!({
            "items": items,
            "total": total,
            "truncated": truncated,
        }),
        status: ReportStatus::Ok,
    })
}

/// Whether a fact matches every compiled condition.
fn matches_all(fact: &Fact, conditions: &[(&str, Glob)]) -> bool {
    conditions
        .iter()
        .all(|(field, glob)| match field_value(fact, field) {
            Some(value) => glob.is_match(&value),
            None => false,
        })
}

/// Text a condition field refers to; `None` when a data field is missing or null.
fn field_value(fact: &Fact, field: &str) -> Option<String> {
    match field {
        "key" => Some(fact.key.clone()),
        "file" => Some(fact.file.as_str().to_owned()),
        "line" => Some(fact.line.to_string()),
        other => fact.data_text(other),
    }
}
