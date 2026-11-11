//! `arlineo.json`: file selection, route and API client patterns, and rules
//! (`SPEC.md`, "Configuration"). The file is data only; nothing in it is ever executed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::{Fact, FactKind, Lang, RelPath};
use crate::util::glob::{Glob, GlobSet};

/// Name of the configuration file at the repository root.
pub const CONFIG_FILE: &str = "arlineo.json";

/// Environment variable with the absolute path of a configuration file used instead of the
/// repository's own, so rules can live outside a repository that must not be changed.
pub const CONFIG_ENV: &str = "ARLINEO_CONFIG";

/// Language a pattern applies to; `ts` covers TypeScript and JavaScript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatternLang {
    Rust,
    Ts,
}

/// A pattern that recognizes route registrations in Rust code:
/// `call(path, get(handler).post(handler))`, where the HTTP method comes from wrapper calls.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePattern {
    pub id: String,
    #[serde(default)]
    pub note: Option<String>,
    pub lang: PatternLang,
    /// Name of the registering call, e.g. `route`.
    pub call: String,
    /// Index of the path string argument.
    #[serde(default)]
    pub path_arg: usize,
    /// Wrapper name to HTTP method; the value is an uppercase method or `ANY`.
    pub methods: BTreeMap<String, String>,
    /// Prefix put in front of every path found by this pattern.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Path the found paths are relative to: put in front of a path (after `prefix`) unless
    /// the path already starts with it.
    #[serde(default)]
    pub base_path: Option<String>,
}

/// A pattern that recognizes HTTP calls from client code.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiClientPattern {
    pub id: String,
    #[serde(default)]
    pub note: Option<String>,
    pub lang: PatternLang,
    /// Called function or method name, e.g. `request`.
    pub call: String,
    /// Empty: only a bare call `call(...)`. `["*"]`: a method call on any receiver.
    /// Otherwise: a method call on one of these receiver identifiers.
    #[serde(default)]
    pub receivers: Vec<String>,
    /// Index of the path argument (string or template literal).
    #[serde(default)]
    pub path_arg: usize,
    /// Fixed HTTP method.
    #[serde(default)]
    pub method: Option<String>,
    /// Index of a string argument that holds the method, e.g. `request("POST", path)`.
    #[serde(default)]
    pub method_arg: Option<usize>,
    /// Key of an object literal argument that holds the method, e.g. `{ method: "POST" }`.
    #[serde(default)]
    pub method_option: Option<String>,
    /// Method when none of the above gives one.
    #[serde(default)]
    pub default_method: Option<String>,
    /// Prefix removed from the path before normalizing, e.g. `/api`.
    #[serde(default)]
    pub strip_prefix: Option<String>,
    /// Path the called paths are relative to: put in front of a path (after `strip_prefix`)
    /// unless the path already starts with it.
    #[serde(default)]
    pub base_path: Option<String>,
}

/// Kind of a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleKind {
    RequiresBefore,
    OrderInFunction,
    SourcesAgree,
    HasTarget,
    Forbidden,
    ReviewOnChange,
}

/// Which side of a `sources-agree` rule must have its tuples on the other side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Both sides (equal sets).
    #[default]
    Both,
    /// Only the left side: every left tuple is also on the right.
    LeftInRight,
}

/// A fact selector as written in the file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSelector {
    /// A comment; ignored.
    #[serde(default)]
    pub note: Option<String>,
    pub kind: FactKind,
    /// Glob over the fact key.
    #[serde(default)]
    pub key: Option<String>,
    /// Glob over the fact file.
    #[serde(default)]
    pub file: Option<String>,
    /// Data field name to glob over the field's text (`Fact::data_text`).
    #[serde(default)]
    pub data: BTreeMap<String, String>,
}

