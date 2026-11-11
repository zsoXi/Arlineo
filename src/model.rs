//! Shared data model: paths, commit ids, facts, fact data and the call graph.
//!
//! Every module builds on these types. Field names and serialized names are part of the
//! facts file format (`SPEC.md`, "Facts"); change them only together with
//! [`FACTS_FORMAT_VERSION`].

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::util::fnv;

/// Version of the facts file format (`facts.jsonl`).
pub const FACTS_FORMAT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Paths and ids

/// A repository-relative path with `/` separators.
///
/// Invariant: not empty, not absolute, no `\`, no NUL, and no empty, `.` or `..` segments.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Validates and wraps a repository-relative path.
    pub fn new(path: &str) -> Result<RelPath> {
        let bad = path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path.contains('\0')
            || path
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..");
        if bad {
            return Err(Error::InvalidPath {
                path: path.to_owned(),
            });
        }
        Ok(RelPath(path.to_owned()))
    }

    /// The path as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Last segment of the path.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Extension of the last segment without the dot; `None` for names like `.env`.
    pub fn extension(&self) -> Option<&str> {
        let (stem, ext) = self.file_name().rsplit_once('.')?;
        if stem.is_empty() { None } else { Some(ext) }
    }

    /// Everything before the last `/`, or `None` for a top-level file.
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }
}

impl TryFrom<String> for RelPath {
    type Error = Error;

    fn try_from(value: String) -> Result<RelPath> {
        RelPath::new(&value)
    }
}

