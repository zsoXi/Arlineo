//! `arlineo check`: project rules (`SPEC.md`, "Rules" and "check").

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, Direction, Rule, RuleSpec, Selector};
use crate::error::Result;
use crate::model::{Confidence, EdgeKind, Fact, FactKind, NodeId, RelPath};
use crate::output::{Report, ReportStatus};
use crate::overlay::View;

/// One rule violation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    pub rule: String,
    /// Identity of the hit without a line number (`SPEC.md`, "Hit keys").
    pub key: String,
    pub file: RelPath,
    pub line: u32,
    pub message: String,
    /// `false` for an uncertain hit, which `run` lists apart and does not count: a
    /// `requires-before` chain cut by `depth`, or ending at a function whose only callers are
    /// `guessed`.
    pub certain: bool,
}

/// Every violation of every rule in a view, sorted, without the hits a rule's `ignore` lists.
pub fn hits(config: &Config, view: &View) -> Result<Vec<Hit>> {
    Ok(rule_hits(config, view)?.0)
}

/// Every violation of every rule in a view, sorted: the reported hits and, apart, the hits that
/// a rule's `ignore` lists.
fn rule_hits(config: &Config, view: &View) -> Result<(Vec<Hit>, Vec<Hit>)> {
    let mut found = Vec::new();
    let mut ignored = Vec::new();
    for rule in &config.rules {
        let start = found.len();
        match &rule.spec {
            RuleSpec::RequiresBefore {
                when,
                require,
                depth,
                include_ancestor_callees,
                all_paths,
            } => check_requires_before(
                rule,
                when,
                require,
                *depth,
                *include_ancestor_callees,
                *all_paths,
                view,
                &mut found,
            ),
            RuleSpec::OrderInFunction {
                first,
                then,
                only_with_first,
            } => check_order_in_function(rule, first, then, *only_with_first, view, &mut found),
            RuleSpec::SourcesAgree {
                left,
                right,
                fields,
                direction,
                minus,
                dropped,
            } => check_sources_agree(
                rule,
                left,
                right,
                fields,
                *direction,
                minus.as_ref(),
                dropped.as_ref(),
                view,
                &mut found,
            ),
            RuleSpec::HasTarget {
                source,
                target,
                source_field,
                target_field,
            } => check_has_target(
                rule,
                source,
                target,
                source_field,
                target_field,
                &api_bases(config),
                view,
                &mut found,
            ),
            RuleSpec::Forbidden { selector } => check_forbidden(rule, selector, view, &mut found),
            RuleSpec::ReviewOnChange { selector } => {
                check_review_on_change(rule, selector, view, &mut found)
            }
        }
        if !rule.ignore.is_empty() {
            for hit in found.split_off(start) {
                if rule.ignore.contains(&hit.key) {
                    ignored.push(hit);
                } else {
                    found.push(hit);
                }
            }
        }
    }
    for list in [&mut found, &mut ignored] {
        list.sort();
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        list.retain(|hit| seen.insert((hit.rule.clone(), hit.key.clone())));
    }
    Ok((found, ignored))
}

/// Violations in `now` whose `(rule, key)` is not among the violations in `before`: a certain
/// hit is new unless `before` has it as certain, an uncertain hit unless `before` has it at all.
/// Status `Findings` when there is at least one certain one; uncertain ones are listed apart.
pub fn run(config: &Config, now: &View, before: &View) -> Result<Report> {
    let (now_hits, ignored) = rule_hits(config, now)?;
    let before_hits = hits(config, before)?;
    let mut before_certain: BTreeSet<(String, String)> = BTreeSet::new();
    let mut before_any: BTreeSet<(String, String)> = BTreeSet::new();
    for hit in &before_hits {
        let id = (hit.rule.clone(), hit.key.clone());
        if hit.certain {
            before_certain.insert(id.clone());
        }
        before_any.insert(id);
    }
    let mut new_hits: Vec<Hit> = Vec::new();
    let mut uncertain: Vec<Hit> = Vec::new();
    for hit in now_hits.iter() {
        let id = (hit.rule.clone(), hit.key.clone());
        if hit.certain && !before_certain.contains(&id) {
            new_hits.push(hit.clone());
        } else if !hit.certain && !before_any.contains(&id) {
            uncertain.push(hit.clone());
        }
    }
    // `ignore` entries that matched no hit in `now`: stale or mistyped keys.
    let mut unused: Vec<(String, String)> = Vec::new();
    for rule in &config.rules {
        for key in &rule.ignore {
            if !ignored
                .iter()
                .any(|hit| hit.rule == rule.id && hit.key == *key)
            {
                unused.push((rule.id.clone(), key.clone()));
            }
        }
    }
    let already_present = now_hits
        .len()
        .saturating_sub(new_hits.len() + uncertain.len());
    let mut lines: Vec<String> = Vec::new();
    for hit in &new_hits {
        // The message already starts with "Rule <id>:".
        lines.push(format!("{}:{} {}", hit.file, hit.line, hit.message));
    }
    for hit in &uncertain {
        lines.push(format!(
            "uncertain (not counted): {}:{} {}",
            hit.file, hit.line, hit.message
        ));
    }
    for (rule, key) in &unused {
        lines.push(format!(
            "warning: rule {rule}: ignore entry '{key}' matches no hit"
        ));
    }
    let mut summary = if new_hits.is_empty() {
        "no new violations".to_owned()
    } else {
        format!("{} new violations", new_hits.len())
    };
    if !uncertain.is_empty() {
        summary.push_str(&format!(", {} uncertain", uncertain.len()));
    }
    if !ignored.is_empty() {
        summary.push_str(&format!(", {} ignored", ignored.len()));
    }
    if already_present > 0 {
        summary.push_str(&format!(" ({already_present} already present on the line)"));
    }
    lines.push(summary);
    let status = if new_hits.is_empty() {
        ReportStatus::Ok
    } else {
        ReportStatus::Findings
    };
    Ok(Report {
        command: "check".to_owned(),
        lines,
        json: serde_json::json!({
            "new": hits_json(&new_hits),
            "uncertain": hits_json(&uncertain),
            "already_present": already_present,
            "ignored": ignored.len(),
            "ignore_unused": unused
                .iter()
                .map(|(rule, key)| serde_json::json!({ "rule": rule, "key": key }))
                .collect::<Vec<_>>(),
        }),
        status,
    })
}