/// A rule as written in the file. Which fields are required depends on `kind`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRule {
    pub id: String,
    #[serde(default)]
    pub note: Option<String>,
    pub kind: RuleKind,
    #[serde(default)]
    pub when: Option<RawSelector>,
    #[serde(default)]
    pub require: Option<RawSelector>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub include_ancestor_callees: Option<bool>,
    #[serde(default)]
    pub all_paths: Option<bool>,
    #[serde(default)]
    pub first: Option<RawSelector>,
    #[serde(default)]
    pub then: Option<RawSelector>,
    #[serde(default)]
    pub only_with_first: Option<bool>,
    #[serde(default)]
    pub left: Option<RawSelector>,
    #[serde(default)]
    pub right: Option<RawSelector>,
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub minus: Option<RawSelector>,
    #[serde(default)]
    pub dropped: Option<RawSelector>,
    #[serde(default)]
    pub source: Option<RawSelector>,
    #[serde(default)]
    pub target: Option<RawSelector>,
    #[serde(default)]
    pub source_field: Option<String>,
    #[serde(default)]
    pub target_field: Option<String>,
    #[serde(default)]
    pub selector: Option<RawSelector>,
    #[serde(default)]
    pub ignore: Option<Vec<IgnoreEntry>>,
}

/// One hit a rule no longer reports: its hit key and why it is not a violation.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreEntry {
    pub key: String,
    pub reason: String,
}

/// The file as written.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub routes: Vec<RoutePattern>,
    #[serde(default)]
    pub api_clients: Vec<ApiClientPattern>,
    #[serde(default)]
    pub rules: Vec<RawRule>,
}

/// A compiled fact selector.
#[derive(Clone, Debug)]
pub struct Selector {
    pub kind: FactKind,
    pub key: Option<Glob>,
    pub file: Option<Glob>,
    pub data: Vec<(String, Glob)>,
}

impl Selector {
    /// Compiles a raw selector; `Error::Config` for an invalid glob.
    pub fn compile(raw: &RawSelector) -> Result<Selector> {
        let key = match &raw.key {
            Some(pattern) => Some(Glob::new(pattern)?),
            None => None,
        };
        let file = match &raw.file {
            Some(pattern) => Some(Glob::new(pattern)?),
            None => None,
        };
        let mut data = Vec::new();
        for (field, pattern) in &raw.data {
            data.push((field.clone(), Glob::new(pattern)?));
        }
        Ok(Selector {
            kind: raw.kind,
            key,
            file,
            data,
        })
    }

    /// Whether the fact has the selector's kind and matches every given glob. A data field
    /// that is missing or null never matches.
    pub fn matches(&self, fact: &Fact) -> bool {
        if fact.kind != self.kind {
            return false;
        }
        if let Some(glob) = &self.key {
            if !glob.is_match(&fact.key) {
                return false;
            }
        }
        if let Some(glob) = &self.file {
            if !glob.is_match(fact.file.as_str()) {
                return false;
            }
        }
        for (field, glob) in &self.data {
            match fact.data_text(field) {
                Some(text) => {
                    if !glob.is_match(&text) {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }
}

/// A validated rule.
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "built once per rule when the config is read"
)]
pub enum RuleSpec {
    RequiresBefore {
        when: Selector,
        require: Selector,
        depth: u32,
        include_ancestor_callees: bool,
        /// Every caller chain (not guessed edges) must take the lock before the call that
        /// leads to the `when` fact, in a function on the chain.
        all_paths: bool,
    },
    OrderInFunction {
        first: Selector,
        then: Selector,
        /// Only owners that also have a `first` fact are checked (wrong order only).
        only_with_first: bool,
    },
    SourcesAgree {
        left: Selector,
        right: Selector,
        fields: Vec<String>,
        direction: Direction,
        /// Facts that cancel an earlier left fact with the same tuple (by file name, then line),
        /// e.g. a `REVOKE` in a later migration.
        minus: Option<Selector>,
        /// Facts whose key names an object that ends at their position, e.g. a `DROP TABLE` in a
        /// later migration: they cancel every earlier left fact on that object.
        dropped: Option<Selector>,
    },
    HasTarget {
        source: Selector,
        target: Selector,
        source_field: String,
        target_field: String,
    },
    Forbidden {
        selector: Selector,
    },
    ReviewOnChange {
        selector: Selector,
    },
}

/// A validated rule with its id and note.
#[derive(Clone, Debug)]
pub struct Rule {
    pub id: String,
    pub note: Option<String>,
    pub spec: RuleSpec,
    /// Hit keys from `ignore`; each had a non-empty reason.
    pub ignore: BTreeSet<String>,
}

/// The validated configuration. Build it only with [`Config::load`], [`Config::from_bytes`]
/// or [`Config::defaults`].
#[derive(Clone, Debug)]
pub struct Config {
    /// Base line ref from the file, if any.
    pub base: Option<String>,
    pub include: GlobSet,
    pub exclude: GlobSet,
    pub routes: Vec<RoutePattern>,
    pub api_clients: Vec<ApiClientPattern>,
    pub rules: Vec<Rule>,
    /// FNV-1a 64 hex of the parts that change extracted facts (`include`, `exclude`,
    /// `routes`, `api_clients`, without `note` fields), so rules, notes and
    /// `base` can differ between worktrees without rebuilding indexes. Indexes built with
    /// another hash are rebuilt.
    pub hash: String,
}

impl Config {
    /// Reads the file named by `$ARLINEO_CONFIG` when the variable is set and not empty, else
    /// `<repo_top>/arlineo.json`, or returns [`Config::defaults`] when the repository has no
    /// file. A file named by the variable must exist, and the path must be absolute.
    pub fn load(repo_top: &Path) -> Result<Config> {
        if let Some(value) = std::env::var_os(CONFIG_ENV).filter(|value| !value.is_empty()) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(Error::Config {
                    message: format!("{CONFIG_ENV} must be an absolute path"),
                });
            }
            let bytes = std::fs::read(&path).map_err(|source| {
                Error::io(format!("reading {CONFIG_ENV} {}", path.display()), source)
            })?;
            return Config::from_bytes(&bytes);
        }
        match std::fs::read(repo_top.join(CONFIG_FILE)) {
            Ok(bytes) => Config::from_bytes(&bytes),
            Err(source) if source.kind() == ErrorKind::NotFound => Config::defaults(),
            Err(source) => Err(Error::io("reading arlineo.json", source)),
        }
    }