impl From<RelPath> for String {
    fn from(path: RelPath) -> String {
        path.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A full commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hexadecimal digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitId(String);

impl CommitId {
    /// Validates and wraps a full commit id.
    pub fn new(value: &str) -> Result<CommitId> {
        let hex = value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if (value.len() == 40 || value.len() == 64) && hex {
            Ok(CommitId(value.to_owned()))
        } else {
            Err(Error::InvalidCommitId {
                value: value.to_owned(),
            })
        }
    }

    /// The full id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 9 digits, for display.
    pub fn short(&self) -> &str {
        self.0.get(..9).unwrap_or(&self.0)
    }
}

impl TryFrom<String> for CommitId {
    type Error = Error;

    fn try_from(value: String) -> Result<CommitId> {
        CommitId::new(&value)
    }
}

impl From<CommitId> for String {
    fn from(id: CommitId) -> String {
        id.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable id of a worktree: FNV-1a 64 of its absolute path, as 16 hex digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorktreeId(String);

impl WorktreeId {
    /// Id for the worktree at `path` (pass the canonical absolute path).
    pub fn from_path(path: &Path) -> WorktreeId {
        WorktreeId(fnv::hex(path.as_os_str().as_encoded_bytes()))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------------------------
// Languages, kinds, confidence

/// Source languages Arlineo reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Sql,
    TypeScript,
    JavaScript,
}

impl Lang {
    /// Language by file extension, or `None` for files Arlineo does not read.
    pub fn from_path(path: &RelPath) -> Option<Lang> {
        match path.extension()? {
            "rs" => Some(Lang::Rust),
            "sql" => Some(Lang::Sql),
            "ts" | "tsx" | "mts" | "cts" => Some(Lang::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
            _ => None,
        }
    }

    /// Whether the file may contain JSX (`.tsx`, `.jsx`).
    pub fn is_jsx(path: &RelPath) -> bool {
        matches!(path.extension(), Some("tsx" | "jsx"))
    }
}

/// Kind of a fact. The serialized name is the snake_case of the variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    File,
    Unparsed,
    Module,
    FnDef,
    TypeDef,
    Impl,
    Import,
    Call,
    Route,
    ApiCall,
    EnvVar,
    SqlInCode,
    Table,
    Column,
    Grant,
    Trigger,
    SqlFunction,
    TableAccess,
    ConstDef,
}

impl FactKind {
    /// Every kind with its serialized name, in declaration order.
    pub const ALL: [(FactKind, &'static str); 19] = [
        (FactKind::File, "file"),
        (FactKind::Unparsed, "unparsed"),
        (FactKind::Module, "module"),
        (FactKind::FnDef, "fn_def"),
        (FactKind::TypeDef, "type_def"),
        (FactKind::Impl, "impl"),
        (FactKind::Import, "import"),
        (FactKind::Call, "call"),
        (FactKind::Route, "route"),
        (FactKind::ApiCall, "api_call"),
        (FactKind::EnvVar, "env_var"),
        (FactKind::SqlInCode, "sql_in_code"),
        (FactKind::Table, "table"),
        (FactKind::Column, "column"),
        (FactKind::Grant, "grant"),
        (FactKind::Trigger, "trigger"),
        (FactKind::SqlFunction, "sql_function"),
        (FactKind::TableAccess, "table_access"),
        (FactKind::ConstDef, "const_def"),
    ];

    /// Serialized name.
    pub fn as_str(self) -> &'static str {
        FactKind::ALL
            .iter()
            .find(|(kind, _)| *kind == self)
            .map(|(_, name)| *name)
            .unwrap_or("unknown")
    }

    /// Kind by serialized name.
    pub fn parse(name: &str) -> Option<FactKind> {
        FactKind::ALL
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(kind, _)| *kind)
    }
}

/// How sure Arlineo is about a fact or an edge. Ordered from strongest to weakest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Read directly from syntax, or resolved through an explicit path, `use`, `self` or import.
    Exact,
    /// Resolved because exactly one definition has this name.
    UniqueName,
    /// Recognized by a configured or built-in pattern (routes, API calls, SQL in strings).
    Heuristic,
    /// One of several possible targets.
    Guessed,
}

impl Confidence {
    /// The weaker of two levels (used for paths through the graph).
    pub fn weakest(self, other: Confidence) -> Confidence {
        self.max(other)
    }

    /// Serialized name.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::UniqueName => "unique_name",
            Confidence::Heuristic => "heuristic",
            Confidence::Guessed => "guessed",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Facts

/// One fact about the code. Serialized as one line of `facts.jsonl`.
///
/// `data` holds the kind-specific fields; build facts with [`Fact::new`] and read the fields
/// with [`Fact::data_as`] so that kind and data always agree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub kind: FactKind,
    pub key: String,
    pub file: RelPath,
    /// 1-based line of the fact; 0 means the whole file.
    pub line: u32,
    #[serde(default)]
    pub data: serde_json::Map<String, serde_json::Value>,
    pub confidence: Confidence,
}

/// Kind-specific data of a fact.
pub trait FactData: Serialize + DeserializeOwned {
    /// The fact kind this data belongs to.
    const KIND: FactKind;
}

impl Fact {
    /// Builds a fact whose kind matches its data.
    pub fn new<D: FactData>(
        file: &RelPath,
        line: u32,
        key: impl Into<String>,
        confidence: Confidence,
        data: &D,
    ) -> Result<Fact> {
        let value = serde_json::to_value(data)
            .map_err(|source| Error::json(format!("{} fact data", D::KIND.as_str()), source))?;
        let serde_json::Value::Object(data) = value else {
            return Err(Error::Internal {
                message: format!("{} fact data is not a JSON object", D::KIND.as_str()),
            });
        };
        Ok(Fact {
            kind: D::KIND,
            key: key.into(),
            file: file.clone(),
            line,
            data,
            confidence,
        })
    }

    /// Reads the kind-specific data. Fails when the fact has another kind.
    pub fn data_as<D: FactData>(&self) -> Result<D> {
        if self.kind != D::KIND {
            return Err(Error::Internal {
                message: format!(
                    "{} fact {} read as {}",
                    self.kind.as_str(),
                    self.key,
                    D::KIND.as_str()
                ),
            });
        }
        serde_json::from_value(serde_json::Value::Object(self.data.clone())).map_err(|source| {
            Error::json(
                format!(
                    "data of {} fact {} in {}",
                    self.kind.as_str(),
                    self.key,
                    self.file
                ),
                source,
            )
        })
    }

    /// A data field as text, for selectors and `find --where`: strings as they are, numbers and
    /// booleans in JSON form, arrays as their elements joined with `,`, `None` for null or a
    /// missing field.
    pub fn data_text(&self, field: &str) -> Option<String> {
        value_text(self.data.get(field)?)
    }
}

fn value_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(value_text)
                .collect::<Vec<_>>()
                .join(","),
        ),
        other => Some(other.to_string()),
    }
}