fn hits_json(hits: &[Hit]) -> Vec<serde_json::Value> {
    hits.iter()
        .map(|hit| {
            serde_json::json!({
                "rule": hit.rule,
                "key": hit.key,
                "file": hit.file.as_str(),
                "line": hit.line,
                "message": hit.message,
            })
        })
        .collect()
}

fn matching_facts<'a>(view: &'a View, selector: &Selector) -> Vec<&'a Fact> {
    view.fact_index
        .of_kind(&view.facts, selector.kind)
        .filter(|fact| selector.matches(fact))
        .collect()
}

fn owner_of(fact: &Fact) -> Option<String> {
    fact.data_text("owner")
}

fn owner_or_key(fact: &Fact) -> String {
    match owner_of(fact) {
        Some(owner) => owner,
        None => fact.key.clone(),
    }
}

fn field_value(fact: &Fact, field: &str) -> Option<String> {
    if field == "key" {
        Some(fact.key.clone())
    } else {
        fact.data_text(field)
    }
}

fn selector_text(selector: &Selector) -> String {
    let mut text = format!("{} fact", selector.kind.as_str());
    if let Some(key) = &selector.key {
        text.push_str(&format!(" with key '{}'", key.as_str()));
    }
    if let Some(file) = &selector.file {
        text.push_str(&format!(" in file '{}'", file.as_str()));
    }
    for (field, glob) in &selector.data {
        text.push_str(&format!(" with {} '{}'", field, glob.as_str()));
    }
    text
}

fn with_note(rule: &Rule, sentence: String) -> String {
    match &rule.note {
        Some(note) if !note.trim().is_empty() => format!("{sentence} {note}"),
        _ => sentence,
    }
}

fn make_hit(rule: &Rule, key: String, fact: &Fact, sentence: String) -> Hit {
    Hit {
        rule: rule.id.clone(),
        key,
        file: fact.file.clone(),
        line: fact.line,
        message: with_note(rule, sentence),
        certain: true,
    }
}

