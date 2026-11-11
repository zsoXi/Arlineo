//! Graph construction and call resolution (`SPEC.md`, "Graph").

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::{
    ApiCallData, CallData, Confidence, DdlAction, Edge, EdgeKind, Fact, FactKind, FnDefData, Graph,
    ImportData, Lang, ModuleData, Node, NodeId, NodeKind, RelPath, RouteData, SqlFunctionData,
    SqlOp, TableAccessData, TableData, TriggerData, TypeDefData, compare_facts,
};

/// Most candidates a call may get as `guessed` edges; with more, the call gets no edge.
pub const MAX_GUESSES: usize = 8;

/// Names that never get `unique_name` or `guessed` edges (SPEC 12.3 step 7).
const COMMON_NAMES: [&str; 23] = [
    "clone",
    "to_string",
    "into",
    "from",
    "iter",
    "map",
    "unwrap",
    "push",
    "len",
    "get",
    "insert",
    "collect",
    "then",
    "catch",
    "forEach",
    "filter",
    "find",
    "join",
    "split",
    "toString",
    "parse",
    "json",
    "log",
];

/// Extensions tried when resolving a relative TypeScript specifier, in order.
const TS_EXTENSIONS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

/// A node before its index is known.
struct NodeCandidate {
    kind: NodeKind,
    key: String,
    file: RelPath,
    line: u32,
    /// For table nodes: the file and line came from a `create` fact.
    from_create: bool,
}

/// A decoded `fn_def` fact.
struct FnEntry {
    key: String,
    name: String,
    owner_type: Option<String>,
    lang: Option<Lang>,
}

/// A decoded call, or a route handler read as a call.
#[derive(Clone)]
struct CallInfo {
    owner: String,
    callee: String,
    name: String,
    qualifier: Vec<String>,
    receiver: Option<String>,
    is_method: bool,
    is_macro: bool,
    lang: Option<Lang>,
    line: u32,
}

/// A decoded `import` fact.
#[derive(Clone)]
struct ImportEntry {
    owner: String,
    path: String,
    name: String,
    alias: Option<String>,
    lang: Option<Lang>,
    line: u32,
}

/// A decoded `route` fact.
struct RouteEntry {
    key: String,
    method: String,
    norm_path: String,
    handler: Option<String>,
    owner: String,
    lang: Option<Lang>,
    line: u32,
}

/// A decoded `api_call` fact.
struct ApiCallEntry {
    key: String,
    norm_path: String,
    owner: String,
    line: u32,
}

/// A decoded `table_access` fact.
struct AccessEntry {
    owner: String,
    table: String,
    op: SqlOp,
    confidence: Confidence,
    line: u32,
}

/// A decoded `trigger` fact.
struct TriggerEntry {
    table: String,
    function: String,
    line: u32,
}

/// What is known about one function definition.
struct FnInfo {
    name: String,
    owner_type: Option<String>,
    module: Option<String>,
}

/// Lookup tables built once for call resolution and edge building.
struct Lookups {
    node_ids: NodeIds,
    fn_keys: BTreeSet<String>,
    fn_by_name: BTreeMap<String, BTreeSet<String>>,
    methods_by_name: BTreeMap<String, BTreeSet<String>>,
    fn_info: BTreeMap<String, FnInfo>,
    type_keys: BTreeSet<String>,
    module_keys: BTreeSet<String>,
    imports_by_module: BTreeMap<String, Vec<ImportEntry>>,
    nested_by_owner: BTreeMap<String, BTreeSet<String>>,
    crate_names: BTreeSet<String>,
}

impl Lookups {
    /// Id of the node of `kind` with `key`, if there is one.
    fn node_id(&self, kind: NodeKind, key: &str) -> Option<NodeId> {
        self.node_ids.get(&(kind.code(), key.to_owned())).copied()
    }
}