/// Total order of facts: file, line, kind, key, data, confidence. Facts files are written in
/// this order so that the same input always gives the same bytes.
pub fn compare_facts(a: &Fact, b: &Fact) -> Ordering {
    a.file
        .cmp(&b.file)
        .then(a.line.cmp(&b.line))
        .then(a.kind.cmp(&b.kind))
        .then_with(|| a.key.cmp(&b.key))
        .then_with(|| {
            let left = a.data.iter().map(|(k, v)| (k, v.to_string()));
            let right = b.data.iter().map(|(k, v)| (k, v.to_string()));
            left.cmp(right)
        })
        .then(a.confidence.cmp(&b.confidence))
}

/// Sorts facts in [`compare_facts`] order and removes exact duplicates.
pub fn sort_facts(facts: &mut Vec<Fact>) {
    facts.sort_by(compare_facts);
    facts.dedup();
}

// ---------------------------------------------------------------------------------------------
// Fact data, one struct per kind. Field meanings are in SPEC.md, "Facts".

/// Kind of a type definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeKind {
    Struct,
    Enum,
    Union,
    Trait,
    TypeAlias,
    Class,
    Interface,
}

/// SQL statement effect on a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlOp {
    Select,
    SelectForUpdate,
    Insert,
    Update,
    Delete,
    Truncate,
}

/// Kind of a DDL change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DdlAction {
    Create,
    Alter,
    Drop,
    Rename,
}