    /// Parses and validates file bytes (`Error::Config` with the problem, `Error::Json` for
    /// syntax errors).
    pub fn from_bytes(bytes: &[u8]) -> Result<Config> {
        let raw: RawConfig =
            serde_json::from_slice(bytes).map_err(|source| Error::json("arlineo.json", source))?;
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|source| Error::json("arlineo.json", source))?;
        let hash = extraction_hash(&raw, Some(&value))?;
        validate_pattern_ids(&raw.routes, &raw.api_clients)?;
        for (index, route) in raw.routes.iter().enumerate() {
            validate_route(index, route)?;
        }
        for (index, client) in raw.api_clients.iter().enumerate() {
            validate_api_client(index, client)?;
        }
        validate_rule_ids(&raw.rules)?;
        let mut rules = Vec::new();
        for (index, rule) in raw.rules.iter().enumerate() {
            rules.push(build_rule(index, rule)?);
        }
        let include = compile_glob_set(&raw.include, "include")?;
        let exclude = compile_glob_set(&raw.exclude, "exclude")?;
        Ok(Config {
            base: raw.base,
            include,
            exclude,
            routes: raw.routes,
            api_clients: raw.api_clients,
            rules,
            hash,
        })
    }

    /// Configuration without a file: no route or API client patterns, no rules, every
    /// supported file.
    pub fn defaults() -> Result<Config> {
        Ok(Config {
            base: None,
            include: GlobSet::default(),
            exclude: GlobSet::default(),
            routes: Vec::new(),
            api_clients: Vec::new(),
            rules: Vec::new(),
            hash: extraction_hash(&RawConfig::default(), None)?,
        })
    }

    /// Whether a file is indexed: a known language, matched by `include` (empty means every
    /// file), not matched by `exclude`, and not a secret file (`source::is_secret_path`).
    pub fn wants(&self, path: &RelPath) -> bool {
        Lang::from_path(path).is_some()
            && (self.include.is_empty() || self.include.is_match(path.as_str()))
            && !self.exclude.is_match(path.as_str())
            && !crate::source::is_secret_path(path)
    }
}

fn object_prefix(collection: &str, index: usize, id: &str) -> String {
    format!("{collection}[{index}] {id:?}")
}

fn field_error(object: &str, field: &str, message: &str) -> Error {
    Error::Config {
        message: format!("{object}: field {field:?} {message}"),
    }
}

fn prefixed_error(prefix: &str, error: Error) -> Error {
    match error {
        Error::Config { message } => Error::Config {
            message: format!("{prefix}: {message}"),
        },
        other => other,
    }
}

fn valid_method(value: &str) -> bool {
    value == "ANY" || (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_uppercase()))
}