/// Builds nodes from definition facts and edges from calls, routes, API calls, table
/// accesses, triggers and imports, with confidence per edge. Deterministic: the same facts
/// give the same graph. Fails when the data of a fact does not decode for its kind.
pub fn build(facts: &[Fact]) -> Result<Graph> {
    // Work in `compare_facts` order so that "first fact" below does not depend on the
    // order in which the caller happened to store the same set of facts.
    let mut ordered: Vec<&Fact> = facts.iter().collect();
    ordered.sort_by(|a, b| compare_facts(a, b));

    let mut node_map: BTreeMap<(u8, String), NodeCandidate> = BTreeMap::new();
    let mut fn_defs: BTreeMap<String, FnEntry> = BTreeMap::new();
    // Every route fact, not one per key: two registrations of `GET /orders` (a backend and a
    // mock server) share the route node but each has its own handler.
    let mut route_defs: Vec<RouteEntry> = Vec::new();
    let mut calls: Vec<CallInfo> = Vec::new();
    let mut imports: Vec<ImportEntry> = Vec::new();
    let mut api_calls: Vec<ApiCallEntry> = Vec::new();
    let mut accesses: Vec<AccessEntry> = Vec::new();
    let mut triggers: Vec<TriggerEntry> = Vec::new();

    for fact in ordered {
        let lang = Lang::from_path(&fact.file);
        match fact.kind {
            FactKind::Module => {
                fact.data_as::<ModuleData>()?;
                insert_node(&mut node_map, NodeKind::Module, fact);
            }
            FactKind::FnDef => {
                let data = fact.data_as::<FnDefData>()?;
                insert_node(&mut node_map, NodeKind::Fn, fact);
                fn_defs.entry(fact.key.clone()).or_insert_with(|| FnEntry {
                    key: fact.key.clone(),
                    name: data.name.clone(),
                    owner_type: data.owner_type.clone(),
                    lang,
                });
            }
            FactKind::TypeDef => {
                fact.data_as::<TypeDefData>()?;
                insert_node(&mut node_map, NodeKind::Type, fact);
            }
            FactKind::SqlFunction => {
                fact.data_as::<SqlFunctionData>()?;
                insert_node(&mut node_map, NodeKind::SqlFunction, fact);
            }
            FactKind::Table => {
                let data = fact.data_as::<TableData>()?;
                insert_table_node(&mut node_map, fact, data.action == DdlAction::Create)
            }
            FactKind::Route => {
                let data = fact.data_as::<RouteData>()?;
                insert_node(&mut node_map, NodeKind::Route, fact);
                route_defs.push(RouteEntry {
                    key: fact.key.clone(),
                    method: data.method.clone(),
                    norm_path: data.norm_path.clone(),
                    handler: data.handler.clone(),
                    owner: data.owner.clone(),
                    lang,
                    line: fact.line,
                });
            }
            FactKind::Call => {
                let data = fact.data_as::<CallData>()?;
                calls.push(CallInfo {
                    owner: data.owner.clone(),
                    callee: data.callee.clone(),
                    name: data.name.clone(),
                    qualifier: data.qualifier.clone(),
                    receiver: data.receiver.clone(),
                    is_method: data.is_method,
                    is_macro: data.is_macro,
                    lang,
                    line: fact.line,
                })
            }
            FactKind::Import => {
                let data = fact.data_as::<ImportData>()?;
                imports.push(ImportEntry {
                    owner: data.owner.clone(),
                    path: data.path.clone(),
                    name: data.name.clone(),
                    alias: data.alias.clone(),
                    lang,
                    line: fact.line,
                })
            }
            FactKind::ApiCall => {
                let data = fact.data_as::<ApiCallData>()?;
                api_calls.push(ApiCallEntry {
                    key: fact.key.clone(),
                    norm_path: data.norm_path.clone(),
                    owner: data.owner.clone(),
                    line: fact.line,
                })
            }
            FactKind::TableAccess => {
                let data = fact.data_as::<TableAccessData>()?;
                accesses.push(AccessEntry {
                    owner: data.owner.clone(),
                    table: data.table.clone(),
                    op: data.op,
                    confidence: fact.confidence,
                    line: fact.line,
                })
            }
            FactKind::Trigger => {
                let data = fact.data_as::<TriggerData>()?;
                triggers.push(TriggerEntry {
                    table: data.table.clone(),
                    function: data.function.clone(),
                    line: fact.line,
                })
            }
            _ => {}
        }
    }
    // Fact data that does not decode is an error, not a skip: the extractors always write
    // decodable data, so a failure here is a bug or a damaged facts file the user must see.

    let (nodes, node_ids) = build_nodes(node_map)?;
    let lookups = build_lookups(&fn_defs, &imports, node_ids, &nodes);
    let mut edges: BTreeMap<(u32, u8, u32), Edge> = BTreeMap::new();

    add_contains_edges(&mut edges, &lookups, &nodes, &fn_defs);
    add_call_edges(&mut edges, &lookups, &calls);
    add_handle_edges(&mut edges, &lookups, &route_defs);
    add_request_edges(&mut edges, &lookups, &route_defs, &api_calls);
    add_access_edges(&mut edges, &lookups, &accesses);
    add_trigger_edges(&mut edges, &lookups, &triggers);
    add_import_edges(&mut edges, &lookups, &imports);

    let edges: Vec<Edge> = edges.into_values().collect();
    Graph::from_parts(nodes, edges)
}

// ---------------------------------------------------------------------------------------------
// Nodes

/// Adds a definition node, keeping the first fact in facts order for a duplicate key.
fn insert_node(map: &mut BTreeMap<(u8, String), NodeCandidate>, kind: NodeKind, fact: &Fact) {
    if let Entry::Vacant(slot) = map.entry((kind.code(), fact.key.clone())) {
        slot.insert(NodeCandidate {
            kind,
            key: fact.key.clone(),
            file: fact.file.clone(),
            line: fact.line,
            from_create: false,
        });
    }
}

/// Adds a table node: the first `create` fact wins, else the first fact for the key.
fn insert_table_node(
    map: &mut BTreeMap<(u8, String), NodeCandidate>,
    fact: &Fact,
    is_create: bool,
) {
    match map.entry((NodeKind::Table.code(), fact.key.clone())) {
        Entry::Vacant(slot) => {
            slot.insert(NodeCandidate {
                kind: NodeKind::Table,
                key: fact.key.clone(),
                file: fact.file.clone(),
                line: fact.line,
                from_create: is_create,
            });
        }
        Entry::Occupied(mut slot) => {
            let node = slot.get_mut();
            if is_create && !node.from_create {
                node.file = fact.file.clone();
                node.line = fact.line;
                node.from_create = true;
            }
        }
    }
}

