//! `arlineo route` (`SPEC.md`, "route").

use crate::error::Result;
use crate::model::{EdgeKind, Fact, FactKind, NodeKind, norm_path};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;
use crate::query::fnq::last_segment;
use crate::util::glob::Glob;

/// Routes whose normalized path matches `path` (normalized, or a glob over normalized
/// paths), with handlers and the API calls that reach them; also API calls without a route.
pub fn run(view: &View, path: &str) -> Result<Report> {
    let glob = if path.contains('*') {
        Some(Glob::new(path)?)
    } else {
        None
    };
    let target = norm_path(path);
    let all_routes: Vec<&Fact> = view
        .fact_index
        .of_kind(&view.facts, FactKind::Route)
        .collect();
    let all_calls: Vec<&Fact> = view
        .fact_index
        .of_kind(&view.facts, FactKind::ApiCall)
        .collect();
    let mut lines: Vec<String> = Vec::new();
    let mut route_items: Vec<serde_json::Value> = Vec::new();
    for route in all_routes
        .iter()
        .filter(|fact| path_matches(&fact_norm(fact), &target, &glob))
    {
        let method = fact_method(route);
        let route_norm = fact_norm(route);
        let written_path = text_or(route.data_text("path"), &route_norm);
        let pattern = text_or(route.data_text("pattern"), "-");
        let handler = text_or(route.data_text("handler"), "-");
        lines.push(format!(
            "{}:{} route {} {} {} pattern {} handler {}",
            route.file, route.line, method, route_norm, written_path, pattern, handler
        ));
        let mut handler_items: Vec<serde_json::Value> = Vec::new();
        // Routes with the same key share one node; show only the handler this fact names.
        let handler_name = route.data_text("handler");
        if let Some(route_id) = view.graph.lookup(NodeKind::Route, &route.key) {
            for edge in view.graph.outgoing(route_id) {
                if edge.kind != EdgeKind::Handles {
                    continue;
                }
                let Some(node) = view.graph.node(edge.to) else {
                    continue;
                };
                if handler_name.as_deref().map(last_segment) != Some(last_segment(&node.key)) {
                    continue;
                }
                match &node.file {
                    Some(file) => lines.push(format!(
                        "  handler {} {} {}:{}",
                        node.key,
                        edge.confidence.as_str(),
                        file,
                        node.line
                    )),
                    None => lines.push(format!(
                        "  handler {} {}",
                        node.key,
                        edge.confidence.as_str()
                    )),
                }
                handler_items.push(serde_json::json!({
                    "kind": node.kind.as_str(),
                    "key": node.key,
                    "file": node.file.as_ref().map(|file| file.as_str()),
                    "line": node.line,
                    "confidence": edge.confidence.as_str(),
                    "edge_line": edge.line,
                }));
            }
        }
        lines.push("  calls:".to_owned());
        let mut call_items: Vec<serde_json::Value> = Vec::new();
        for call in all_calls
            .iter()
            .filter(|call| call_reaches_route(call, route, &method, &route_norm))
        {
            lines.push(format!(
                "    {}:{} api_call {} {}",
                call.file,
                call.line,
                call.key,
                call.confidence.as_str()
            ));
            call_items.push(fact_item(call));
        }
        route_items.push(serde_json::json!({
            "kind": route.kind.as_str(),
            "key": route.key,
            "file": route.file.as_str(),
            "line": route.line,
            "confidence": route.confidence.as_str(),
            "method": method,
            "path": written_path,
            "norm_path": route_norm,
            "pattern": route.data_text("pattern"),
            "handler": route.data_text("handler"),
            "handlers": handler_items,
            "calls": call_items,
            "data": serde_json::Value::Object(route.data.clone()),
        }));
    }
    lines.push("calls without a route".to_owned());
    let mut orphan_items: Vec<serde_json::Value> = Vec::new();
    for call in all_calls
        .iter()
        .filter(|fact| path_matches(&fact_norm(fact), &target, &glob))
    {
        if has_route(call, &all_routes) {
            continue;
        }
        lines.push(format!(
            "{}:{} api_call {} {}",
            call.file,
            call.line,
            call.key,
            call.confidence.as_str()
        ));
        orphan_items.push(fact_item(call));
    }
    Ok(Report {
        command: "route".to_owned(),
        lines,
        json: serde_json::json!({
            "path": path,
            "norm_path": match &glob {
                Some(_) => path,
                None => target.as_str(),
            },
            "routes": route_items,
            "calls_without_route": orphan_items,
        }),
        status: ReportStatus::Ok,
    })
}

/// Whether a normalized path equals the target or matches the glob over normalized paths.
fn path_matches(norm: &str, target: &str, glob: &Option<Glob>) -> bool {
    match glob {
        Some(glob) => glob.is_match(norm),
        None => norm == target,
    }
}

/// Whether an API call reaches a route: same key, or any method to an `ANY` route's path.
fn call_reaches_route(call: &Fact, route: &Fact, route_method: &str, route_norm: &str) -> bool {
    call.key == route.key || (route_method == "ANY" && fact_norm(call) == route_norm)
}

/// Whether any route has the call's key, or an `ANY` route has the call's normalized path.
fn has_route(call: &Fact, routes: &[&Fact]) -> bool {
    let call_norm = fact_norm(call);
    routes.iter().any(|route| {
        route.key == call.key || (fact_method(route) == "ANY" && fact_norm(route) == call_norm)
    })
}

/// Normalized path of a route or API call fact, from its data or, failing that, its key.
fn fact_norm(fact: &Fact) -> String {
    if let Some(norm) = fact.data_text("norm_path") {
        return norm;
    }
    if let Some(written) = fact.data_text("path") {
        return norm_path(&written);
    }
    match fact.key.split_once(' ') {
        Some((_, rest)) => rest.to_owned(),
        None => norm_path(&fact.key),
    }
}

/// HTTP method of a route or API call fact, from its data or, failing that, its key.
fn fact_method(fact: &Fact) -> String {
    if let Some(method) = fact.data_text("method") {
        return method;
    }
    match fact.key.split_once(' ') {
        Some((method, _)) => method.to_owned(),
        None => String::new(),
    }
}

/// The text when present, else a fixed placeholder.
fn text_or(value: Option<String>, fallback: &str) -> String {
    match value {
        Some(value) => value,
        None => fallback.to_owned(),
    }
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