fn validate_pattern_id(object: &str, id: &str, seen: &mut BTreeSet<String>) -> Result<()> {
    if id.is_empty() {
        return Err(field_error(object, "id", "must not be empty"));
    }
    if !seen.insert(id.to_owned()) {
        return Err(field_error(object, "id", "is a duplicate pattern id"));
    }
    Ok(())
}

fn validate_pattern_ids(routes: &[RoutePattern], clients: &[ApiClientPattern]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (index, route) in routes.iter().enumerate() {
        let object = object_prefix("routes", index, &route.id);
        validate_pattern_id(&object, &route.id, &mut seen)?;
    }
    for (index, client) in clients.iter().enumerate() {
        let object = object_prefix("api_clients", index, &client.id);
        validate_pattern_id(&object, &client.id, &mut seen)?;
    }
    Ok(())
}

fn validate_route(index: usize, route: &RoutePattern) -> Result<()> {
    let object = object_prefix("routes", index, &route.id);
    // Only the Rust extractor reads routes; TypeScript code only makes API calls.
    if route.lang != PatternLang::Rust {
        return Err(field_error(&object, "lang", "must be \"rust\" for a route"));
    }
    if route.methods.is_empty() {
        return Err(field_error(&object, "methods", "must not be empty"));
    }
    for (name, method) in &route.methods {
        if name.is_empty() {
            return Err(field_error(
                &object,
                "methods",
                "must not have an empty key",
            ));
        }
        if !valid_method(method) {
            return Err(Error::Config {
                message: format!(
                    "{object}: field \"methods\" value {method:?} for key {name:?} must be uppercase ASCII letters or ANY"
                ),
            });
        }
    }
    if route.call.is_empty() {
        return Err(field_error(&object, "call", "must not be empty"));
    }
    if let Some(prefix) = &route.prefix {
        if !prefix.starts_with('/') {
            return Err(field_error(&object, "prefix", "must start with /"));
        }
    }
    if let Some(base) = &route.base_path {
        if !base.starts_with('/') {
            return Err(field_error(&object, "base_path", "must start with /"));
        }
    }
    Ok(())
}

fn validate_api_client(index: usize, client: &ApiClientPattern) -> Result<()> {
    let object = object_prefix("api_clients", index, &client.id);
    if client.call.is_empty() {
        return Err(field_error(&object, "call", "must not be empty"));
    }
    // Only the TypeScript extractor reads API client patterns; a Rust one would match nothing.
    if client.lang != PatternLang::Ts {
        return Err(field_error(
            &object,
            "lang",
            "must be \"ts\" for an API client pattern",
        ));
    }
    let given = [
        client.method.is_some(),
        client.method_arg.is_some(),
        client.method_option.is_some(),
    ]
    .into_iter()
    .filter(|given| *given)
    .count();
    if given > 1 {
        return Err(Error::Config {
            message: format!(
                "{object}: at most one of fields \"method\", \"method_arg\" and \"method_option\" may be given"
            ),
        });
    }
    if let Some(method) = &client.method {
        if !valid_method(method) {
            return Err(field_error(
                &object,
                "method",
                "must be uppercase ASCII letters or ANY",
            ));
        }
    }
    if let Some(method) = &client.default_method {
        if !valid_method(method) {
            return Err(field_error(
                &object,
                "default_method",
                "must be uppercase ASCII letters or ANY",
            ));
        }
    }
    if let Some(prefix) = &client.strip_prefix {
        if !prefix.starts_with('/') {
            return Err(field_error(&object, "strip_prefix", "must start with /"));
        }
    }
    if let Some(base) = &client.base_path {
        if !base.starts_with('/') {
            return Err(field_error(&object, "base_path", "must start with /"));
        }
    }
    Ok(())
}

fn validate_rule_ids(rules: &[RawRule]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (index, rule) in rules.iter().enumerate() {
        let object = object_prefix("rules", index, &rule.id);
        if rule.id.is_empty() {
            return Err(field_error(&object, "id", "must not be empty"));
        }
        if !seen.insert(rule.id.clone()) {
            return Err(field_error(&object, "id", "is a duplicate rule id"));
        }
    }
    Ok(())
}