/// Node id per `(NodeKind::code, key)`.
type NodeIds = BTreeMap<(u8, String), NodeId>;

/// Turns the ordered node map into the node vector and the key-to-id map.
fn build_nodes(node_map: BTreeMap<(u8, String), NodeCandidate>) -> Result<(Vec<Node>, NodeIds)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut node_ids: NodeIds = BTreeMap::new();
    for ((code, _), candidate) in node_map {
        let Some(id) = NodeId::from_index(nodes.len()) else {
            return Err(Error::Internal {
                message: format!("too many graph nodes: {}", nodes.len()),
            });
        };
        node_ids.insert((code, candidate.key.clone()), id);
        nodes.push(Node {
            key: candidate.key,
            kind: candidate.kind,
            file: Some(candidate.file),
            line: candidate.line,
        });
    }
    Ok((nodes, node_ids))
}

/// Builds every lookup table once, from the definitions and the finished node list.
fn build_lookups(
    fn_defs: &BTreeMap<String, FnEntry>,
    imports: &[ImportEntry],
    node_ids: NodeIds,
    nodes: &[Node],
) -> Lookups {
    let mut module_keys: BTreeSet<String> = BTreeSet::new();
    let mut type_keys: BTreeSet<String> = BTreeSet::new();
    for node in nodes {
        match node.kind {
            NodeKind::Module => {
                module_keys.insert(node.key.clone());
            }
            NodeKind::Type => {
                type_keys.insert(node.key.clone());
            }
            _ => {}
        }
    }

    let mut fn_keys: BTreeSet<String> = BTreeSet::new();
    let mut fn_by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut methods_by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut fn_info: BTreeMap<String, FnInfo> = BTreeMap::new();
    for entry in fn_defs.values() {
        fn_keys.insert(entry.key.clone());
        fn_by_name
            .entry(entry.name.clone())
            .or_default()
            .insert(entry.key.clone());
        if entry.owner_type.is_some() {
            methods_by_name
                .entry(entry.name.clone())
                .or_default()
                .insert(entry.key.clone());
        }
        let module = longest_module_prefix(&entry.key, &module_keys);
        fn_info.insert(
            entry.key.clone(),
            FnInfo {
                name: entry.name.clone(),
                owner_type: entry.owner_type.clone(),
                module,
            },
        );
    }

    let mut nested_by_owner: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in fn_defs.values() {
        if let Some((parent, _)) = entry.key.rsplit_once("::") {
            if fn_keys.contains(parent) {
                nested_by_owner
                    .entry(parent.to_owned())
                    .or_default()
                    .insert(entry.key.clone());
            }
        }
    }

    let mut imports_by_module: BTreeMap<String, Vec<ImportEntry>> = BTreeMap::new();
    for import in imports {
        imports_by_module
            .entry(import.owner.clone())
            .or_default()
            .push(import.clone());
    }

    let mut crate_names: BTreeSet<String> = BTreeSet::new();
    for key in &module_keys {
        if !key.contains('/') {
            if let Some(first) = key.split("::").next() {
                crate_names.insert(first.to_owned());
            }
        }
    }

    Lookups {
        node_ids,
        fn_keys,
        fn_by_name,
        methods_by_name,
        fn_info,
        type_keys,
        module_keys,
        imports_by_module,
        nested_by_owner,
        crate_names,
    }
}

/// Longest module key that is a `key` prefix ending at a `::` boundary.
fn longest_module_prefix(key: &str, module_keys: &BTreeSet<String>) -> Option<String> {
    let mut best: Option<&String> = None;
    for module in module_keys {
        let is_prefix = key.len() > module.len()
            && key.starts_with(module.as_str())
            && key
                .get(module.len()..)
                .is_some_and(|rest| rest.starts_with("::"));
        if is_prefix && best.is_none_or(|current| module.len() > current.len()) {
            best = Some(module);
        }
    }
    best.cloned()
}

/// Module a caller belongs to: its own key for module-level code, else the longest
/// module prefix of the function key.
fn caller_module(call: &CallInfo, lookups: &Lookups) -> Option<String> {
    if let Some(info) = lookups.fn_info.get(&call.owner) {
        if info.module.is_some() {
            return info.module.clone();
        }
    }
    if lookups.module_keys.contains(&call.owner) {
        return Some(call.owner.clone());
    }
    longest_module_prefix(&call.owner, &lookups.module_keys)
}

// ---------------------------------------------------------------------------------------------
// Edges: collection and deduplication

/// Adds one edge, deduplicated on `(from, to, kind)`, keeping the strongest
/// confidence and the lowest line (SPEC 12.2). Missing nodes give no edge.
fn add_edge(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    from: Option<NodeId>,
    to: Option<NodeId>,
    kind: EdgeKind,
    confidence: Confidence,
    line: u32,
) {
    let (Some(from), Some(to)) = (from, to) else {
        return;
    };
    // The map key is the edge order of SPEC 12.2: by from, kind, to.
    match edges.entry((from.raw(), kind.code(), to.raw())) {
        Entry::Vacant(slot) => {
            slot.insert(Edge {
                from,
                to,
                kind,
                confidence,
                line,
            });
        }
        Entry::Occupied(mut slot) => {
            let edge = slot.get_mut();
            edge.confidence = edge.confidence.min(confidence);
            edge.line = edge.line.min(line);
        }
    }
}