fn caller_owners(
    view: &View,
    owner: &str,
    depth: u32,
    include_ancestor_callees: bool,
) -> BTreeSet<String> {
    let mut allowed: BTreeSet<String> = BTreeSet::new();
    allowed.insert(owner.to_owned());
    let start = view.graph.lookup_any(owner);
    let mut visited: BTreeSet<NodeId> = BTreeSet::new();
    for id in &start {
        visited.insert(*id);
    }
    let mut frontier = start;
    let mut callers: Vec<NodeId> = Vec::new();
    let mut level: u32 = 0;
    while level < depth {
        let mut next: Vec<NodeId> = Vec::new();
        for node in &frontier {
            for edge in view.graph.incoming(*node) {
                if edge.kind != EdgeKind::Calls && edge.kind != EdgeKind::Handles {
                    continue;
                }
                if visited.insert(edge.from) {
                    callers.push(edge.from);
                    next.push(edge.from);
                    if let Some(caller) = view.graph.node(edge.from) {
                        allowed.insert(caller.key.clone());
                    }
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
        level = level.saturating_add(1);
    }
    if include_ancestor_callees {
        for caller in &callers {
            for edge in view.graph.outgoing(*caller) {
                if edge.kind != EdgeKind::Calls && edge.kind != EdgeKind::Handles {
                    continue;
                }
                if let Some(callee) = view.graph.node(edge.to) {
                    allowed.insert(callee.key.clone());
                }
            }
        }
    }
    allowed
}

/// Whether the function `node` takes a lock before `line`: one of its own `require` facts, or a
/// call to a function that has one, at an earlier line. Guessed calls do not count.
fn locks_before(view: &View, node: NodeId, line: u32, lock_lines: &BTreeMap<String, u32>) -> bool {
    let Some(function) = view.graph.node(node) else {
        return false;
    };
    if lock_lines.get(&function.key).is_some_and(|at| *at < line) {
        return true;
    }
    view.graph.outgoing(node).any(|edge| {
        edge.kind == EdgeKind::Calls
            && edge.confidence != Confidence::Guessed
            && edge.line < line
            && view
                .graph
                .node(edge.to)
                .is_some_and(|callee| lock_lines.contains_key(&callee.key))
    })
}

/// A chain of callers without the lock, and whether it is certain.
type Chain = (Vec<String>, bool);

/// The first chain of callers from `node` (reached at `line`) up to a function without callers,
/// or `depth` callers up, on which no function takes the lock before its next call; `None` when
/// every chain takes it. Guessed edges are not followed. A caller already on the chain is a
/// call cycle and is skipped; a function that only calls itself counts as one without callers,
/// and one whose callers are all on the chain ends an uncertain chain.
/// A chain is uncertain when `depth` cuts it, it ends at a function whose only callers are
/// `guessed`, or it ends in a call cycle; a certain chain is returned before an uncertain one.
/// The flag is true when a caller was skipped for being on the chain, here or further up: such
/// a result depends on the path, so it is not remembered in `locked`.
fn unlocked_chain(
    view: &View,
    node: NodeId,
    line: u32,
    depth: u32,
    lock_lines: &BTreeMap<String, u32>,
    on_chain: &mut Vec<NodeId>,
    locked: &mut BTreeSet<(NodeId, u32, u32)>,
) -> (Option<Chain>, bool) {
    if locked.contains(&(node, line, depth)) || locks_before(view, node, line, lock_lines) {
        return (None, false);
    }
    let Some(key) = view.graph.node(node).map(|function| function.key.clone()) else {
        return (None, false);
    };
    let mut guessed_callers = false;
    let mut callers: Vec<(NodeId, u32)> = Vec::new();
    for edge in view.graph.incoming(node) {
        if edge.kind != EdgeKind::Calls && edge.kind != EdgeKind::Handles {
            continue;
        }
        if edge.confidence == Confidence::Guessed {
            guessed_callers = true;
        } else if edge.from != node {
            callers.push((edge.from, edge.line));
        }
    }
    if callers.is_empty() {
        return (Some((vec![key], !guessed_callers)), false);
    }
    if depth == 0 {
        return (Some((vec![key], false)), false);
    }
    on_chain.push(node);
    let mut found: Option<Chain> = None;
    let mut cut = false;
    let mut outside = false;
    for (caller, at) in callers {
        if on_chain.contains(&caller) {
            cut = true;
            continue;
        }
        outside = true;
        let (result, caller_cut) = unlocked_chain(
            view,
            caller,
            at,
            depth.saturating_sub(1),
            lock_lines,
            on_chain,
            locked,
        );
        cut |= caller_cut;
        if let Some(chain) = result {
            let certain = chain.1;
            if found.is_none() || certain {
                found = Some(chain);
            }
            if certain {
                break;
            }
        }
    }
    on_chain.pop();
    if !outside {
        return (Some((vec![key], false)), true);
    }
    match found {
        Some((mut chain, certain)) => {
            chain.insert(0, key);
            (Some((chain, certain)), cut)
        }
        None => {
            if !cut {
                locked.insert((node, line, depth));
            }
            (None, cut)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the fields of one rule kind, used once"
)]
fn check_requires_before(
    rule: &Rule,
    when: &Selector,
    require: &Selector,
    depth: u32,
    include_ancestor_callees: bool,
    all_paths: bool,
    view: &View,
    out: &mut Vec<Hit>,
) {
    let mut require_owners: BTreeSet<String> = BTreeSet::new();
    // First line of a `require` fact in each function.
    let mut lock_lines: BTreeMap<String, u32> = BTreeMap::new();
    for fact in matching_facts(view, require) {
        if let Some(owner) = owner_of(fact) {
            let first = lock_lines.entry(owner.clone()).or_insert(fact.line);
            *first = (*first).min(fact.line);
            require_owners.insert(owner);
        }
    }
    let mut locked: BTreeSet<(NodeId, u32, u32)> = BTreeSet::new();
    for fact in matching_facts(view, when) {
        let owner = match owner_of(fact) {
            Some(owner) => owner,
            None => continue,
        };
        if all_paths {
            let nodes = view.graph.lookup_any(&owner);
            let chain = if nodes.is_empty() {
                // Not in the graph: only the function itself can take the lock.
                match lock_lines.get(&owner) {
                    Some(at) if *at < fact.line => None,
                    _ => Some((vec![owner.clone()], true)),
                }
            } else {
                let mut found: Option<Chain> = None;
                for node in nodes {
                    let (result, _) = unlocked_chain(
                        view,
                        node,
                        fact.line,
                        depth,
                        &lock_lines,
                        &mut Vec::new(),
                        &mut locked,
                    );
                    if let Some(chain) = result {
                        let certain = chain.1;
                        if found.is_none() || certain {
                            found = Some(chain);
                        }
                        if certain {
                            break;
                        }
                    }
                }
                found
            };
            let Some((chain, certain)) = chain else {
                continue;
            };
            let key = format!("{}|{}|{}", rule.id, owner, fact.key);
            let sentence = format!(
                "Rule {}: {} has no required {} before it on the caller chain {}.",
                rule.id,
                owner,
                selector_text(require),
                chain.join(" <- ")
            );
            let mut hit = make_hit(rule, key, fact, sentence);
            hit.certain = certain;
            out.push(hit);
            continue;
        }
        let allowed = caller_owners(view, &owner, depth, include_ancestor_callees);
        let mut holds = false;
        for required in &require_owners {
            if allowed.contains(required) {
                holds = true;
                break;
            }
        }
        if holds {
            continue;
        }
        let key = format!("{}|{}|{}", rule.id, owner, fact.key);
        let sentence = format!(
            "Rule {}: {} has no required {} in itself or its callers.",
            rule.id,
            owner,
            selector_text(require)
        );
        out.push(make_hit(rule, key, fact, sentence));
    }
}

fn check_order_in_function(
    rule: &Rule,
    first: &Selector,
    then: &Selector,
    only_with_first: bool,
    view: &View,
    out: &mut Vec<Hit>,
) {
    let mut first_line: BTreeMap<String, u32> = BTreeMap::new();
    for fact in matching_facts(view, first) {
        let owner = match owner_of(fact) {
            Some(owner) => owner,
            None => continue,
        };
        match first_line.get(&owner) {
            Some(line) if *line <= fact.line => {}
            _ => {
                first_line.insert(owner, fact.line);
            }
        }
    }
    let mut first_then: BTreeMap<String, &Fact> = BTreeMap::new();
    for fact in matching_facts(view, then) {
        let owner = match owner_of(fact) {
            Some(owner) => owner,
            None => continue,
        };
        first_then.entry(owner).or_insert(fact);
    }
    for (owner, then_fact) in &first_then {
        let holds = match first_line.get(owner) {
            Some(line) => *line < then_fact.line,
            None => only_with_first,
        };
        if holds {
            continue;
        }
        let key = format!("{}|{}|", rule.id, owner);
        let sentence = format!(
            "Rule {}: {} has no {} before its first {}.",
            rule.id,
            owner,
            selector_text(first),
            selector_text(then)
        );
        out.push(make_hit(rule, key, then_fact, sentence));
    }
}

fn tuple_of(fact: &Fact, fields: &[String]) -> Vec<String> {
    let mut tuple: Vec<String> = Vec::new();
    for field in fields {
        match field_value(fact, field) {
            Some(value) => tuple.push(value),
            None => tuple.push(String::new()),
        }
    }
    tuple
}

/// Tuples that also cover a fact on the other side: the fact's own tuple and, for a grant, the
/// same grant on the whole object (empty `column`) and `ALL` instead of the privilege, in every
/// combination. Coverage works through the `privilege` and `column` fields, not through `key`.
fn covering_tuples(fact: &Fact, fields: &[String]) -> Vec<Vec<String>> {
    let mut tuples = vec![tuple_of(fact, fields)];
    if fact.kind != FactKind::Grant {
        return tuples;
    }
    for all in [false, true] {
        for whole_object in [false, true] {
            let tuple: Vec<String> = fields
                .iter()
                .map(|field| match field.as_str() {
                    "privilege" if all => "ALL".to_owned(),
                    "column" if whole_object => String::new(),
                    _ => field_value(fact, field).unwrap_or_default(),
                })
                .collect();
            if !tuples.contains(&tuple) {
                tuples.push(tuple);
            }
        }
    }
    tuples
}

/// Order of facts across files of one side: file name (migrations sort by their timestamp
/// prefix), then line.
fn position(fact: &Fact) -> (&str, u32) {
    let file = fact.file.as_str();
    (file.rsplit('/').next().unwrap_or(file), fact.line)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the fields of one rule kind, used once"
)]
fn check_sources_agree(
    rule: &Rule,
    left: &Selector,
    right: &Selector,
    fields: &[String],
    direction: Direction,
    minus: Option<&Selector>,
    dropped: Option<&Selector>,
    view: &View,
    out: &mut Vec<Hit>,
) {
    let default_fields: Vec<String> = vec!["key".to_owned()];
    let effective: &[String] = if fields.is_empty() {
        &default_fields
    } else {
        fields
    };
    // Latest position of each cancelling tuple.
    let mut cancelled: BTreeMap<Vec<String>, (&str, u32)> = BTreeMap::new();
    if let Some(minus) = minus {
        for fact in matching_facts(view, minus) {
            let at = position(fact);
            let latest = cancelled.entry(tuple_of(fact, effective)).or_insert(at);
            *latest = (*latest).max(at);
        }
    }
    // Latest position at which each object ends.
    let mut ended: BTreeMap<String, (&str, u32)> = BTreeMap::new();
    if let Some(dropped) = dropped {
        for fact in matching_facts(view, dropped) {
            let at = position(fact);
            let latest = ended.entry(fact.key.clone()).or_insert(at);
            *latest = (*latest).max(at);
        }
        // A dropped table takes the sequences of its identity and serial columns with it.
        let owned: Vec<(String, (&str, u32))> = view
            .fact_index
            .of_kind(&view.facts, FactKind::Column)
            .filter_map(|column| {
                let at = *ended.get(column.data_text("table")?.as_str())?;
                Some((column.data_text("sequence")?, at))
            })
            .collect();
        for (sequence, at) in owned {
            let latest = ended.entry(sequence).or_insert(at);
            *latest = (*latest).max(at);
        }
    }
    let mut left_map: BTreeMap<Vec<String>, &Fact> = BTreeMap::new();
    for fact in matching_facts(view, left) {
        let at = position(fact);
        let object = field_value(fact, "object").unwrap_or_else(|| fact.key.clone());
        let gone = covering_tuples(fact, effective)
            .iter()
            .any(|tuple| cancelled.get(tuple).is_some_and(|latest| *latest > at))
            || ended.get(&object).is_some_and(|latest| *latest > at);
        if gone {
            continue;
        }
        let tuple = tuple_of(fact, effective);
        left_map.entry(tuple).or_insert(fact);
    }
    let mut right_map: BTreeMap<Vec<String>, &Fact> = BTreeMap::new();
    for fact in matching_facts(view, right) {
        let tuple = tuple_of(fact, effective);
        right_map.entry(tuple).or_insert(fact);
    }
    for (tuple, fact) in &left_map {
        if covering_tuples(fact, effective)
            .iter()
            .any(|covering| right_map.contains_key(covering))
        {
            continue;
        }
        let joined = tuple.join(",");
        let key = format!("{}|{}|{},{}", rule.id, fact.key, joined, "left");
        let sentence = format!(
            "Rule {}: fact '{}' has tuple '{}' present only on the left side and missing from the right side.",
            rule.id, fact.key, joined
        );
        out.push(make_hit(rule, key, fact, sentence));
    }
    if direction == Direction::LeftInRight {
        return;
    }
    for (tuple, fact) in &right_map {
        if covering_tuples(fact, effective)
            .iter()
            .any(|covering| left_map.contains_key(covering))
        {
            continue;
        }
        let joined = tuple.join(",");
        let key = format!("{}|{}|{},{}", rule.id, fact.key, joined, "right");
        let sentence = format!(
            "Rule {}: fact '{}' has tuple '{}' present only on the right side and missing from the left side.",
            rule.id, fact.key, joined
        );
        out.push(make_hit(rule, key, fact, sentence));
    }
}

/// Normalized `base_path` segments of the API client patterns.
fn api_bases(config: &Config) -> Vec<Vec<String>> {
    let mut bases: Vec<Vec<String>> = Vec::new();
    for pattern in &config.api_clients {
        if let Some(base) = &pattern.base_path {
            let segments: Vec<String> = path_segments(&crate::model::norm_path(base))
                .into_iter()
                .map(str::to_owned)
                .collect();
            if !segments.is_empty() && !bases.contains(&segments) {
                bases.push(segments);
            }
        }
    }
    bases
}

fn path_segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Length of the longest base path that starts `segments`; 0 when none does.
fn base_len<S: AsRef<str>>(segments: &[S], bases: &[Vec<String>]) -> usize {
    bases
        .iter()
        .filter(|base| {
            base.len() <= segments.len()
                && base
                    .iter()
                    .zip(segments)
                    .all(|(b, s)| b.as_str() == s.as_ref())
        })
        .map(Vec::len)
        .max()
        .unwrap_or(0)
}

/// A route as an HTTP method and normalized path segments.
type RouteShape = (String, Vec<String>);

/// Whether call segments match route segments (`RULES.md`, "has-target"): the same count, a
/// route `{}` takes any segment, and a call `{}` against a constant route segment is a weak
/// match that holds only when a constant segment after the base (`base_len`) agrees too.
fn segments_match(call: &[&str], route: &[String], base_len: usize) -> bool {
    if call.len() != route.len() {
        return false;
    }
    let mut weak = false;
    let mut agreed = false;
    for (index, (call_segment, route_segment)) in call.iter().zip(route).enumerate() {
        if route_segment == "{}" {
            continue;
        }
        if *call_segment == "{}" {
            weak = true;
        } else if *call_segment != route_segment.as_str() {
            return false;
        } else if index >= base_len {
            agreed = true;
        }
    }
    !weak || agreed
}

/// Routes that API calls can match (`RULES.md`, "has-target"): every route with a constant
/// segment after a base path.
fn api_routes(targets: &[&Fact], bases: &[Vec<String>]) -> Vec<RouteShape> {
    targets
        .iter()
        .filter_map(|fact| {
            let method = fact.data_text("method")?;
            let path = fact.data_text("norm_path")?;
            let segments: Vec<String> = path_segments(&path)
                .into_iter()
                .map(str::to_owned)
                .collect();
            let constant = segments
                .iter()
                .skip(base_len(&segments, bases))
                .any(|segment| segment != "{}");
            constant.then_some((method, segments))
        })
        .collect()
}

/// For an API call key (`METHOD /path`): `Some(true)` when a route with the same method (or
/// `ANY`) matches segment by segment; `Some(false)` otherwise; `None` when the call path starts
/// with a parameter right after a base path, so its prefix is unknown and it is skipped.
fn api_call_has_route(value: &str, routes: &[RouteShape], bases: &[Vec<String>]) -> Option<bool> {
    let (method, path) = value.split_once(' ')?;
    let call = path_segments(path);
    let base_len = base_len(&call, bases);
    if call.get(base_len).is_none_or(|segment| *segment == "{}") {
        return None;
    }
    Some(routes.iter().any(|(route_method, route)| {
        (route_method == method || route_method == "ANY") && segments_match(&call, route, base_len)
    }))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the fields of one rule kind, used once"
)]
fn check_has_target(
    rule: &Rule,
    source: &Selector,
    target: &Selector,
    source_field: &str,
    target_field: &str,
    bases: &[Vec<String>],
    view: &View,
    out: &mut Vec<Hit>,
) {
    let targets = matching_facts(view, target);
    let mut target_values: BTreeSet<String> = BTreeSet::new();
    for fact in &targets {
        if let Some(value) = field_value(fact, target_field) {
            target_values.insert(value);
        }
    }
    // API calls against routes match by path segments, not by text.
    let by_segments = source.kind == FactKind::ApiCall
        && target.kind == FactKind::Route
        && source_field == "key"
        && target_field == "key";
    let routes = if by_segments {
        api_routes(&targets, bases)
    } else {
        Vec::new()
    };
    for fact in matching_facts(view, source) {
        let value = field_value(fact, source_field);
        let matched = match &value {
            Some(text) if by_segments => match api_call_has_route(text, &routes, bases) {
                Some(found) => found,
                None => continue,
            },
            Some(text) => target_values.contains(text),
            None => false,
        };
        if matched {
            continue;
        }
        let owner = owner_or_key(fact);
        let key = format!("{}|{}|{}", rule.id, owner, fact.key);
        let sentence = match &value {
            Some(text) => format!(
                "Rule {}: {} has no {} with {} '{}'.",
                rule.id,
                owner,
                selector_text(target),
                target_field,
                text
            ),
            None => format!(
                "Rule {}: {} has no {} because its {} field is missing.",
                rule.id,
                owner,
                selector_text(target),
                source_field
            ),
        };
        out.push(make_hit(rule, key, fact, sentence));
    }
}

fn check_forbidden(rule: &Rule, selector: &Selector, view: &View, out: &mut Vec<Hit>) {
    for fact in matching_facts(view, selector) {
        let owner = owner_or_key(fact);
        let key = format!("{}|{}|{},{}", rule.id, owner, fact.file, fact.key);
        let sentence = format!(
            "Rule {}: {} matches forbidden {}.",
            rule.id,
            owner,
            selector_text(selector)
        );
        out.push(make_hit(rule, key, fact, sentence));
    }
}

/// One hit per matching fact, keyed by the fact's content: its `hash` field (definitions),
/// else a hash of all its data. On the line every hit is already present, so `check` reports
/// exactly the matching facts that a worktree adds or changes.
fn check_review_on_change(rule: &Rule, selector: &Selector, view: &View, out: &mut Vec<Hit>) {
    for fact in matching_facts(view, selector) {
        let version = match fact.data_text("hash") {
            Some(hash) => hash,
            None => {
                let data = serde_json::Value::Object(fact.data.clone()).to_string();
                crate::util::fnv::hex(data.as_bytes())
            }
        };
        let key = format!("{}|{}|{}", rule.id, fact.key, version);
        let sentence = format!(
            "Rule {}: {} {} is new or changed; review it.",
            rule.id,
            fact.kind.as_str(),
            fact.key
        );
        out.push(make_hit(rule, key, fact, sentence));
    }
}

#[cfg(test)]
mod tests {
    use super::{Hit, hits, run};
    use crate::config::Config;
    use crate::extract::test_support::facts_with;
    use crate::model::FactIndex;
    use crate::output::ReportStatus;
    use crate::overlay::View;

    fn config(value: serde_json::Value) -> Config {
        Config::from_bytes(value.to_string().as_bytes()).expect("test configuration")
    }

    fn view(config: &Config, files: &[(&str, &str)]) -> View {
        let mut facts = Vec::new();
        for (path, text) in files {
            facts.extend(facts_with(config, path, text));
        }
        View {
            fact_index: FactIndex::new(&facts),
            graph: crate::graph::build::build(&facts).expect("graph"),
            facts,
        }
    }

    fn all_paths_rule(depth: u32) -> Config {
        config(serde_json::json!({ "rules": [ {
            "id": "lock", "kind": "requires-before", "depth": depth, "all_paths": true,
            "when": { "kind": "call", "data": { "name": "write_balance" } },
            "require": { "kind": "call", "data": { "name": "take_lock" } }
        } ] }))
    }

    const LOCKED: &str = "fn take_lock() {}\nfn write_balance() {}\nfn update() {\n    write_balance();\n}\npub fn handler_ok() {\n    take_lock();\n    update();\n}\n";

    #[test]
    fn all_paths_reports_the_chain_without_the_lock() {
        let config = all_paths_rule(3);
        let ok = view(&config, &[("src/lib.rs", LOCKED)]);
        assert_eq!(hits(&config, &ok).expect("hits"), Vec::<Hit>::new());
        let text = format!("{LOCKED}pub fn handler_bad() {{\n    update();\n}}\n");
        let found = hits(&config, &view(&config, &[("src/lib.rs", &text)])).expect("hits");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].certain, "{found:?}");
        assert!(
            found[0].message.contains("app::update <- app::handler_bad"),
            "{found:?}"
        );
    }

    #[test]
    fn all_paths_checks_the_order() {
        let config = all_paths_rule(3);
        let text =
            format!("{LOCKED}pub fn handler_late() {{\n    update();\n    take_lock();\n}}\n");
        let found = hits(&config, &view(&config, &[("src/lib.rs", &text)])).expect("hits");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("app::handler_late"), "{found:?}");
    }

    #[test]
    fn a_chain_cut_by_depth_is_uncertain_and_not_counted() {
        let config = all_paths_rule(1);
        let text = "fn take_lock() {}\nfn write_balance() {}\nfn update() {\n    write_balance();\n}\nfn middle() {\n    update();\n}\npub fn top() {\n    middle();\n}\n";
        let now = view(&config, &[("src/lib.rs", text)]);
        let found = hits(&config, &now).expect("hits");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].certain, "{found:?}");
        let before = view(&config, &[]);
        let report = run(&config, &now, &before).expect("run");
        assert_eq!(report.status, ReportStatus::Ok);
        assert_eq!(report.json["new"].as_array().map(Vec::len), Some(0));
        assert_eq!(report.json["uncertain"].as_array().map(Vec::len), Some(1));
        assert!(
            report
                .lines
                .iter()
                .any(|line| line.starts_with("uncertain (not counted): src/lib.rs:4 ")),
            "{:?}",
            report.lines
        );
    }

    #[test]
    fn grants_agree_with_revokes_drops_coverage_and_foreach_grants() {
        let config = config(serde_json::json!({ "rules": [ {
            "id": "grants-agree", "kind": "sources-agree",
            "left": { "kind": "grant", "file": "db/migrations/*.sql", "data": { "revoke": "false" } },
            "right": { "kind": "grant", "file": "db/grants.sql", "data": { "revoke": "false" } },
            "fields": ["privilege", "object", "role", "column"],
            "direction": "left-in-right",
            "minus": { "kind": "grant", "file": "db/migrations/*.sql", "data": { "revoke": "true" } },
            "dropped": { "kind": "table", "file": "db/migrations/*.sql", "data": { "action": "drop" } }
        } ] }));
        let first = "GRANT SELECT, INSERT ON orders TO worker;\nGRANT SELECT (id) ON audit TO worker;\nGRANT UPDATE ON stock TO worker;\nGRANT DELETE ON bills TO worker;\nGRANT SELECT ON roles_only TO admin;\n\
                     CREATE TABLE hooks (id bigint GENERATED ALWAYS AS IDENTITY, name text);\n\
                     CREATE TABLE logs (id bigserial PRIMARY KEY);\n\
                     GRANT SELECT ON hooks TO worker;\n\
                     GRANT USAGE ON SEQUENCE hooks_id_seq, logs_id_seq TO worker;\n";
        let second = "REVOKE INSERT ON orders FROM worker;\nDROP TABLE IF EXISTS hooks, logs;\n";
        // A grant after the drop is on the table created again, so it still needs grants.sql.
        let third = "CREATE TABLE hooks (id int);\nGRANT INSERT ON hooks TO worker;\n";
        let roles = "GRANT SELECT ON orders TO worker;\nGRANT ALL ON audit TO worker;\nGRANT SELECT ON extra TO worker;\n\
                     CREATE FUNCTION grant_all() RETURNS void AS $$\n\
                     DECLARE r text;\n\
                     BEGIN\n\
                       FOREACH r IN ARRAY ARRAY['worker'] LOOP\n\
                         EXECUTE format('GRANT UPDATE ON stock TO %I', r);\n\
                       END LOOP;\n\
                     END;\n\
                     $$ LANGUAGE plpgsql;\n";
        let now = view(
            &config,
            &[
                ("db/migrations/0001_a.sql", first),
                ("db/migrations/0002_b.sql", second),
                ("db/migrations/0003_c.sql", third),
                ("db/grants.sql", roles),
            ],
        );
        let found: Vec<String> = hits(&config, &now)
            .expect("hits")
            .into_iter()
            .map(|hit| hit.key)
            .collect();
        // Left-in-right: `extra` only on the right is no hit; `roles_only` is.
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(
            found.iter().any(|key| key.contains("DELETE,bills,worker")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|key| key.contains("SELECT,roles_only,admin")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|key| key.contains("INSERT,hooks,worker")),
            "{found:?}"
        );
    }

    const CLIENT: &str = "export async function load(id: string) {\n  await client.call(`/uploads/${id}/files`, \"GET\");\n  await client.call(`/uploads/${id}/files`, \"POST\");\n  await client.call(\"/items\", \"GET\");\n  await client.call(\"/orders\", \"GET\");\n  await client.call(`/reports/${id}/pdf`, \"GET\");\n}\n";

    const SERVER: &str = r#"