/// `Config::hash`: a canonical JSON text of the extraction fields (object keys sorted by
/// `serde_json::Map`, absent fields as their defaults), hashed.
fn extraction_hash(raw: &RawConfig, value: Option<&serde_json::Value>) -> Result<String> {
    let patterns = |key: &str| match value.and_then(|value| value.get(key)) {
        Some(found) => without_notes(found),
        None => serde_json::Value::Array(Vec::new()),
    };
    let mut canonical = serde_json::Map::new();
    canonical.insert(
        "include".to_owned(),
        serde_json::Value::from(raw.include.clone()),
    );
    canonical.insert(
        "exclude".to_owned(),
        serde_json::Value::from(raw.exclude.clone()),
    );
    canonical.insert("routes".to_owned(), patterns("routes"));
    canonical.insert("api_clients".to_owned(), patterns("api_clients"));
    let text = serde_json::to_string(&serde_json::Value::Object(canonical))
        .map_err(|source| Error::json("hashing arlineo.json", source))?;
    Ok(crate::util::fnv::hex(text.as_bytes()))
}

/// A JSON value with every `note` member removed, at any depth. Depth is bounded by the
/// JSON parser's own nesting limit.
fn without_notes(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "note")
                .map(|(key, item)| (key.clone(), without_notes(item)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(without_notes).collect())
        }
        other => other.clone(),
    }
}

fn compile_glob_set(patterns: &[String], name: &str) -> Result<GlobSet> {
    for (index, pattern) in patterns.iter().enumerate() {
        Glob::new(pattern).map_err(|error| prefixed_error(&format!("{name}[{index}]"), error))?;
    }
    GlobSet::new(patterns).map_err(|error| prefixed_error(name, error))
}

fn rule_kind_name(kind: RuleKind) -> &'static str {
    match kind {
        RuleKind::RequiresBefore => "requires-before",
        RuleKind::OrderInFunction => "order-in-function",
        RuleKind::SourcesAgree => "sources-agree",
        RuleKind::HasTarget => "has-target",
        RuleKind::Forbidden => "forbidden",
        RuleKind::ReviewOnChange => "review-on-change",
    }
}

fn check_unused(object: &str, kind: RuleKind, field: &str, given: bool) -> Result<()> {
    if given {
        return Err(field_error(
            object,
            field,
            &format!("is not used by kind {}", rule_kind_name(kind)),
        ));
    }
    Ok(())
}

fn required_selector(
    object: &str,
    kind: RuleKind,
    field: &str,
    raw: Option<&RawSelector>,
) -> Result<Selector> {
    match raw {
        Some(raw) => Selector::compile(raw)
            .map_err(|error| prefixed_error(&format!("{object}: field {field:?}"), error)),
        None => Err(field_error(
            object,
            field,
            &format!("is required for kind {}", rule_kind_name(kind)),
        )),
    }
}

/// Options that belong to one kind only.
fn check_kind_options(object: &str, kind: RuleKind, raw: &RawRule) -> Result<()> {
    if kind != RuleKind::RequiresBefore {
        check_unused(object, kind, "all_paths", raw.all_paths.is_some())?;
    }
    if kind != RuleKind::SourcesAgree {
        check_unused(object, kind, "direction", raw.direction.is_some())?;
        check_unused(object, kind, "minus", raw.minus.is_some())?;
        check_unused(object, kind, "dropped", raw.dropped.is_some())?;
    }
    Ok(())
}