/// `contains`: module to its direct functions and types, type to its methods.
fn add_contains_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    nodes: &[Node],
    fn_defs: &BTreeMap<String, FnEntry>,
) {
    for node in nodes {
        if node.kind != NodeKind::Fn && node.kind != NodeKind::Type {
            continue;
        }
        // A direct child is the module key, `::` and a rest without `::`, so the only
        // candidate module is the key before the last `::`. Rust keys put a method under
        // its type (`M::T::name`), whose prefix is no module; TypeScript method keys
        // (`M::Class.name`) have no further `::` and are direct children.
        let Some((module, rest)) = node.key.rsplit_once("::") else {
            continue;
        };
        if !rest.is_empty() && lookups.module_keys.contains(module) {
            add_edge(
                edges,
                lookups.node_id(NodeKind::Module, module),
                lookups.node_id(node.kind, &node.key),
                EdgeKind::Contains,
                Confidence::Exact,
                0,
            );
        }
    }
    for entry in fn_defs.values() {
        if entry.owner_type.is_none() {
            continue;
        }
        for parent in method_parent_keys(entry) {
            add_edge(
                edges,
                lookups.node_id(NodeKind::Type, &parent),
                lookups.node_id(NodeKind::Fn, &entry.key),
                EdgeKind::Contains,
                Confidence::Exact,
                0,
            );
        }
    }
}

/// Type keys that could own a method: Rust `M::T` from `M::T::name`, TypeScript
/// `M::Class` from `M::Class.name`.
fn method_parent_keys(entry: &FnEntry) -> Vec<String> {
    let mut parents: Vec<String> = Vec::new();
    let separator = match entry.lang {
        Some(Lang::Rust) => Some("::"),
        Some(Lang::TypeScript) | Some(Lang::JavaScript) => Some("."),
        _ => None,
    };
    match separator {
        Some(sep) => {
            if let Some((parent, _)) = entry.key.rsplit_once(sep) {
                parents.push(parent.to_owned());
            }
        }
        None => {
            if let Some((parent, _)) = entry.key.rsplit_once("::") {
                parents.push(parent.to_owned());
            }
            if let Some((parent, _)) = entry.key.rsplit_once('.') {
                if !parents.iter().any(|known| known.as_str() == parent) {
                    parents.push(parent.to_owned());
                }
            }
        }
    }
    parents
}

/// `calls`: each call fact from its owner function to every resolved callee.
fn add_call_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    calls: &[CallInfo],
) {
    for call in calls {
        let Some(from) = lookups.node_id(NodeKind::Fn, &call.owner) else {
            continue;
        };
        for (target, confidence) in resolve_call(call, lookups) {
            add_edge(
                edges,
                Some(from),
                lookups.node_id(NodeKind::Fn, &target),
                EdgeKind::Calls,
                confidence,
                call.line,
            );
        }
    }
}

/// `handles`: the route handler text resolved as a call from the registering owner,
/// at most `heuristic` (SPEC 12.2).
fn add_handle_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    route_defs: &[RouteEntry],
) {
    for route in route_defs {
        let Some(handler) = route.handler.as_deref() else {
            continue;
        };
        let Some(call) = handler_as_call(handler, route) else {
            continue;
        };
        for (target, confidence) in resolve_call(&call, lookups) {
            add_edge(
                edges,
                lookups.node_id(NodeKind::Route, &route.key),
                lookups.node_id(NodeKind::Fn, &target),
                EdgeKind::Handles,
                confidence.weakest(Confidence::Heuristic),
                route.line,
            );
        }
    }
}

/// Reads handler text (`orders::create`, `create_order`) as a call by its owner.
fn handler_as_call(handler: &str, route: &RouteEntry) -> Option<CallInfo> {
    let text = handler.trim();
    if text.is_empty() {
        return None;
    }
    let segments: Vec<String> = if text.contains("::") {
        text.split("::").map(str::to_owned).collect()
    } else if text.contains('.') {
        text.split('.').map(str::to_owned).collect()
    } else {
        vec![text.to_owned()]
    };
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let name = segments.last()?.clone();
    let qualifier: Vec<String> = segments
        .iter()
        .take(segments.len().saturating_sub(1))
        .cloned()
        .collect();
    Some(CallInfo {
        owner: route.owner.clone(),
        callee: text.to_owned(),
        name,
        qualifier,
        receiver: None,
        is_method: false,
        is_macro: false,
        lang: route.lang,
        line: route.line,
    })
}

/// `requests`: API call owner to the route with the same key, else to every `ANY`
/// route with the same normalized path.
fn add_request_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    route_defs: &[RouteEntry],
    api_calls: &[ApiCallEntry],
) {
    for api_call in api_calls {
        let from = lookups
            .node_id(NodeKind::Fn, &api_call.owner)
            .or_else(|| lookups.node_id(NodeKind::Module, &api_call.owner));
        if from.is_none() {
            continue;
        }
        if let Some(to) = lookups.node_id(NodeKind::Route, &api_call.key) {
            add_edge(
                edges,
                from,
                Some(to),
                EdgeKind::Requests,
                Confidence::Heuristic,
                api_call.line,
            );
            continue;
        }
        for route in route_defs {
            if route.method == "ANY" && route.norm_path == api_call.norm_path {
                add_edge(
                    edges,
                    from,
                    lookups.node_id(NodeKind::Route, &route.key),
                    EdgeKind::Requests,
                    Confidence::Heuristic,
                    api_call.line,
                );
            }
        }
    }
}