/// Kind of a SQL relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlObject {
    Table,
    View,
    MaterializedView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileData {
    pub lang: Lang,
    pub lines: u32,
    /// FNV-1a 64 of the file bytes, 16 hex digits.
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnparsedData {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleData {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FnDefData {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trait_name: Option<String>,
    pub is_pub: bool,
    pub is_async: bool,
    pub end_line: u32,
    /// FNV-1a 64 of the item's token texts joined with one space, 16 hex digits.
    pub hash: String,
}

/// A Rust `const` or `static` item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConstDefData {
    pub name: String,
    pub is_static: bool,
    pub end_line: u32,
    /// FNV-1a 64 of the item's token texts joined with one space, 16 hex digits.
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeDefData {
    pub name: String,
    pub type_kind: TypeKind,
    pub end_line: u32,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplData {
    pub self_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trait_name: Option<String>,
    pub end_line: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportData {
    /// Key of the importing module.
    pub owner: String,
    /// Path as written: `crate::orders` in Rust, `./orders` in TypeScript.
    pub path: String,
    /// Imported name, or `*` for a glob import.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallData {
    /// Key of the enclosing function, or of the module for top-level code.
    pub owner: String,
    /// Callee as written, path segments joined with `::` (Rust) or `.` (TypeScript).
    pub callee: String,
    /// Last segment of the callee.
    pub name: String,
    /// Path segments before `name` (empty for a bare call or a method call).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qualifier: Vec<String>,
    /// Receiver of a method call when it is `self`, `this` or a single identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    pub is_method: bool,
    pub is_macro: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteData {
    /// Uppercase HTTP method, or `ANY`.
    pub method: String,
    /// Path as written.
    pub path: String,
    /// Path after [`norm_path`].
    pub norm_path: String,
    /// Handler expression as written (`orders::create`, `create_order`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
    /// Id of the pattern that recognized the route.
    pub pattern: String,
    /// Key of the function or module that registers the route.
    pub owner: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiCallData {
    pub method: String,
    pub path: String,
    pub norm_path: String,
    /// Called function or method name (`fetch`, `get`).
    pub via: String,
    pub pattern: String,
    pub owner: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVarData {
    pub owner: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlInCodeData {
    pub owner: String,
    /// First 120 characters of the SQL with whitespace collapsed and `'...'` literals
    /// replaced by `'?'`.
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableData {
    pub object: SqlObject,
    pub action: DdlAction,
    pub end_line: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnData {
    pub table: String,
    pub column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_type: Option<String>,
    pub action: DdlAction,
    /// Default name (`<table>_<column>_seq`) of the sequence an identity or `serial` column
    /// creates; PostgreSQL drops it with the table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantData {
    pub revoke: bool,
    /// One uppercase privilege (`SELECT`, `INSERT`, `ALL`, ...).
    pub privilege: String,
    /// `TABLE`, `SEQUENCE`, `FUNCTION`, `SCHEMA` or `ALL <kind> IN SCHEMA`, where kind is
    /// `TABLES`, `SEQUENCES`, `FUNCTIONS`, `PROCEDURES` or `ROUTINES`.
    pub object_kind: String,
    pub object: String,
    pub role: String,
    /// One column of a column grant: `GRANT SELECT (a, b)` gives one fact per column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// Key of the enclosing SQL function or `DO` block, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl GrantData {
    /// Fact key: `[REVOKE ]<privilege>[(<column>)] <object> <role>`.
    pub fn key(&self) -> String {
        let prefix = if self.revoke { "REVOKE " } else { "" };
        match &self.column {
            Some(column) => format!(
                "{prefix}{}({column}) {} {}",
                self.privilege, self.object, self.role
            ),
            None => format!("{prefix}{} {} {}", self.privilege, self.object, self.role),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerData {
    pub name: String,
    pub table: String,
    pub function: String,
    /// `BEFORE`, `AFTER` or `INSTEAD OF`.
    pub timing: String,
    /// Uppercase events: `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`.
    pub events: Vec<String>,
    pub action: DdlAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlFunctionData {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub action: DdlAction,
    pub end_line: u32,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableAccessData {
    /// Key of the function, SQL function, `DO` block or file that runs the statement.
    pub owner: String,
    pub table: String,
    pub op: SqlOp,
}

impl FactData for FileData {
    const KIND: FactKind = FactKind::File;
}
impl FactData for UnparsedData {
    const KIND: FactKind = FactKind::Unparsed;
}
impl FactData for ModuleData {
    const KIND: FactKind = FactKind::Module;
}
impl FactData for FnDefData {
    const KIND: FactKind = FactKind::FnDef;
}
impl FactData for TypeDefData {
    const KIND: FactKind = FactKind::TypeDef;
}
impl FactData for ImplData {
    const KIND: FactKind = FactKind::Impl;
}
impl FactData for ImportData {
    const KIND: FactKind = FactKind::Import;
}
impl FactData for CallData {
    const KIND: FactKind = FactKind::Call;
}
impl FactData for RouteData {
    const KIND: FactKind = FactKind::Route;
}
impl FactData for ApiCallData {
    const KIND: FactKind = FactKind::ApiCall;
}
impl FactData for EnvVarData {
    const KIND: FactKind = FactKind::EnvVar;
}
impl FactData for SqlInCodeData {
    const KIND: FactKind = FactKind::SqlInCode;
}
impl FactData for TableData {
    const KIND: FactKind = FactKind::Table;
}
impl FactData for ColumnData {
    const KIND: FactKind = FactKind::Column;
}
impl FactData for GrantData {
    const KIND: FactKind = FactKind::Grant;
}
impl FactData for TriggerData {
    const KIND: FactKind = FactKind::Trigger;
}
impl FactData for SqlFunctionData {
    const KIND: FactKind = FactKind::SqlFunction;
}
impl FactData for TableAccessData {
    const KIND: FactKind = FactKind::TableAccess;
}
impl FactData for ConstDefData {
    const KIND: FactKind = FactKind::ConstDef;
}

// ---------------------------------------------------------------------------------------------
// HTTP paths

/// Normalizes an HTTP path so that routes and API calls can be matched (`SPEC.md`, "Paths").
///
/// Drops scheme and host, query and fragment; replaces parameter segments (`:id`, `{id}`,
/// `{*rest}`, `*`, and any segment containing `${`) with `{}`; removes empty segments.
/// The result always starts with `/`.
pub fn norm_path(raw: &str) -> String {
    let mut path = raw.trim();
    // The query and fragment go first, so a URL inside them (`/r?to=https://a/b`) is ignored.
    if let Some(cut) = path.find(['?', '#']) {
        path = &path[..cut];
    }
    // `scheme://host` is only a prefix: a `://` after a `/` belongs to the path.
    if let Some(scheme_end) = path.find("://").filter(|end| !path[..*end].contains('/')) {
        let rest = &path[scheme_end + 3..];
        path = match rest.find('/') {
            Some(slash) => &rest[slash..],
            None => "/",
        };
    }
    let mut out = String::new();
    for seg in path.split('/').filter(|seg| !seg.is_empty()) {
        out.push('/');
        let param = seg.starts_with(':')
            || (seg.starts_with('{') && seg.ends_with('}'))
            || seg.starts_with('*')
            || seg.contains("${");
        out.push_str(if param { "{}" } else { seg });
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// [`norm_path`] of `path` read relative to `base`: the normalized `base` is put in front
/// unless the normalized path already starts with it as whole segments.
pub fn norm_path_in(base: &str, path: &str) -> String {
    let base = norm_path(base);
    let path = norm_path(path);
    if base == "/" || path == base || path.starts_with(&format!("{base}/")) {
        path
    } else if path == "/" {
        base
    } else {
        format!("{base}{path}")
    }
}

// ---------------------------------------------------------------------------------------------
// Graph

/// Index of a node in a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// Id for position `index`, or `None` when it does not fit in `u32`.
    pub fn from_index(index: usize) -> Option<NodeId> {
        u32::try_from(index).ok().map(NodeId)
    }

    /// Position in [`Graph::nodes`].
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Raw value, for the graph file.
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Kind of a graph node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Module,
    Fn,
    Type,
    SqlFunction,
    Table,
    Route,
}

impl NodeKind {
    /// Every kind in declaration order.
    pub const ALL: [NodeKind; 6] = [
        NodeKind::Module,
        NodeKind::Fn,
        NodeKind::Type,
        NodeKind::SqlFunction,
        NodeKind::Table,
        NodeKind::Route,
    ];

    /// Byte code for the graph file.
    pub fn code(self) -> u8 {
        match self {
            NodeKind::Module => 0,
            NodeKind::Fn => 1,
            NodeKind::Type => 2,
            NodeKind::SqlFunction => 3,
            NodeKind::Table => 4,
            NodeKind::Route => 5,
        }
    }

    /// Kind for a byte code from the graph file.
    pub fn from_code(code: u8) -> Option<NodeKind> {
        NodeKind::ALL.into_iter().find(|kind| kind.code() == code)
    }

    /// Serialized name.
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Module => "module",
            NodeKind::Fn => "fn",
            NodeKind::Type => "type",
            NodeKind::SqlFunction => "sql_function",
            NodeKind::Table => "table",
            NodeKind::Route => "route",
        }
    }
}

/// Kind of a graph edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Function or SQL function calls function or SQL function.
    Calls,
    /// Module contains function or type; type contains method.
    Contains,
    /// Route is handled by function.
    Handles,
    /// Function sends an API call that matches route.
    Requests,
    /// Function or SQL function reads table.
    Reads,
    /// Function or SQL function writes table.
    Writes,
    /// Table fires trigger function.
    Triggers,
    /// Module imports module.
    Imports,
}

impl EdgeKind {
    /// Every kind in declaration order.
    pub const ALL: [EdgeKind; 8] = [
        EdgeKind::Calls,
        EdgeKind::Contains,
        EdgeKind::Handles,
        EdgeKind::Requests,
        EdgeKind::Reads,
        EdgeKind::Writes,
        EdgeKind::Triggers,
        EdgeKind::Imports,
    ];

    /// Byte code for the graph file.
    pub fn code(self) -> u8 {
        match self {
            EdgeKind::Calls => 0,
            EdgeKind::Contains => 1,
            EdgeKind::Handles => 2,
            EdgeKind::Requests => 3,
            EdgeKind::Reads => 4,
            EdgeKind::Writes => 5,
            EdgeKind::Triggers => 6,
            EdgeKind::Imports => 7,
        }
    }

    /// Kind for a byte code from the graph file.
    pub fn from_code(code: u8) -> Option<EdgeKind> {
        EdgeKind::ALL.into_iter().find(|kind| kind.code() == code)
    }

    /// Serialized name.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Contains => "contains",
            EdgeKind::Handles => "handles",
            EdgeKind::Requests => "requests",
            EdgeKind::Reads => "reads",
            EdgeKind::Writes => "writes",
            EdgeKind::Triggers => "triggers",
            EdgeKind::Imports => "imports",
        }
    }
}

/// A definition in the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub key: String,
    pub kind: NodeKind,
    pub file: Option<RelPath>,
    pub line: u32,
}

/// A relation between two nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub confidence: Confidence,
    /// Line of the call site or registration in the `from` node's file; 0 if none.
    pub line: u32,
}

/// The call graph with adjacency indexes.
///
/// Invariants: every edge refers to existing nodes; `(kind, key)` is unique among nodes.
#[derive(Clone, Debug)]
pub struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    by_key: HashMap<(NodeKind, String), NodeId>,
    outgoing: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
}

impl Graph {
    /// Builds a graph and its indexes; fails when an invariant does not hold.
    pub fn from_parts(nodes: Vec<Node>, edges: Vec<Edge>) -> Result<Graph> {
        if NodeId::from_index(nodes.len()).is_none() {
            return Err(Error::Internal {
                message: format!("too many graph nodes: {}", nodes.len()),
            });
        }
        let mut by_key = HashMap::with_capacity(nodes.len());
        for (index, node) in nodes.iter().enumerate() {
            let id = NodeId::from_index(index).ok_or_else(|| Error::Internal {
                message: format!("node index {index} does not fit in u32"),
            })?;
            if by_key.insert((node.kind, node.key.clone()), id).is_some() {
                return Err(Error::Internal {
                    message: format!("duplicate {} node {}", node.kind.as_str(), node.key),
                });
            }
        }
        let mut outgoing = vec![Vec::new(); nodes.len()];
        let mut incoming = vec![Vec::new(); nodes.len()];
        for (index, edge) in edges.iter().enumerate() {
            let (Some(out), true) = (
                outgoing.get_mut(edge.from.index()),
                edge.to.index() < nodes.len(),
            ) else {
                return Err(Error::Internal {
                    message: format!(
                        "edge {index} refers to a missing node ({} -> {})",
                        edge.from.raw(),
                        edge.to.raw()
                    ),
                });
            };
            out.push(index);
            if let Some(inc) = incoming.get_mut(edge.to.index()) {
                inc.push(index);
            }
        }
        Ok(Graph {
            nodes,
            edges,
            by_key,
            outgoing,
            incoming,
        })
    }

    /// All nodes; `NodeId::index` is the position.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// All edges.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Node by id.
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }

    /// Node by kind and key.
    pub fn lookup(&self, kind: NodeKind, key: &str) -> Option<NodeId> {
        self.by_key.get(&(kind, key.to_owned())).copied()
    }

    /// Nodes of any kind with this key, in [`NodeKind::ALL`] order.
    pub fn lookup_any(&self, key: &str) -> Vec<NodeId> {
        NodeKind::ALL
            .into_iter()
            .filter_map(|kind| self.lookup(kind, key))
            .collect()
    }

    /// Edges leaving `id`.
    pub fn outgoing(&self, id: NodeId) -> impl Iterator<Item = &Edge> {
        self.outgoing
            .get(id.index())
            .into_iter()
            .flatten()
            .filter_map(|&edge| self.edges.get(edge))
    }

    /// Edges arriving at `id`.
    pub fn incoming(&self, id: NodeId) -> impl Iterator<Item = &Edge> {
        self.incoming
            .get(id.index())
            .into_iter()
            .flatten()
            .filter_map(|&edge| self.edges.get(edge))
    }
}

/// Facts grouped by kind, for queries. Built once per view.
#[derive(Clone, Debug, Default)]
pub struct FactIndex {
    by_kind: BTreeMap<FactKind, Vec<usize>>,
}

impl FactIndex {
    /// Indexes `facts` by kind.
    pub fn new(facts: &[Fact]) -> FactIndex {
        let mut by_kind: BTreeMap<FactKind, Vec<usize>> = BTreeMap::new();
        for (index, fact) in facts.iter().enumerate() {
            by_kind.entry(fact.kind).or_default().push(index);
        }
        FactIndex { by_kind }
    }

    /// Facts of one kind, in the order of `facts`.
    pub fn of_kind<'a>(
        &'a self,
        facts: &'a [Fact],
        kind: FactKind,
    ) -> impl Iterator<Item = &'a Fact> {
        self.by_kind
            .get(&kind)
            .into_iter()
            .flatten()
            .filter_map(move |&index| facts.get(index))
    }
}

#[cfg(test)]
mod tests {
    use super::norm_path;

    #[test]
    fn norm_path_cases() {
        assert_eq!(norm_path("/orders/:id"), "/orders/{}");
        assert_eq!(norm_path("/orders/{id}/items/"), "/orders/{}/items");
        assert_eq!(norm_path("/files/{*rest}"), "/files/{}");
        assert_eq!(norm_path("/v1/orders/${id}?full=1#top"), "/v1/orders/{}");
        assert_eq!(norm_path("https://api.example.com/orders"), "/orders");
        assert_eq!(norm_path("https://api.example.com"), "/");
        assert_eq!(norm_path("/r?to=https://a/b"), "/r");
        assert_eq!(norm_path("/proxy/http://x"), "/proxy/http:/x");
        assert_eq!(norm_path(""), "/");
    }
}