pub fn router() -> Router {
    Router::new()
        .route("/uploads/{id}/files", get(list_files))
        .route("/items", get(list_items))
}
pub async fn list_files() -> Response { not_found() }
pub async fn list_items() -> Response { not_found() }
fn not_found() -> Response { todo!() }
"#;

    fn api_config(ignore: serde_json::Value) -> Config {
        config(serde_json::json!({
            "routes": [ {
                "id": "router", "lang": "rust", "call": "route",
                "methods": { "get": "GET", "post": "POST" }, "base_path": "/api/v1"
            } ],
            "api_clients": [ {
                "id": "web", "lang": "ts", "call": "call", "receivers": ["client"],
                "method_arg": 1, "base_path": "/api/v1"
            } ],
            "rules": [ {
                "id": "calls-have-routes", "kind": "has-target",
                "source": { "kind": "api_call" }, "target": { "kind": "route" },
                "ignore": ignore
            } ]
        }))
    }

    #[test]
    fn calls_meet_routes_by_segments() {
        let config = api_config(serde_json::json!([]));
        let now = view(&config, &[("src/lib.rs", SERVER), ("web/api.ts", CLIENT)]);
        let mut found: Vec<String> = hits(&config, &now)
            .expect("hits")
            .into_iter()
            .map(|hit| hit.key)
            .collect();
        found.sort();
        // `GET /uploads/{}/files` and `/items` have routes; `POST` on uploads, `/orders` and
        // `/reports/..` do not.
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].ends_with("GET /api/v1/orders"), "{found:?}");
        assert!(
            found[1].ends_with("GET /api/v1/reports/{}/pdf"),
            "{found:?}"
        );
        assert!(
            found[2].ends_with("POST /api/v1/uploads/{}/files"),
            "{found:?}"
        );
    }

    #[test]
    fn ignore_drops_a_listed_hit() {
        let config = api_config(serde_json::json!([]));
        let now = view(&config, &[("src/lib.rs", SERVER), ("web/api.ts", CLIENT)]);
        let found = hits(&config, &now).expect("hits");
        let orders = found
            .iter()
            .find(|hit| hit.key.ends_with("GET /api/v1/orders"))
            .expect("orders hit")
            .key
            .clone();
        let config = api_config(serde_json::json!([
            { "key": orders, "reason": "served by the gateway" }
        ]));
        let left = hits(&config, &now).expect("hits");
        assert_eq!(left.len(), found.len() - 1, "{left:?}");
        assert!(left.iter().all(|hit| hit.key != orders), "{left:?}");
    }

    #[test]
    fn ignore_needs_a_reason_and_unique_keys() {
        let rule = |ignore: serde_json::Value| {
            serde_json::json!({ "rules": [ {
                "id": "f", "kind": "forbidden", "selector": { "kind": "call" }, "ignore": ignore
            } ] })
            .to_string()
        };
        let empty = rule(serde_json::json!([{ "key": "a", "reason": " " }]));
        assert!(Config::from_bytes(empty.as_bytes()).is_err());
        let twice = rule(serde_json::json!([
            { "key": "a", "reason": "x" }, { "key": "a", "reason": "y" }
        ]));
        assert!(Config::from_bytes(twice.as_bytes()).is_err());
        let missing = rule(serde_json::json!([{ "key": "a" }]));
        assert!(Config::from_bytes(missing.as_bytes()).is_err());
        let good = rule(serde_json::json!([{ "key": "a", "reason": "x" }]));
        assert!(Config::from_bytes(good.as_bytes()).is_ok());
    }
}