/// `reads` and `writes` from table accesses.
fn add_access_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    accesses: &[AccessEntry],
) {
    for access in accesses {
        let kind = match access.op {
            SqlOp::Select | SqlOp::SelectForUpdate => EdgeKind::Reads,
            _ => EdgeKind::Writes,
        };
        add_edge(
            edges,
            access_owner_node(lookups, &access.owner),
            lookups.node_id(NodeKind::Table, &access.table),
            kind,
            access.confidence,
            access.line,
        );
    }
}

/// Node running a statement: function, SQL function, view table, or module for
/// top-level code. A `<file>#do@<line>` owner has no node.
fn access_owner_node(lookups: &Lookups, owner: &str) -> Option<NodeId> {
    if owner.contains("#do@") {
        return None;
    }
    lookups
        .node_id(NodeKind::Fn, owner)
        .or_else(|| lookups.node_id(NodeKind::SqlFunction, owner))
        .or_else(|| lookups.node_id(NodeKind::Table, owner))
        .or_else(|| lookups.node_id(NodeKind::Module, owner))
}

/// `triggers`: table to its trigger function.
fn add_trigger_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    triggers: &[TriggerEntry],
) {
    for trigger in triggers {
        let function_key = if trigger.function.starts_with("sql:") {
            trigger.function.clone()
        } else {
            format!("sql:{}", trigger.function)
        };
        add_edge(
            edges,
            lookups.node_id(NodeKind::Table, &trigger.table),
            lookups.node_id(NodeKind::SqlFunction, &function_key),
            EdgeKind::Triggers,
            Confidence::Exact,
            trigger.line,
        );
    }
}