fn build_rule(index: usize, raw: &RawRule) -> Result<Rule> {
    let object = object_prefix("rules", index, &raw.id);
    let kind = raw.kind;
    check_kind_options(&object, kind, raw)?;
    let spec = match kind {
        RuleKind::RequiresBefore => {
            check_unused(&object, kind, "first", raw.first.is_some())?;
            check_unused(&object, kind, "then", raw.then.is_some())?;
            check_unused(
                &object,
                kind,
                "only_with_first",
                raw.only_with_first.is_some(),
            )?;
            check_unused(&object, kind, "left", raw.left.is_some())?;
            check_unused(&object, kind, "right", raw.right.is_some())?;
            check_unused(&object, kind, "fields", raw.fields.is_some())?;
            check_unused(&object, kind, "source", raw.source.is_some())?;
            check_unused(&object, kind, "target", raw.target.is_some())?;
            check_unused(&object, kind, "source_field", raw.source_field.is_some())?;
            check_unused(&object, kind, "target_field", raw.target_field.is_some())?;
            check_unused(&object, kind, "selector", raw.selector.is_some())?;
            let when = required_selector(&object, kind, "when", raw.when.as_ref())?;
            let require = required_selector(&object, kind, "require", raw.require.as_ref())?;
            let depth = raw.depth.unwrap_or_default();
            let include_ancestor_callees = raw.include_ancestor_callees.unwrap_or_default();
            let all_paths = raw.all_paths.unwrap_or_default();
            if all_paths && include_ancestor_callees {
                return Err(field_error(
                    &object,
                    "include_ancestor_callees",
                    "cannot be used with all_paths",
                ));
            }
            RuleSpec::RequiresBefore {
                when,
                require,
                depth,
                include_ancestor_callees,
                all_paths,
            }
        }
        RuleKind::OrderInFunction => {
            check_unused(&object, kind, "when", raw.when.is_some())?;
            check_unused(&object, kind, "require", raw.require.is_some())?;
            check_unused(&object, kind, "depth", raw.depth.is_some())?;
            check_unused(
                &object,
                kind,
                "include_ancestor_callees",
                raw.include_ancestor_callees.is_some(),
            )?;
            check_unused(&object, kind, "left", raw.left.is_some())?;
            check_unused(&object, kind, "right", raw.right.is_some())?;
            check_unused(&object, kind, "fields", raw.fields.is_some())?;
            check_unused(&object, kind, "source", raw.source.is_some())?;
            check_unused(&object, kind, "target", raw.target.is_some())?;
            check_unused(&object, kind, "source_field", raw.source_field.is_some())?;
            check_unused(&object, kind, "target_field", raw.target_field.is_some())?;
            check_unused(&object, kind, "selector", raw.selector.is_some())?;
            let first = required_selector(&object, kind, "first", raw.first.as_ref())?;
            let then = required_selector(&object, kind, "then", raw.then.as_ref())?;
            RuleSpec::OrderInFunction {
                first,
                then,
                only_with_first: raw.only_with_first.unwrap_or_default(),
            }
        }
        RuleKind::SourcesAgree => {
            check_unused(&object, kind, "when", raw.when.is_some())?;
            check_unused(&object, kind, "require", raw.require.is_some())?;
            check_unused(&object, kind, "depth", raw.depth.is_some())?;
            check_unused(
                &object,
                kind,
                "include_ancestor_callees",
                raw.include_ancestor_callees.is_some(),
            )?;
            check_unused(&object, kind, "first", raw.first.is_some())?;
            check_unused(&object, kind, "then", raw.then.is_some())?;
            check_unused(
                &object,
                kind,
                "only_with_first",
                raw.only_with_first.is_some(),
            )?;
            check_unused(&object, kind, "source", raw.source.is_some())?;
            check_unused(&object, kind, "target", raw.target.is_some())?;
            check_unused(&object, kind, "source_field", raw.source_field.is_some())?;
            check_unused(&object, kind, "target_field", raw.target_field.is_some())?;
            check_unused(&object, kind, "selector", raw.selector.is_some())?;
            let left = required_selector(&object, kind, "left", raw.left.as_ref())?;
            let right = required_selector(&object, kind, "right", raw.right.as_ref())?;
            if let Some(fields) = &raw.fields {
                if fields.is_empty() {
                    return Err(field_error(&object, "fields", "must not be empty"));
                }
            }
            let fields = match &raw.fields {
                Some(fields) => fields.clone(),
                None => vec!["key".to_owned()],
            };
            let minus = match &raw.minus {
                Some(_) => Some(required_selector(
                    &object,
                    kind,
                    "minus",
                    raw.minus.as_ref(),
                )?),
                None => None,
            };
            let dropped = match &raw.dropped {
                Some(_) => Some(required_selector(
                    &object,
                    kind,
                    "dropped",
                    raw.dropped.as_ref(),
                )?),
                None => None,
            };
            RuleSpec::SourcesAgree {
                left,
                right,
                fields,
                direction: raw.direction.unwrap_or_default(),
                minus,
                dropped,
            }
        }
        RuleKind::HasTarget => {
            check_unused(&object, kind, "when", raw.when.is_some())?;
            check_unused(&object, kind, "require", raw.require.is_some())?;
            check_unused(&object, kind, "depth", raw.depth.is_some())?;
            check_unused(
                &object,
                kind,
                "include_ancestor_callees",
                raw.include_ancestor_callees.is_some(),
            )?;
            check_unused(&object, kind, "first", raw.first.is_some())?;
            check_unused(&object, kind, "then", raw.then.is_some())?;
            check_unused(
                &object,
                kind,
                "only_with_first",
                raw.only_with_first.is_some(),
            )?;
            check_unused(&object, kind, "left", raw.left.is_some())?;
            check_unused(&object, kind, "right", raw.right.is_some())?;
            check_unused(&object, kind, "fields", raw.fields.is_some())?;
            check_unused(&object, kind, "selector", raw.selector.is_some())?;
            let source = required_selector(&object, kind, "source", raw.source.as_ref())?;
            let target = required_selector(&object, kind, "target", raw.target.as_ref())?;
            let source_field = match &raw.source_field {
                Some(field) => field.clone(),
                None => "key".to_owned(),
            };
            let target_field = match &raw.target_field {
                Some(field) => field.clone(),
                None => "key".to_owned(),
            };
            RuleSpec::HasTarget {
                source,
                target,
                source_field,
                target_field,
            }
        }
        RuleKind::Forbidden => {
            check_unused(&object, kind, "when", raw.when.is_some())?;
            check_unused(&object, kind, "require", raw.require.is_some())?;
            check_unused(&object, kind, "depth", raw.depth.is_some())?;
            check_unused(
                &object,
                kind,
                "include_ancestor_callees",
                raw.include_ancestor_callees.is_some(),
            )?;
            check_unused(&object, kind, "first", raw.first.is_some())?;
            check_unused(&object, kind, "then", raw.then.is_some())?;
            check_unused(
                &object,
                kind,
                "only_with_first",
                raw.only_with_first.is_some(),
            )?;
            check_unused(&object, kind, "left", raw.left.is_some())?;
            check_unused(&object, kind, "right", raw.right.is_some())?;
            check_unused(&object, kind, "fields", raw.fields.is_some())?;
            check_unused(&object, kind, "source", raw.source.is_some())?;
            check_unused(&object, kind, "target", raw.target.is_some())?;
            check_unused(&object, kind, "source_field", raw.source_field.is_some())?;
            check_unused(&object, kind, "target_field", raw.target_field.is_some())?;
            let selector = required_selector(&object, kind, "selector", raw.selector.as_ref())?;
            RuleSpec::Forbidden { selector }
        }
        RuleKind::ReviewOnChange => {
            check_unused(&object, kind, "when", raw.when.is_some())?;
            check_unused(&object, kind, "require", raw.require.is_some())?;
            check_unused(&object, kind, "depth", raw.depth.is_some())?;
            check_unused(
                &object,
                kind,
                "include_ancestor_callees",
                raw.include_ancestor_callees.is_some(),
            )?;
            check_unused(&object, kind, "first", raw.first.is_some())?;
            check_unused(&object, kind, "then", raw.then.is_some())?;
            check_unused(
                &object,
                kind,
                "only_with_first",
                raw.only_with_first.is_some(),
            )?;
            check_unused(&object, kind, "left", raw.left.is_some())?;
            check_unused(&object, kind, "right", raw.right.is_some())?;
            check_unused(&object, kind, "fields", raw.fields.is_some())?;
            check_unused(&object, kind, "source", raw.source.is_some())?;
            check_unused(&object, kind, "target", raw.target.is_some())?;
            check_unused(&object, kind, "source_field", raw.source_field.is_some())?;
            check_unused(&object, kind, "target_field", raw.target_field.is_some())?;
            let selector = required_selector(&object, kind, "selector", raw.selector.as_ref())?;
            RuleSpec::ReviewOnChange { selector }
        }
    };
    let mut ignore = BTreeSet::new();
    for entry in raw.ignore.iter().flatten() {
        if entry.key.trim().is_empty() {
            return Err(field_error(&object, "ignore", "key must not be empty"));
        }
        if entry.reason.trim().is_empty() {
            return Err(field_error(
                &object,
                "ignore",
                &format!("reason must not be empty for key '{}'", entry.key),
            ));
        }
        if !ignore.insert(entry.key.clone()) {
            return Err(field_error(
                &object,
                "ignore",
                &format!("key '{}' is listed twice", entry.key),
            ));
        }
    }
    Ok(Rule {
        id: raw.id.clone(),
        note: raw.note.clone(),
        spec,
        ignore,
    })
}