/// `imports`: module to the module it imports from, when that resolves.
fn add_import_edges(
    edges: &mut BTreeMap<(u32, u8, u32), Edge>,
    lookups: &Lookups,
    imports: &[ImportEntry],
) {
    for import in imports {
        let target = match import.lang {
            Some(Lang::Rust) => rust_import_module(import, lookups),
            Some(Lang::TypeScript) | Some(Lang::JavaScript) => {
                resolve_ts_module(&import.owner, &import.path, lookups)
            }
            _ => rust_import_module(import, lookups)
                .or_else(|| resolve_ts_module(&import.owner, &import.path, lookups)),
        };
        add_edge(
            edges,
            lookups.node_id(NodeKind::Module, &import.owner),
            target.and_then(|key| lookups.node_id(NodeKind::Module, &key)),
            EdgeKind::Imports,
            Confidence::Exact,
            import.line,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Call resolution (SPEC 12.3, steps in order)

/// Resolves one call to function keys with a confidence each. SQL functions are
/// never targets here (step 8): only `fn` nodes are in the lookup tables.
fn resolve_call(call: &CallInfo, lookups: &Lookups) -> Vec<(String, Confidence)> {
    let module = caller_module(call, lookups);
    let module_ref = module.as_deref();

    // Step 6 is checked first in code only to route method calls on other
    // receivers; step 4 (TypeScript imports) still applies to them, and after
    // it they go straight to step 5, restricted as step 6 says.
    if is_other_method_call(call) {
        if let Some(target) = resolve_step4_ts_import(call, module_ref, lookups) {
            return vec![(target, Confidence::Exact)];
        }
        if exact_only(call) {
            return Vec::new();
        }
        return resolve_step5_by_name(call, true, lookups);
    }

    if let Some(target) = resolve_step1_self(call, lookups) {
        return vec![(target, Confidence::Exact)];
    }
    if let Some(target) = resolve_step2_rust_qualified(call, module_ref, lookups) {
        return vec![(target, Confidence::Exact)];
    }
    if let Some(target) = resolve_step3_bare(call, module_ref, lookups) {
        return vec![(target, Confidence::Exact)];
    }
    if let Some(target) = resolve_step4_ts_import(call, module_ref, lookups) {
        return vec![(target, Confidence::Exact)];
    }
    // Step 7: macros, `new Name` and common names keep only the exact edges above.
    if exact_only(call) {
        return Vec::new();
    }
    resolve_step5_by_name(call, call.is_method, lookups)
}

/// Step 7 gate: macros, calls to `new Name`, and common standard library names.
fn exact_only(call: &CallInfo) -> bool {
    call.is_macro || call.callee.starts_with("new ") || COMMON_NAMES.contains(&call.name.as_str())
}

/// A method call whose receiver is not `self` or `this` (step 6).
fn is_other_method_call(call: &CallInfo) -> bool {
    call.is_method && !matches!(call.receiver.as_deref(), Some("self") | Some("this"))
}

/// Step 1: Rust `self.name(...)` and `Self::name(...)`, TypeScript `this.name(...)`.
fn resolve_step1_self(call: &CallInfo, lookups: &Lookups) -> Option<String> {
    let info = lookups.fn_info.get(&call.owner)?;
    let self_receiver =
        matches!(call.receiver.as_deref(), Some("self") | Some("this")) && call.is_method;
    let self_qualifier =
        call.qualifier.len() == 1 && call.qualifier.first().is_some_and(|head| head == "Self");
    if !self_receiver && !self_qualifier {
        return None;
    }
    let parents = method_parent_keys(&FnEntry {
        key: call.owner.clone(),
        name: info.name.clone(),
        owner_type: info.owner_type.clone(),
        lang: call.lang,
    });
    for parent in parents {
        // Inside `impl T` (or a class): the owner must be a method of a known
        // type, and the sibling key joins the type key and the called name.
        if info.owner_type.is_none() && !lookups.type_keys.contains(&parent) {
            continue;
        }
        let candidate = match call.lang {
            Some(Lang::TypeScript) | Some(Lang::JavaScript) => format!("{parent}.{}", call.name),
            _ => format!("{parent}::{}", call.name),
        };
        if lookups.fn_keys.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Step 2: qualified Rust calls, resolved through `crate::`, `self::`, `super::`,
/// a `use` import, or a crate name, in that order.
fn resolve_step2_rust_qualified(
    call: &CallInfo,
    module: Option<&str>,
    lookups: &Lookups,
) -> Option<String> {
    if matches!(
        call.lang,
        Some(Lang::TypeScript) | Some(Lang::JavaScript) | Some(Lang::Sql)
    ) {
        return None;
    }
    let head = call.qualifier.first()?;
    if head == "Self" {
        return None;
    }
    let module = module?;
    let mut candidates: Vec<String> = Vec::new();
    if head == "crate" || head == "self" || head == "super" {
        if let Some(resolved) = resolve_rust_segments(&call.qualifier, Some(module)) {
            candidates.push(format!("{}::{}", resolved.join("::"), call.name));
        }
    } else {
        let matching: Vec<&ImportEntry> = lookups
            .imports_by_module
            .get(module)
            .into_iter()
            .flatten()
            .filter(|import| import.name != "*" && local_name(import) == head.as_str())
            .collect();
        if !matching.is_empty() {
            for import in matching {
                for target in rust_import_target_candidates(import, Some(module), lookups) {
                    let mut candidate = target;
                    for segment in call.qualifier.iter().skip(1) {
                        candidate.push_str("::");
                        candidate.push_str(segment);
                    }
                    candidate.push_str("::");
                    candidate.push_str(&call.name);
                    candidates.push(candidate);
                }
            }
        } else {
            candidates.push(format!("{}::{}", call.qualifier.join("::"), call.name));
            // A path is relative to the calling module: `Type::name` is read as
            // `M::Type::name` and `handlers::orders::list` as `M::handlers::orders::list`.
            candidates.push(format!(
                "{module}::{}::{}",
                call.qualifier.join("::"),
                call.name
            ));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| lookups.fn_keys.contains(candidate))
}

/// Step 3: bare calls: nested in the owner, then in the module, then brought in
/// by a `use` or `import` in the module.
fn resolve_step3_bare(call: &CallInfo, module: Option<&str>, lookups: &Lookups) -> Option<String> {
    if !call.qualifier.is_empty() || call.is_method {
        return None;
    }
    let nested = format!("{}::{}", call.owner, call.name);
    if lookups
        .nested_by_owner
        .get(&call.owner)
        .is_some_and(|keys| keys.contains(&nested))
    {
        return Some(nested);
    }
    let module = module?;
    let in_module = format!("{module}::{}", call.name);
    if lookups.fn_keys.contains(&in_module) {
        return Some(in_module);
    }
    match call.lang {
        Some(Lang::Rust) => resolve_rust_import_bare(call, module, lookups),
        Some(Lang::TypeScript) | Some(Lang::JavaScript) => {
            resolve_ts_import_bare(call, module, lookups)
        }
        _ => resolve_rust_import_bare(call, module, lookups)
            .or_else(|| resolve_ts_import_bare(call, module, lookups)),
    }
}

/// Step 4: TypeScript calls through an import, including namespace imports.
fn resolve_step4_ts_import(
    call: &CallInfo,
    module: Option<&str>,
    lookups: &Lookups,
) -> Option<String> {
    if matches!(call.lang, Some(Lang::Rust) | Some(Lang::Sql)) {
        return None;
    }
    let module = module?;
    // The imported name is the receiver of `ns.name(...)`, or the first
    // qualifier segment of `ns.name(...)` as a dotted callee.
    let (local, rest): (&str, Vec<String>) = match call.qualifier.first() {
        Some(head) => (
            head.as_str(),
            call.qualifier.iter().skip(1).cloned().collect(),
        ),
        None => {
            let receiver = call.receiver.as_deref()?;
            (receiver, Vec::new())
        }
    };
    let imports = lookups.imports_by_module.get(module)?;
    for import in imports {
        if import.name == "*" {
            if import.alias.as_deref() != Some(local) {
                continue;
            }
            let Some(target_module) = resolve_ts_module(module, &import.path, lookups) else {
                continue;
            };
            for candidate in ts_key_candidates(&target_module, &rest, &call.name) {
                if lookups.fn_keys.contains(&candidate) {
                    return Some(candidate);
                }
            }
        } else if local_name(import) == local {
            let Some(target_module) = resolve_ts_module(module, &import.path, lookups) else {
                continue;
            };
            let base = format!("{target_module}::{}", import.name);
            for candidate in ts_member_candidates(&base, &rest, &call.name) {
                if lookups.fn_keys.contains(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Function keys in a TypeScript module: `M::name`, class methods `M::C.name`,
/// nested functions under a `::` path.
fn ts_key_candidates(module: &str, rest: &[String], name: &str) -> Vec<String> {
    if rest.is_empty() {
        return vec![format!("{module}::{name}")];
    }
    vec![
        format!("{module}::{}.{}", rest.join("."), name),
        format!("{module}::{}::{name}", rest.join("::")),
    ]
}

/// Function keys under an imported TypeScript name: methods with `.`, nested
/// functions with `::`.
fn ts_member_candidates(base: &str, rest: &[String], name: &str) -> Vec<String> {
    if rest.is_empty() {
        return vec![format!("{base}.{name}"), format!("{base}::{name}")];
    }
    vec![
        format!("{base}.{}.{}", rest.join("."), name),
        format!("{base}::{}::{name}", rest.join("::")),
    ]
}

/// Step 5: every function with this last name. One is `unique_name`, up to
/// `MAX_GUESSES` are `guessed`, more give no edge. Step 6 passes
/// `restrict_to_methods`, keeping only methods when any method has the name.
fn resolve_step5_by_name(
    call: &CallInfo,
    restrict_to_methods: bool,
    lookups: &Lookups,
) -> Vec<(String, Confidence)> {
    let all = lookups.fn_by_name.get(&call.name);
    let candidates = if restrict_to_methods {
        match lookups.methods_by_name.get(&call.name) {
            Some(methods) if !methods.is_empty() => Some(methods),
            _ => all,
        }
    } else {
        all
    };
    let Some(candidates) = candidates else {
        return Vec::new();
    };
    match candidates.len() {
        1 => candidates
            .iter()
            .map(|key| (key.clone(), Confidence::UniqueName))
            .collect(),
        2..=MAX_GUESSES => candidates
            .iter()
            .map(|key| (key.clone(), Confidence::Guessed))
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Import resolution helpers

/// Local name an import brings in: its alias, else the imported name.
fn local_name(import: &ImportEntry) -> &str {
    import.alias.as_deref().unwrap_or(&import.name)
}

/// Splits a Rust path as written into segments.
fn split_rust_path(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    path.split("::")
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Resolves Rust path segments against a module: `crate`, `self` and `super`
/// are replaced by module segments. `super` above the crate root does not
/// resolve. Other paths are kept as written.
fn resolve_rust_segments(segments: &[String], module: Option<&str>) -> Option<Vec<String>> {
    let head = segments.first()?;
    match head.as_str() {
        "crate" => {
            let module = module?;
            let crate_name = module.split("::").next()?;
            let mut resolved: Vec<String> = vec![crate_name.to_owned()];
            resolved.extend(segments.iter().skip(1).cloned());
            Some(resolved)
        }
        "self" => {
            let module = module?;
            let mut resolved: Vec<String> = split_rust_path(module);
            resolved.extend(segments.iter().skip(1).cloned());
            Some(resolved)
        }
        "super" => {
            let module = module?;
            let mut resolved: Vec<String> = split_rust_path(module);
            let mut index = 0usize;
            while segments
                .get(index)
                .is_some_and(|segment| segment == "super")
            {
                // The crate segment is the root: popping it leaves the crate.
                if resolved.len() <= 1 {
                    return None;
                }
                resolved.pop();
                index = index.saturating_add(1);
            }
            resolved.extend(segments.iter().skip(index).cloned());
            Some(resolved)
        }
        _ => Some(segments.to_vec()),
    }
}

/// Candidate absolute keys for Rust path segments: resolved as written, then
/// relative to the module when the head is not special and not a crate name.
fn rust_path_candidates(
    segments: &[String],
    module: Option<&str>,
    lookups: &Lookups,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if segments.is_empty() {
        return candidates;
    }
    if let Some(resolved) = resolve_rust_segments(segments, module) {
        candidates.push(resolved.join("::"));
    }
    if let (Some(module), Some(head)) = (module, segments.first()) {
        let special = head == "crate" || head == "self" || head == "super";
        if !special && !lookups.crate_names.contains(head) {
            let relative = format!("{module}::{}", segments.join("::"));
            if !candidates.contains(&relative) {
                candidates.push(relative);
            }
        }
    }
    candidates
}

/// Candidate keys of the item a Rust import brings in (`path` plus `name`).
fn rust_import_target_candidates(
    import: &ImportEntry,
    module: Option<&str>,
    lookups: &Lookups,
) -> Vec<String> {
    let mut segments = split_rust_path(&import.path);
    if import.name != "*" {
        segments.push(import.name.clone());
    }
    rust_path_candidates(&segments, module, lookups)
}

/// A Rust bare call brought in by a `use` in the module (part of step 3).
fn resolve_rust_import_bare(call: &CallInfo, module: &str, lookups: &Lookups) -> Option<String> {
    let imports = lookups.imports_by_module.get(module)?;
    for import in imports {
        if import.name == "*" {
            // A glob import brings every name of its module.
            for base in rust_path_candidates(&split_rust_path(&import.path), Some(module), lookups)
            {
                let candidate = format!("{base}::{}", call.name);
                if lookups.fn_keys.contains(&candidate) {
                    return Some(candidate);
                }
            }
        } else if local_name(import) == call.name.as_str() {
            for target in rust_import_target_candidates(import, Some(module), lookups) {
                if lookups.fn_keys.contains(&target) {
                    return Some(target);
                }
            }
        }
    }
    None
}

/// A TypeScript bare call brought in by an `import` in the module (step 3/4).
fn resolve_ts_import_bare(call: &CallInfo, module: &str, lookups: &Lookups) -> Option<String> {
    let imports = lookups.imports_by_module.get(module)?;
    for import in imports {
        if import.name == "*" || local_name(import) != call.name.as_str() {
            continue;
        }
        let Some(target_module) = resolve_ts_module(module, &import.path, lookups) else {
            continue;
        };
        let candidate = format!("{target_module}::{}", import.name);
        if lookups.fn_keys.contains(&candidate) {
            return Some(candidate);
        }
        // `export default function list()` is keyed by its own name, which is usually
        // the name it is imported under.
        if import.name == "default" {
            let named = format!("{target_module}::{}", call.name);
            if lookups.fn_keys.contains(&named) {
                return Some(named);
            }
        }
    }
    None
}

/// Module a Rust import comes from: its target when that is a module, else the
/// longest module prefix of the imported item key.
fn rust_import_module(import: &ImportEntry, lookups: &Lookups) -> Option<String> {
    let module = Some(import.owner.as_str());
    let mut segments = split_rust_path(&import.path);
    if import.name != "*" {
        segments.push(import.name.clone());
    }
    for target in rust_path_candidates(&segments, module, lookups) {
        if lookups.module_keys.contains(&target) {
            return Some(target);
        }
        // The relative candidate of an external path (`M::std::fmt` for `use std::fmt`)
        // has the importing module itself as prefix; that is not an import of it.
        if let Some(prefix) = longest_module_prefix(&target, &lookups.module_keys)
            .filter(|prefix| prefix != &import.owner)
        {
            return Some(prefix);
        }
    }
    None
}

/// Resolves a relative TypeScript specifier against the importing file's
/// directory, with the extensions and `/index.*` forms of SPEC 12.3 step 4.
/// A specifier that would leave the repository root does not resolve.
fn resolve_ts_module(importing: &str, specifier: &str, lookups: &Lookups) -> Option<String> {
    if lookups.module_keys.contains(specifier) {
        return Some(specifier.to_owned());
    }
    let relative = specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier == "."
        || specifier == "..";
    if !relative {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if let Some((dir, _)) = importing.rsplit_once('/') {
        parts.extend(dir.split('/').filter(|segment| !segment.is_empty()));
    }
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Never outside the root.
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    let base = parts.join("/");
    let mut candidates: Vec<String> = vec![base.clone()];
    for ext in TS_EXTENSIONS {
        candidates.push(format!("{base}.{ext}"));
    }
    for ext in TS_EXTENSIONS {
        candidates.push(format!("{base}/index.{ext}"));
    }
    // TypeScript ESM imports name the emitted file: `./x.js` is the source `./x.ts`.
    let emitted_sources: [(&str, &[&str]); 4] = [
        (".js", &["ts", "tsx"]),
        (".jsx", &["tsx"]),
        (".mjs", &["mts"]),
        (".cjs", &["cts"]),
    ];
    for (emitted, sources) in emitted_sources {
        if let Some(stem) = base.strip_suffix(emitted) {
            for ext in sources {
                candidates.push(format!("{stem}.{ext}"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|candidate| lookups.module_keys.contains(candidate))
}

#[cfg(test)]
mod tests {
    use super::build;
    use crate::config::Config;
    use crate::extract::test_support::{facts, facts_with};
    use crate::model::{EdgeKind, FactKind, NodeKind, sort_facts};

    #[test]
    fn undecodable_fact_data_is_an_error() {
        let mut all = facts("src/lib.rs", "pub fn run() {}\n");
        let fact = all
            .iter_mut()
            .find(|fact| fact.kind == FactKind::Module)
            .expect("module fact");
        fact.kind = FactKind::FnDef;
        assert!(build(&all).is_err());
    }

    #[test]
    fn every_route_registration_gets_its_handler() {
        let config = Config::from_bytes(
            br#"{ "routes": [ {
                "id": "router", "lang": "rust", "call": "route",
                "methods": { "get": "GET" }
            } ] }"#,
        )
        .expect("test configuration");
        let mut all = facts_with(
            &config,
            "src/server.rs",
            "pub fn real() {}\npub fn app() { Router::new().route(\"/x\", get(real)); }\n",
        );
        all.extend(facts_with(
            &config,
            "src/mock.rs",
            "pub fn fake() {}\npub fn app() { Router::new().route(\"/x\", get(fake)); }\n",
        ));
        sort_facts(&mut all);
        let graph = build(&all).expect("graph");
        let route = graph.lookup(NodeKind::Route, "GET /x").expect("route node");
        let mut handlers: Vec<String> = graph
            .outgoing(route)
            .filter(|edge| edge.kind == EdgeKind::Handles)
            .filter_map(|edge| graph.node(edge.to).map(|node| node.key.clone()))
            .collect();
        handlers.sort();
        assert_eq!(
            handlers,
            vec!["app::mock::fake".to_owned(), "app::server::real".to_owned()]
        );
    }
}
