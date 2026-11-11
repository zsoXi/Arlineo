//! PostgreSQL extractor (`SPEC.md`, "Extraction", "SQL").

use super::ExtractCtx;
use crate::error::{Error, Result};
use crate::lex::{Lexed, TokKind, Token};
use crate::model::{
    ColumnData, Confidence, DdlAction, Fact, FactData, GrantData, SqlFunctionData, SqlObject,
    SqlOp, TableAccessData, TableData, TriggerData, UnparsedData,
};
use crate::source::SourceFile;
use crate::util::fnv;

/// Facts of a `.sql` file (everything except the `file` fact and lexer errors).
pub fn extract_sql(file: &SourceFile, lexed: &Lexed, ctx: &ExtractCtx<'_>) -> Result<Vec<Fact>> {
    // The context is reserved for future configured patterns.
    let _ = ctx;
    let mut facts = Vec::new();
    for statement in statements(&file.text, &lexed.tokens) {
        let Some(first) = statement.first() else {
            continue;
        };
        let view = View {
            src: &file.text,
            tokens: statement,
        };
        match process_statement(file, view) {
            Ok(found) => facts.extend(found),
            Err(StmtError::Bad(reason)) => {
                facts.push(Fact::new(
                    &file.path,
                    first.line,
                    file.path.as_str(),
                    Confidence::Exact,
                    &UnparsedData { reason },
                )?);
            }
            Err(StmtError::Fatal(error)) => return Err(error),
        }
    }
    Ok(facts)
}

/// Whether a string literal value looks like SQL: after trimming, it starts (case-insensitive)
/// with `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `WITH`, `MERGE` or `TRUNCATE` followed by
/// whitespace.
pub fn looks_like_sql(text: &str) -> bool {
    let trimmed = text.trim();
    for keyword in [
        "SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "MERGE", "TRUNCATE",
    ] {
        let Some(head) = trimmed.get(..keyword.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(keyword) {
            continue;
        }
        let Some(rest) = trimmed.get(keyword.len()..) else {
            continue;
        };
        if rest.chars().next().is_some_and(|c| c.is_whitespace()) {
            return true;
        }
    }
    false
}

/// Preview of SQL text found in code for `sql_in_code` facts: every `'...'` literal replaced
/// by `'?'` so values written into the query (passwords, keys) are not stored, whitespace
/// collapsed, at most 120 characters.
pub fn sql_preview(sql: &str) -> String {
    let mut masked = String::with_capacity(sql.len());
    let mut in_literal = false;
    for c in sql.chars() {
        if c == '\'' {
            if !in_literal {
                masked.push_str("'?'");
            }
            in_literal = !in_literal;
        } else if !in_literal {
            masked.push(c);
        }
    }
    let collapsed = masked.split_whitespace().collect::<Vec<&str>>().join(" ");
    collapsed.chars().take(120).collect()
}

/// Tables a SQL text reads or writes, in order of appearance, each `(table key, op)` once.
/// Used by the Rust and TypeScript extractors for SQL in string literals and by this
/// extractor for function and `DO` bodies.
pub fn sql_text_accesses(sql: &str) -> Vec<(String, SqlOp)> {
    let lexed = crate::lex::sql::lex(sql);
    let mut out: Vec<(String, SqlOp)> = Vec::new();
    for statement in statements(sql, &lexed.tokens) {
        let view = View {
            src: sql,
            tokens: statement,
        };
        let (found, _) = accesses_in_statement(view);
        for access in found {
            if !out.contains(&access) {
                out.push(access);
            }
        }
    }
    out
}

/// Table key for a SQL name as written (`SPEC.md`, "SQL keys").
pub fn table_key(name: &str) -> String {
    let trimmed = name.trim();
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            current.push(c);
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else {
                    in_quotes = false;
                }
            }
        } else if c == '"' {
            in_quotes = true;
            current.push(c);
        } else if c == '.' {
            parts.push(current);
            current = String::new();
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    let mut names: Vec<String> = parts
        .iter()
        .map(|part| crate::lex::sql::ident_name(part.trim()))
        .collect();
    if names.len() > 1 && names.first().is_some_and(|first| first == "public") {
        names.remove(0);
    }
    names.join(".")
}

// ---------------------------------------------------------------------------------------------
// Token view, statement splitting and errors

#[derive(Clone, Copy)]
struct View<'a> {
    src: &'a str,
    tokens: &'a [Token],
}

impl<'a> View<'a> {
    fn len(&self) -> usize {
        self.tokens.len()
    }

    fn token(&self, index: usize) -> Option<&'a Token> {
        self.tokens.get(index)
    }

    fn text(&self, index: usize) -> &'a str {
        self.token(index).map(|t| t.text(self.src)).unwrap_or("")
    }

    fn line(&self, index: usize) -> u32 {
        self.token(index).map(|t| t.line).unwrap_or(0)
    }

    fn last_line(&self) -> u32 {
        self.tokens.last().map(|t| t.line).unwrap_or(0)
    }

    fn is_kw(&self, index: usize, keyword: &str) -> bool {
        self.token(index).is_some_and(|t| {
            t.kind == TokKind::Ident && t.text(self.src).eq_ignore_ascii_case(keyword)
        })
    }

    fn upper(&self, index: usize) -> String {
        self.text(index).to_ascii_uppercase()
    }

    fn is_punct(&self, index: usize, punct: &str) -> bool {
        self.token(index)
            .is_some_and(|t| t.kind == TokKind::Punct && t.text(self.src) == punct)
    }

    fn is_name(&self, index: usize) -> bool {
        self.token(index)
            .is_some_and(|t| t.kind == TokKind::Ident || t.kind == TokKind::QuotedIdent)
    }
}

enum StmtError {
    Bad(String),
    Fatal(Error),
}

type StmtResult<T> = std::result::Result<T, StmtError>;

fn bad<T>(reason: &str) -> StmtResult<T> {
    Err(StmtError::Bad(reason.to_owned()))
}

fn make_fact<D: FactData>(file: &SourceFile, line: u32, key: String, data: &D) -> StmtResult<Fact> {
    Fact::new(&file.path, line, key, Confidence::Exact, data).map_err(StmtError::Fatal)
}

/// Statements of a token list, split at `;` tokens; the semicolons are not included.
fn statements<'a>(src: &str, tokens: &'a [Token]) -> Vec<&'a [Token]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokKind::Punct && token.text(src) == ";" {
            if let Some(slice) = tokens.get(start..index) {
                out.push(slice);
            }
            start = index.saturating_add(1);
        }
    }
    if let Some(slice) = tokens.get(start..) {
        out.push(slice);
    }
    out
}

/// Index of the `)` matching the `(` at `open`, or `None` when unbalanced.
fn find_matching(view: View<'_>, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < view.len() {
        if view.is_punct(index, "(") {
            depth = depth.saturating_add(1);
        } else if view.is_punct(index, ")") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
        index = index.saturating_add(1);
    }
    None
}

/// Index just after the group starting at `open` (end of input when unbalanced).
fn skip_group(view: View<'_>, open: usize) -> usize {
    match find_matching(view, open) {
        Some(close) => close.saturating_add(1),
        None => view.len(),
    }
}

/// Comma separated ranges inside `start..end`, ignoring commas in nested groups.
fn split_comma_ranges(view: View<'_>, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut range_start = start;
    let mut index = start;
    while index < end && index < view.len() {
        if view.is_punct(index, "(") || view.is_punct(index, "[") {
            depth = depth.saturating_add(1);
        } else if view.is_punct(index, ")") || view.is_punct(index, "]") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && view.is_punct(index, ",") {
            ranges.push((range_start, index));
            range_start = index.saturating_add(1);
        }
        index = index.saturating_add(1);
    }
    ranges.push((range_start, end.min(view.len())));
    ranges
}

fn is_clause_keyword(upper: &str) -> bool {
    matches!(
        upper,
        "WHERE"
            | "GROUP"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "FETCH"
            | "FOR"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "NATURAL"
            | "ON"
            | "USING"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "HAVING"
            | "WINDOW"
            | "RETURNING"
            | "INTO"
            | "VALUES"
            | "SET"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "AND"
            | "OR"
            | "NOT"
            | "AS"
            | "ASC"
            | "DESC"
            | "CASE"
            | "SELECT"
            | "FROM"
            | "WITH"
    )
}

fn is_column_constraint(upper: &str) -> bool {
    matches!(
        upper,
        "CONSTRAINT"
            | "PRIMARY"
            | "NOT"
            | "NULL"
            | "UNIQUE"
            | "CHECK"
            | "DEFAULT"
            | "REFERENCES"
            | "COLLATE"
            | "GENERATED"
    )
}

fn is_table_constraint_start(upper: &str) -> bool {
    matches!(
        upper,
        "PRIMARY" | "FOREIGN" | "UNIQUE" | "CHECK" | "CONSTRAINT" | "EXCLUDE" | "LIKE"
    )
}

// ---------------------------------------------------------------------------------------------
// Names

struct NameInfo {
    key: String,
    last: String,
    single: bool,
    next: usize,
}

/// A possibly qualified name at `start`: its table key, last part and the following index.
fn parse_qualified(view: View<'_>, start: usize) -> Option<NameInfo> {
    if !view.is_name(start) {
        return None;
    }
    let mut parts: Vec<String> = vec![crate::lex::sql::ident_name(view.text(start))];
    let mut next = start.saturating_add(1);
    while view.is_punct(next, ".") && view.is_name(next.saturating_add(1)) {
        parts.push(crate::lex::sql::ident_name(
            view.text(next.saturating_add(1)),
        ));
        next = next.saturating_add(2);
    }
    let single = parts.len() == 1;
    let last = parts.last().cloned().unwrap_or_default();
    let mut key_parts = parts;
    if key_parts.len() > 1 && key_parts.first().is_some_and(|first| first == "public") {
        key_parts.remove(0);
    }
    Some(NameInfo {
        key: key_parts.join("."),
        last,
        single,
        next,
    })
}

fn skip_lateral_only(view: View<'_>, start: usize) -> usize {
    let mut index = start;
    while view.is_kw(index, "LATERAL") || view.is_kw(index, "ONLY") {
        index = index.saturating_add(1);
    }
    index
}

/// Skips an optional table alias (`AS name`, `name`, each with optional column names).
fn skip_table_alias(view: View<'_>, start: usize) -> usize {
    let mut index = start;
    if view.is_kw(index, "AS") {
        index = index.saturating_add(1);
        if view.is_name(index) {
            index = index.saturating_add(1);
        }
    } else if view.is_name(index) && !is_clause_keyword(&view.upper(index)) {
        index = index.saturating_add(1);
    } else {
        return start;
    }
    if view.is_punct(index, "(") {
        index = skip_group(view, index);
    }
    index
}

// ---------------------------------------------------------------------------------------------
// Table accesses inside one statement (SPEC 9.2 point 11)

#[derive(Default)]
struct CtxInfo {
    parent: Option<usize>,
    has_select: bool,
    for_update: bool,
    resume_from: bool,
    /// The parentheses are the arguments of a function whose syntax uses `FROM`
    /// (`EXTRACT(EPOCH FROM x)`, `SUBSTRING(x FROM 2)`, `TRIM(... FROM x)`).
    from_function: bool,
}

struct RawAccess {
    key: String,
    op: SqlOp,
    context: Option<usize>,
}

/// CTE names defined by a `WITH` at `with_index`, appended to `ctes`.
fn collect_ctes(view: View<'_>, with_index: usize, ctes: &mut Vec<String>) {
    let mut index = with_index.saturating_add(1);
    if view.is_kw(index, "RECURSIVE") {
        index = index.saturating_add(1);
    }
    loop {
        if !view.is_name(index) {
            return;
        }
        let name = crate::lex::sql::ident_name(view.text(index));
        if !ctes.contains(&name) {
            ctes.push(name);
        }
        index = index.saturating_add(1);
        if view.is_punct(index, "(") {
            index = skip_group(view, index);
        }
        if !view.is_kw(index, "AS") {
            return;
        }
        index = index.saturating_add(1);
        if view.is_kw(index, "MATERIALIZED") {
            index = index.saturating_add(1);
        } else if view.is_kw(index, "NOT") && view.is_kw(index.saturating_add(1), "MATERIALIZED") {
            index = index.saturating_add(2);
        }
        if !view.is_punct(index, "(") {
            return;
        }
        index = skip_group(view, index);
        if view.is_punct(index, ",") {
            index = index.saturating_add(1);
        } else {
            return;
        }
    }
}

/// Tables after FROM or JOIN starting at `start`; returns the index to continue scanning at.
fn parse_table_list(
    view: View<'_>,
    start: usize,
    context: usize,
    contexts: &mut [CtxInfo],
    ctes: &[String],
    raw: &mut Vec<RawAccess>,
    malformed: &mut bool,
) -> usize {
    let mut index = start;
    loop {
        index = skip_lateral_only(view, index);
        if index >= view.len() {
            *malformed = true;
            return index;
        }
        if view.is_punct(index, "(") {
            if let Some(info) = contexts.get_mut(context) {
                info.resume_from = true;
            }
            return index;
        }
        if !view.is_name(index) {
            *malformed = true;
            return index;
        }
        if view.token(index).is_some_and(|t| t.kind == TokKind::Ident)
            && is_clause_keyword(&view.upper(index))
        {
            *malformed = true;
            return index;
        }
        let Some(name) = parse_qualified(view, index) else {
            return index;
        };
        if view.is_punct(name.next, "(") {
            // A name followed by `(` is a function call, not a table.
            if let Some(info) = contexts.get_mut(context) {
                info.resume_from = true;
            }
            return name.next;
        }
        if !(name.single && ctes.contains(&name.last)) {
            raw.push(RawAccess {
                key: name.key,
                op: SqlOp::Select,
                context: Some(context),
            });
        }
        let after_alias = skip_table_alias(view, name.next);
        if view.is_punct(after_alias, ",") {
            index = after_alias.saturating_add(1);
        } else {
            return after_alias;
        }
    }
}

/// Parses one target name for INSERT, UPDATE, DELETE, TRUNCATE or MERGE.
fn parse_target(
    view: View<'_>,
    start: usize,
    raw: &mut Vec<RawAccess>,
    op: SqlOp,
) -> Option<usize> {
    let index = skip_lateral_only(view, start);
    if view.token(index).is_some_and(|t| t.kind == TokKind::Ident)
        && is_clause_keyword(&view.upper(index))
    {
        return None;
    }
    let name = parse_qualified(view, index)?;
    raw.push(RawAccess {
        key: name.key,
        op,
        context: None,
    });
    Some(name.next)
}

/// Accesses of one statement and whether a target keyword could not be read.
fn accesses_in_statement(view: View<'_>) -> (Vec<(String, SqlOp)>, bool) {
    let mut contexts: Vec<CtxInfo> = vec![CtxInfo::default()];
    let mut stack: Vec<usize> = vec![0];
    let mut ctes: Vec<String> = Vec::new();
    let mut raw: Vec<RawAccess> = Vec::new();
    let mut malformed = false;
    // `THEN INSERT` and `THEN DELETE` are actions of a MERGE only; in PL/pgSQL
    // (`IF ... THEN INSERT INTO t`) they are ordinary statements.
    let in_merge = (0..view.len()).any(|i| view.is_kw(i, "MERGE"));
    let mut index = 0usize;
    while index < view.len() {
        let current = stack.last().copied().unwrap_or(0);
        if view.is_punct(index, "(") {
            let id = contexts.len();
            let from_function = index > 0
                && matches!(
                    view.upper(index.saturating_sub(1)).as_str(),
                    "EXTRACT" | "SUBSTRING" | "TRIM" | "OVERLAY"
                );
            contexts.push(CtxInfo {
                parent: Some(current),
                has_select: false,
                for_update: false,
                resume_from: false,
                from_function,
            });
            stack.push(id);
            index = index.saturating_add(1);
            continue;
        }
        if view.is_punct(index, ")") {
            if stack.len() > 1 {
                stack.pop();
            }
            let top = stack.last().copied().unwrap_or(0);
            let resume = contexts
                .get(top)
                .map(|info| info.resume_from)
                .unwrap_or(false);
            if resume {
                if let Some(info) = contexts.get_mut(top) {
                    info.resume_from = false;
                }
                let after = skip_table_alias(view, index.saturating_add(1));
                if view.is_punct(after, ",") {
                    index = parse_table_list(
                        view,
                        after.saturating_add(1),
                        top,
                        &mut contexts,
                        &ctes,
                        &mut raw,
                        &mut malformed,
                    );
                    continue;
                }
            }
            index = index.saturating_add(1);
            continue;
        }
        if view.token(index).is_some_and(|t| t.kind != TokKind::Ident) {
            index = index.saturating_add(1);
            continue;
        }
        let upper = view.upper(index);
        match upper.as_str() {
            "SELECT" => {
                if let Some(info) = contexts.get_mut(current) {
                    info.has_select = true;
                }
                index = index.saturating_add(1);
            }
            "WITH" => {
                collect_ctes(view, index, &mut ctes);
                index = index.saturating_add(1);
            }
            "INSERT" => {
                if in_merge && index > 0 && view.is_kw(index.saturating_sub(1), "THEN") {
                    // A MERGE action; the merge target already covers it.
                    index = index.saturating_add(1);
                    continue;
                }
                let mut next = index.saturating_add(1);
                if view.is_kw(next, "INTO") {
                    next = next.saturating_add(1);
                    match parse_target(view, next, &mut raw, SqlOp::Insert) {
                        Some(after) => index = after,
                        None => {
                            malformed = true;
                            index = index.saturating_add(1);
                        }
                    }
                } else {
                    malformed = true;
                    index = index.saturating_add(1);
                }
            }
            "UPDATE" => {
                let after_for = index > 0 && view.is_kw(index.saturating_sub(1), "FOR");
                let after_no_key = index >= 3
                    && view.is_kw(index.saturating_sub(3), "FOR")
                    && view.is_kw(index.saturating_sub(2), "NO")
                    && view.is_kw(index.saturating_sub(1), "KEY");
                let target_start = skip_lateral_only(view, index.saturating_add(1));
                if after_for || after_no_key || view.is_kw(target_start, "SET") {
                    // `FOR [NO KEY] UPDATE` and `DO UPDATE SET` have no target here.
                    index = index.saturating_add(1);
                } else {
                    match parse_target(view, index.saturating_add(1), &mut raw, SqlOp::Update) {
                        Some(after) => index = after,
                        None => {
                            malformed = true;
                            index = index.saturating_add(1);
                        }
                    }
                }
            }
            "DELETE" => {
                if in_merge && index > 0 && view.is_kw(index.saturating_sub(1), "THEN") {
                    index = index.saturating_add(1);
                    continue;
                }
                let mut next = index.saturating_add(1);
                if view.is_kw(next, "FROM") {
                    next = next.saturating_add(1);
                }
                match parse_target(view, next, &mut raw, SqlOp::Delete) {
                    Some(after) => index = after,
                    None => {
                        malformed = true;
                        index = index.saturating_add(1);
                    }
                }
            }
            "TRUNCATE" => {
                let mut next = index.saturating_add(1);
                if view.is_kw(next, "TABLE") {
                    next = next.saturating_add(1);
                }
                let mut parsed = false;
                let mut cursor = next;
                while let Some(after) = parse_target(view, cursor, &mut raw, SqlOp::Truncate) {
                    parsed = true;
                    let after_alias = skip_table_alias(view, after);
                    if view.is_punct(after_alias, ",") {
                        cursor = after_alias.saturating_add(1);
                    } else {
                        cursor = after_alias;
                        break;
                    }
                }
                if parsed {
                    index = cursor;
                } else {
                    malformed = true;
                    index = index.saturating_add(1);
                }
            }
            "MERGE" => {
                let mut next = index.saturating_add(1);
                if view.is_kw(next, "INTO") {
                    next = next.saturating_add(1);
                    let before = raw.len();
                    match parse_target(view, next, &mut raw, SqlOp::Insert) {
                        Some(after) => {
                            if let Some(first) = raw.get(before) {
                                let key = first.key.clone();
                                raw.push(RawAccess {
                                    key,
                                    op: SqlOp::Update,
                                    context: None,
                                });
                            }
                            index = after;
                        }
                        None => {
                            malformed = true;
                            index = index.saturating_add(1);
                        }
                    }
                } else {
                    malformed = true;
                    index = index.saturating_add(1);
                }
            }
            // `IS DISTINCT FROM` and `FROM` inside `EXTRACT(...)` and similar name no table.
            "FROM"
                if (index > 0 && view.is_kw(index.saturating_sub(1), "DISTINCT"))
                    || contexts.get(current).is_some_and(|info| info.from_function) =>
            {
                index = index.saturating_add(1);
            }
            "FROM" | "JOIN" => {
                index = parse_table_list(
                    view,
                    index.saturating_add(1),
                    current,
                    &mut contexts,
                    &ctes,
                    &mut raw,
                    &mut malformed,
                );
            }
            "FOR" => {
                let plain = view.is_kw(index.saturating_add(1), "UPDATE");
                let no_key = view.is_kw(index.saturating_add(1), "NO")
                    && view.is_kw(index.saturating_add(2), "KEY")
                    && view.is_kw(index.saturating_add(3), "UPDATE");
                if plain || no_key {
                    if let Some(info) = contexts.get_mut(current) {
                        info.for_update = true;
                    }
                }
                index = index.saturating_add(1);
            }
            _ => {
                index = index.saturating_add(1);
            }
        }
    }
    let mut out: Vec<(String, SqlOp)> = Vec::new();
    for access in raw {
        let mut op = access.op;
        if op == SqlOp::Select {
            let mut cursor = access.context;
            while let Some(id) = cursor {
                let Some(info) = contexts.get(id) else {
                    break;
                };
                if info.has_select {
                    if info.for_update {
                        op = SqlOp::SelectForUpdate;
                    }
                    break;
                }
                cursor = info.parent;
            }
        }
        let pair = (access.key, op);
        if !out.contains(&pair) {
            out.push(pair);
        }
    }
    (out, malformed)
}

// ---------------------------------------------------------------------------------------------
// Statement dispatch

fn process_statement(file: &SourceFile, view: View<'_>) -> StmtResult<Vec<Fact>> {
    let Some(first) = view.token(0) else {
        return Ok(Vec::new());
    };
    if first.kind != TokKind::Ident {
        return Ok(Vec::new());
    }
    let line = first.line;
    match view.upper(0).as_str() {
        "CREATE" => process_create(file, view, line),
        "ALTER" => {
            if view.is_kw(1, "TABLE") {
                alter_table(file, view, line)
            } else {
                Ok(Vec::new())
            }
        }
        "DROP" => {
            if view.is_kw(1, "TABLE") {
                drop_relation(file, view, line, SqlObject::Table)
            } else if view.is_kw(1, "VIEW") {
                drop_relation(file, view, line, SqlObject::View)
            } else if view.is_kw(1, "TRIGGER") {
                drop_trigger(file, view, line)
            } else {
                Ok(Vec::new())
            }
        }
        "GRANT" => parse_grant(file, view, line, None),
        "REVOKE" => parse_grant(file, view, line, None),
        "DO" => process_do(file, view, line),
        "INSERT" | "UPDATE" | "DELETE" | "TRUNCATE" | "SELECT" | "WITH" | "MERGE" => {
            process_access_statement(file, view, line)
        }
        _ => Ok(Vec::new()),
    }
}

fn process_access_statement(file: &SourceFile, view: View<'_>, line: u32) -> StmtResult<Vec<Fact>> {
    let (accesses, malformed) = accesses_in_statement(view);
    if malformed {
        return bad("cannot read data statement");
    }
    let needed = match view.upper(0).as_str() {
        "INSERT" => Some(SqlOp::Insert),
        "UPDATE" => Some(SqlOp::Update),
        "DELETE" => Some(SqlOp::Delete),
        "TRUNCATE" => Some(SqlOp::Truncate),
        "MERGE" => Some(SqlOp::Insert),
        _ => None,
    };
    if let Some(op) = needed {
        if !accesses.iter().any(|(_, found)| *found == op) {
            return bad("cannot read data statement");
        }
    }
    let owner = file.path.as_str().to_owned();
    let mut facts = Vec::new();
    for (table, op) in accesses {
        facts.push(make_fact(
            file,
            line,
            table.clone(),
            &TableAccessData {
                owner: owner.clone(),
                table,
                op,
            },
        )?);
    }
    Ok(facts)
}

fn statement_hash(view: View<'_>) -> String {
    let mut joined = String::new();
    for index in 0..view.len() {
        if index > 0 {
            joined.push(' ');
        }
        joined.push_str(view.text(index));
    }
    fnv::hex(joined.as_bytes())
}

// ---------------------------------------------------------------------------------------------
// CREATE

fn process_create(file: &SourceFile, view: View<'_>, line: u32) -> StmtResult<Vec<Fact>> {
    let mut index = 1usize;
    if view.is_kw(index, "OR") && view.is_kw(index.saturating_add(1), "REPLACE") {
        index = index.saturating_add(2);
    }
    if view.is_kw(index, "TABLE") {
        return create_table(file, view, line, index.saturating_add(1));
    }
    if view.is_kw(index, "UNLOGGED") || view.is_kw(index, "TEMP") || view.is_kw(index, "TEMPORARY")
    {
        let mut cursor = index;
        while view.is_kw(cursor, "UNLOGGED")
            || view.is_kw(cursor, "TEMP")
            || view.is_kw(cursor, "TEMPORARY")
        {
            cursor = cursor.saturating_add(1);
        }
        if view.is_kw(cursor, "TABLE") {
            return create_table(file, view, line, cursor.saturating_add(1));
        }
        return Ok(Vec::new());
    }
    if view.is_kw(index, "MATERIALIZED") {
        return create_view(file, view, line, index, true);
    }
    if view.is_kw(index, "VIEW") {
        return create_view(file, view, line, index, false);
    }
    if view.is_kw(index, "FUNCTION") || view.is_kw(index, "PROCEDURE") {
        return create_function(file, view, line, index.saturating_add(1));
    }
    if view.is_kw(index, "TRIGGER") {
        return create_trigger(file, view, line, index);
    }
    if view.is_kw(index, "CONSTRAINT") && view.is_kw(index.saturating_add(1), "TRIGGER") {
        return create_trigger(file, view, line, index.saturating_add(1));
    }
    Ok(Vec::new())
}

fn create_table(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    start: usize,
) -> StmtResult<Vec<Fact>> {
    let mut index = start;
    if view.is_kw(index, "IF") {
        if view.is_kw(index.saturating_add(1), "NOT")
            && view.is_kw(index.saturating_add(2), "EXISTS")
        {
            index = index.saturating_add(3);
        } else {
            return bad("cannot read CREATE TABLE statement");
        }
    }
    let Some(name) = parse_qualified(view, index) else {
        return bad("missing table name in CREATE TABLE");
    };
    let table = name.key.clone();
    index = name.next;
    if !view.is_punct(index, "(") {
        return bad("missing column list in CREATE TABLE");
    }
    let Some(close) = find_matching(view, index) else {
        return bad("unbalanced column list in CREATE TABLE");
    };
    let mut facts = Vec::new();
    facts.push(make_fact(
        file,
        line,
        table.clone(),
        &TableData {
            object: SqlObject::Table,
            action: DdlAction::Create,
            end_line: view.line(close),
        },
    )?);
    for (seg_start, seg_end) in split_comma_ranges(view, index.saturating_add(1), close) {
        if seg_start >= seg_end {
            continue;
        }
        let first = seg_start;
        if view.token(first).is_some_and(|t| t.kind == TokKind::Ident)
            && is_table_constraint_start(&view.upper(first))
        {
            continue;
        }
        if !view.is_name(first) {
            return bad("cannot read column definition in CREATE TABLE");
        }
        let column = crate::lex::sql::ident_name(view.text(first));
        let mut type_end = seg_end;
        let mut cursor = first.saturating_add(1);
        while cursor < seg_end {
            if view.token(cursor).is_some_and(|t| t.kind == TokKind::Ident)
                && is_column_constraint(&view.upper(cursor))
            {
                type_end = cursor;
                break;
            }
            cursor = cursor.saturating_add(1);
        }
        let mut type_parts: Vec<&str> = Vec::new();
        let mut token_index = first.saturating_add(1);
        while token_index < type_end {
            type_parts.push(view.text(token_index));
            token_index = token_index.saturating_add(1);
        }
        let sql_type = if type_parts.is_empty() {
            None
        } else {
            Some(type_parts.join(" "))
        };
        let sequence = owned_sequence(view, first.saturating_add(1), seg_end, &table, &column);
        facts.push(make_fact(
            file,
            view.line(first),
            format!("{table}.{column}"),
            &ColumnData {
                table: table.clone(),
                column,
                sql_type,
                action: DdlAction::Create,
                sequence,
            },
        )?);
    }
    Ok(facts)
}

fn create_view(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    view_index: usize,
    materialized: bool,
) -> StmtResult<Vec<Fact>> {
    let name_start = if materialized {
        if !view.is_kw(view_index.saturating_add(1), "VIEW") {
            return bad("cannot read CREATE VIEW statement");
        }
        view_index.saturating_add(2)
    } else {
        view_index.saturating_add(1)
    };
    let Some(name) = parse_qualified(view, name_start) else {
        return bad("missing view name in CREATE VIEW");
    };
    let table = name.key.clone();
    let has_query = (0..view.len()).any(|i| view.is_kw(i, "SELECT") || view.is_kw(i, "WITH"));
    if !has_query {
        return bad("missing query in CREATE VIEW");
    }
    let object = if materialized {
        SqlObject::MaterializedView
    } else {
        SqlObject::View
    };
    let mut facts = Vec::new();
    facts.push(make_fact(
        file,
        line,
        table.clone(),
        &TableData {
            object,
            action: DdlAction::Create,
            end_line: view.last_line(),
        },
    )?);
    let (accesses, _) = accesses_in_statement(view);
    for (accessed, op) in accesses {
        facts.push(make_fact(
            file,
            line,
            accessed.clone(),
            &TableAccessData {
                owner: table.clone(),
                table: accessed,
                op,
            },
        )?);
    }
    Ok(facts)
}

// ---------------------------------------------------------------------------------------------
// ALTER and DROP

fn push_column(
    facts: &mut Vec<Fact>,
    file: &SourceFile,
    line: u32,
    table: &str,
    column: String,
    sql_type: Option<String>,
    action: DdlAction,
) -> StmtResult<()> {
    facts.push(make_fact(
        file,
        line,
        format!("{table}.{column}"),
        &ColumnData {
            table: table.to_owned(),
            column,
            sql_type,
            action,
            sequence: None,
        },
    )?);
    Ok(())
}

/// Default name of the sequence a column definition (tokens `start..end` after the column
/// name) creates: a `serial` type or `GENERATED ... AS IDENTITY`.
fn owned_sequence(
    view: View<'_>,
    start: usize,
    end: usize,
    table: &str,
    column: &str,
) -> Option<String> {
    let serial = matches!(
        view.upper(start).as_str(),
        "SERIAL" | "BIGSERIAL" | "SMALLSERIAL" | "SERIAL2" | "SERIAL4" | "SERIAL8"
    );
    let identity = (start..end).any(|index| view.is_kw(index, "IDENTITY"));
    (serial || identity).then(|| format!("{table}_{column}_seq"))
}

fn column_type(view: View<'_>, start: usize, end: usize) -> Option<String> {
    let mut type_end = end;
    let mut cursor = start;
    while cursor < end {
        if view.token(cursor).is_some_and(|t| t.kind == TokKind::Ident)
            && is_column_constraint(&view.upper(cursor))
        {
            type_end = cursor;
            break;
        }
        cursor = cursor.saturating_add(1);
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut index = start;
    while index < type_end {
        parts.push(view.text(index));
        index = index.saturating_add(1);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn alter_table(file: &SourceFile, view: View<'_>, line: u32) -> StmtResult<Vec<Fact>> {
    let mut index = 2usize;
    if view.is_kw(index, "IF") {
        if view.is_kw(index.saturating_add(1), "EXISTS") {
            index = index.saturating_add(2);
        } else {
            return bad("cannot read ALTER TABLE statement");
        }
    }
    index = skip_lateral_only(view, index);
    // `skip_lateral_only` also skips ONLY, which is the keyword allowed here.
    let Some(name) = parse_qualified(view, index) else {
        return bad("missing table name in ALTER TABLE");
    };
    let table = name.key.clone();
    index = name.next;
    if index >= view.len() {
        return bad("missing action in ALTER TABLE");
    }
    let end_line = view.last_line();
    let mut facts = Vec::new();
    facts.push(make_fact(
        file,
        line,
        table.clone(),
        &TableData {
            object: SqlObject::Table,
            action: DdlAction::Alter,
            end_line,
        },
    )?);
    for (seg_start, seg_end) in split_comma_ranges(view, index, view.len()) {
        if seg_start >= seg_end {
            continue;
        }
        let action = view.upper(seg_start);
        let mut cursor = seg_start.saturating_add(1);
        match action.as_str() {
            "ADD" => {
                if cursor < seg_end
                    && view.token(cursor).is_some_and(|t| t.kind == TokKind::Ident)
                    && is_table_constraint_start(&view.upper(cursor))
                {
                    continue;
                }
                if view.is_kw(cursor, "COLUMN") {
                    cursor = cursor.saturating_add(1);
                }
                if view.is_kw(cursor, "IF") {
                    if view.is_kw(cursor.saturating_add(1), "NOT")
                        && view.is_kw(cursor.saturating_add(2), "EXISTS")
                    {
                        cursor = cursor.saturating_add(3);
                    } else {
                        return bad("cannot read ALTER TABLE ADD");
                    }
                }
                if !view.is_name(cursor) {
                    return bad("missing column name in ALTER TABLE ADD");
                }
                let column = crate::lex::sql::ident_name(view.text(cursor));
                let sql_type = column_type(view, cursor.saturating_add(1), seg_end);
                let sequence =
                    owned_sequence(view, cursor.saturating_add(1), seg_end, &table, &column);
                facts.push(make_fact(
                    file,
                    view.line(cursor),
                    format!("{table}.{column}"),
                    &ColumnData {
                        table: table.clone(),
                        column,
                        sql_type,
                        action: DdlAction::Create,
                        sequence,
                    },
                )?);
            }
            "DROP" => {
                if cursor < seg_end
                    && view.token(cursor).is_some_and(|t| t.kind == TokKind::Ident)
                    && is_table_constraint_start(&view.upper(cursor))
                {
                    continue;
                }
                if view.is_kw(cursor, "COLUMN") {
                    cursor = cursor.saturating_add(1);
                }
                if view.is_kw(cursor, "IF") {
                    if view.is_kw(cursor.saturating_add(1), "EXISTS") {
                        cursor = cursor.saturating_add(2);
                    } else {
                        return bad("cannot read ALTER TABLE DROP");
                    }
                }
                if !view.is_name(cursor) {
                    return bad("missing column name in ALTER TABLE DROP");
                }
                let column = crate::lex::sql::ident_name(view.text(cursor));
                push_column(
                    &mut facts,
                    file,
                    view.line(cursor),
                    &table,
                    column,
                    None,
                    DdlAction::Drop,
                )?;
            }
            "ALTER" => {
                if view.is_kw(cursor, "COLUMN") {
                    cursor = cursor.saturating_add(1);
                }
                if !view.is_name(cursor) {
                    return bad("missing column name in ALTER TABLE ALTER");
                }
                let column = crate::lex::sql::ident_name(view.text(cursor));
                push_column(
                    &mut facts,
                    file,
                    view.line(cursor),
                    &table,
                    column,
                    None,
                    DdlAction::Alter,
                )?;
            }
            // A constraint name is not a column; renaming it changes no fact.
            "RENAME" if view.is_kw(cursor, "CONSTRAINT") => {}
            "RENAME" => {
                if view.is_kw(cursor, "TO") {
                    let Some(new_name) = parse_qualified(view, cursor.saturating_add(1)) else {
                        return bad("missing new name in ALTER TABLE RENAME");
                    };
                    facts.push(make_fact(
                        file,
                        line,
                        table.clone(),
                        &TableData {
                            object: SqlObject::Table,
                            action: DdlAction::Rename,
                            end_line,
                        },
                    )?);
                    facts.push(make_fact(
                        file,
                        line,
                        new_name.key.clone(),
                        &TableData {
                            object: SqlObject::Table,
                            action: DdlAction::Create,
                            end_line,
                        },
                    )?);
                } else {
                    if view.is_kw(cursor, "COLUMN") {
                        cursor = cursor.saturating_add(1);
                    }
                    if !view.is_name(cursor) {
                        return bad("missing column name in ALTER TABLE RENAME");
                    }
                    let old_column = crate::lex::sql::ident_name(view.text(cursor));
                    let old_line = view.line(cursor);
                    let to_index = cursor.saturating_add(1);
                    if !view.is_kw(to_index, "TO") || !view.is_name(to_index.saturating_add(1)) {
                        return bad("cannot read ALTER TABLE RENAME");
                    }
                    let new_index = to_index.saturating_add(1);
                    let new_column = crate::lex::sql::ident_name(view.text(new_index));
                    push_column(
                        &mut facts,
                        file,
                        old_line,
                        &table,
                        old_column,
                        None,
                        DdlAction::Rename,
                    )?;
                    push_column(
                        &mut facts,
                        file,
                        view.line(new_index),
                        &table,
                        new_column,
                        None,
                        DdlAction::Create,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}

fn drop_relation(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    object: SqlObject,
) -> StmtResult<Vec<Fact>> {
    let mut index = 2usize;
    if view.is_kw(index, "IF") {
        if view.is_kw(index.saturating_add(1), "EXISTS") {
            index = index.saturating_add(2);
        } else {
            return bad("cannot read DROP statement");
        }
    }
    let mut facts = Vec::new();
    loop {
        let Some(name) = parse_qualified(view, index) else {
            return bad("missing name in DROP statement");
        };
        index = name.next;
        facts.push(make_fact(
            file,
            line,
            name.key,
            &TableData {
                object,
                action: DdlAction::Drop,
                end_line: view.last_line(),
            },
        )?);
        if view.is_punct(index, ",") {
            index = index.saturating_add(1);
        } else {
            break;
        }
    }
    Ok(facts)
}

fn drop_trigger(file: &SourceFile, view: View<'_>, line: u32) -> StmtResult<Vec<Fact>> {
    let mut index = 2usize;
    if view.is_kw(index, "IF") {
        if view.is_kw(index.saturating_add(1), "EXISTS") {
            index = index.saturating_add(2);
        } else {
            return bad("cannot read DROP TRIGGER statement");
        }
    }
    let Some(name) = parse_qualified(view, index) else {
        return bad("missing trigger name in DROP TRIGGER");
    };
    let trigger_name = name.last.clone();
    index = name.next;
    if !view.is_kw(index, "ON") {
        return bad("missing ON in DROP TRIGGER");
    }
    let Some(table_name) = parse_qualified(view, index.saturating_add(1)) else {
        return bad("missing table name in DROP TRIGGER");
    };
    let table = table_name.key;
    Ok(vec![make_fact(
        file,
        line,
        format!("{table}.{trigger_name}"),
        &TriggerData {
            name: trigger_name,
            table,
            function: String::new(),
            timing: String::new(),
            events: Vec::new(),
            action: DdlAction::Drop,
        },
    )?])
}

// ---------------------------------------------------------------------------------------------
// GRANT and REVOKE

/// Finds a keyword at parenthesis depth zero in `start..end`.
fn find_keyword(view: View<'_>, start: usize, end: usize, keyword: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = start;
    while index < end && index < view.len() {
        if view.is_punct(index, "(") {
            depth = depth.saturating_add(1);
        } else if view.is_punct(index, ")") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && view.is_kw(index, keyword) {
            return Some(index);
        }
        index = index.saturating_add(1);
    }
    None
}

fn parse_grant(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    owner: Option<String>,
) -> StmtResult<Vec<Fact>> {
    let mut facts = Vec::new();
    for data in grant_data(view, owner)? {
        facts.push(make_fact(file, line, data.key(), &data)?);
    }
    Ok(facts)
}

/// One `GrantData` per privilege (and column), object and role of a GRANT or REVOKE.
fn grant_data(view: View<'_>, owner: Option<String>) -> StmtResult<Vec<GrantData>> {
    let revoke = view.is_kw(0, "REVOKE");
    let role_keyword = if revoke { "FROM" } else { "TO" };
    let Some(on_index) = find_keyword(view, 1, view.len(), "ON") else {
        // `GRANT role TO role` (role membership) grants no privilege on an object.
        if find_keyword(view, 1, view.len(), role_keyword).is_some() {
            return Ok(Vec::new());
        }
        return bad("missing ON in grant statement");
    };
    // A privilege with its column list; no columns means the whole object.
    let mut privileges: Vec<(String, Vec<String>)> = Vec::new();
    for (seg_start, seg_end) in split_comma_ranges(view, 1, on_index) {
        if seg_start >= seg_end {
            continue;
        }
        if view
            .token(seg_start)
            .is_some_and(|t| t.kind != TokKind::Ident)
        {
            return bad("cannot read privilege in grant statement");
        }
        let mut columns: Vec<String> = Vec::new();
        if let Some(open) = (seg_start..seg_end).find(|index| view.is_punct(*index, "(")) {
            let Some(close) = find_matching(view, open).filter(|close| *close < seg_end) else {
                return bad("cannot read column list in grant statement");
            };
            for (col_start, col_end) in split_comma_ranges(view, open.saturating_add(1), close) {
                if col_start >= col_end {
                    continue;
                }
                if !view.is_name(col_start) {
                    return bad("cannot read column in grant statement");
                }
                columns.push(crate::lex::sql::ident_name(view.text(col_start)));
            }
            if columns.is_empty() {
                return bad("empty column list in grant statement");
            }
        }
        privileges.push((view.upper(seg_start), columns));
    }
    if privileges.is_empty() {
        return bad("missing privilege in grant statement");
    }
    let mut cursor = on_index.saturating_add(1);
    let object_kind: String;
    let mut objects: Vec<String> = Vec::new();
    let delimiter: usize;
    let all_kind = view.upper(cursor.saturating_add(1));
    if view.is_kw(cursor, "ALL")
        && matches!(
            all_kind.as_str(),
            "TABLES" | "SEQUENCES" | "FUNCTIONS" | "PROCEDURES" | "ROUTINES"
        )
        && view.is_kw(cursor.saturating_add(2), "IN")
        && view.is_kw(cursor.saturating_add(3), "SCHEMA")
    {
        object_kind = format!("ALL {all_kind} IN SCHEMA");
        cursor = cursor.saturating_add(4);
        let Some(schema) = parse_qualified(view, cursor) else {
            return bad("missing schema name in grant statement");
        };
        objects.push(schema.key);
        let Some(found) = find_keyword(view, schema.next, view.len(), role_keyword) else {
            return bad("missing role in grant statement");
        };
        delimiter = found;
    } else {
        if view.is_kw(cursor, "TABLE")
            || view.is_kw(cursor, "SEQUENCE")
            || view.is_kw(cursor, "FUNCTION")
            || view.is_kw(cursor, "SCHEMA")
        {
            object_kind = view.upper(cursor);
            cursor = cursor.saturating_add(1);
        } else {
            object_kind = "TABLE".to_owned();
        }
        let Some(found) = find_keyword(view, cursor, view.len(), role_keyword) else {
            return bad("missing role in grant statement");
        };
        delimiter = found;
        for (seg_start, seg_end) in split_comma_ranges(view, cursor, delimiter) {
            if seg_start >= seg_end {
                continue;
            }
            let Some(object) = parse_qualified(view, seg_start) else {
                return bad("cannot read object in grant statement");
            };
            objects.push(object.key);
        }
    }
    if objects.is_empty() {
        return bad("missing object in grant statement");
    }
    let mut roles: Vec<String> = Vec::new();
    for (seg_start, seg_end) in split_comma_ranges(view, delimiter.saturating_add(1), view.len()) {
        if seg_start >= seg_end {
            continue;
        }
        if !view.is_name(seg_start) {
            return bad("cannot read role in grant statement");
        }
        roles.push(crate::lex::sql::ident_name(view.text(seg_start)));
    }
    if roles.is_empty() {
        return bad("missing role in grant statement");
    }
    let mut grants = Vec::new();
    for (privilege, columns) in &privileges {
        let columns: Vec<Option<&String>> = if columns.is_empty() {
            vec![None]
        } else {
            columns.iter().map(Some).collect()
        };
        for column in &columns {
            for object in &objects {
                for role in &roles {
                    grants.push(GrantData {
                        revoke,
                        privilege: privilege.clone(),
                        object_kind: object_kind.clone(),
                        object: object.clone(),
                        role: role.clone(),
                        column: column.cloned(),
                        owner: owner.clone(),
                    });
                }
            }
        }
    }
    Ok(grants)
}

/// A `FOREACH` loop variable (lowercased) and the constant values it takes.
type LoopVar = (String, Vec<String>);

/// `FOREACH <var> IN ARRAY ARRAY['a', 'b'] LOOP` at `index`, with every element a string
/// literal; `None` for any other loop.
fn foreach_values(view: View<'_>, index: usize) -> Option<LoopVar> {
    let var = index.checked_add(1)?;
    let ok = view.is_kw(index, "FOREACH")
        && view.is_name(var)
        && view.is_kw(var.checked_add(1)?, "IN")
        && view.is_kw(var.checked_add(2)?, "ARRAY")
        && view.is_kw(var.checked_add(3)?, "ARRAY")
        && view.is_punct(var.checked_add(4)?, "[");
    if !ok {
        return None;
    }
    let mut values = Vec::new();
    let mut at = var.checked_add(5)?;
    loop {
        let value = view
            .token(at)
            .filter(|token| token.kind == TokKind::Str)
            .and_then(|token| crate::lex::sql::unquote(token.text(view.src)))?;
        values.push(value);
        at = at.checked_add(1)?;
        if view.is_punct(at, ",") {
            at = at.checked_add(1)?;
        } else if view.is_punct(at, "]") && view.is_kw(at.checked_add(1)?, "LOOP") {
            return Some((view.text(var).to_ascii_lowercase(), values));
        } else {
            return None;
        }
    }
}

/// The variable an `IF` or `ELSIF` condition at `index` limits, and the values it allows:
/// `IF v IN ('a', 'b') THEN` or `IF v = 'a' THEN`; `None` for any other condition.
fn if_values(view: View<'_>, index: usize) -> Option<LoopVar> {
    let var = index.checked_add(1)?;
    if !view.is_name(var) {
        return None;
    }
    let text_at = |at: usize| {
        view.token(at)
            .filter(|token| token.kind == TokKind::Str)
            .and_then(|token| crate::lex::sql::unquote(token.text(view.src)))
    };
    let mut values = Vec::new();
    let mut at = var.checked_add(1)?;
    if view.is_punct(at, "=") {
        values.push(text_at(at.checked_add(1)?)?);
        at = at.checked_add(2)?;
    } else if view.is_kw(at, "IN") && view.is_punct(at.checked_add(1)?, "(") {
        at = at.checked_add(2)?;
        loop {
            values.push(text_at(at)?);
            at = at.checked_add(1)?;
            if view.is_punct(at, ",") {
                at = at.checked_add(1)?;
            } else if view.is_punct(at, ")") {
                at = at.checked_add(1)?;
                break;
            } else {
                return None;
            }
        }
    } else {
        return None;
    }
    if !view.is_kw(at, "THEN") {
        return None;
    }
    Some((view.text(var).to_ascii_lowercase(), values))
}

/// One argument of `format(...)`: a string literal or the index of a `FOREACH` variable.
enum FormatArg {
    Text(String),
    Loop(usize),
}

/// Grants of `EXECUTE format('GRANT …', arg, …)` at `view`, one per value of the `FOREACH`
/// variables in `loops` that the arguments use. Each argument must be a string literal or such
/// a variable; anything else (a query loop variable, a function call) gives no fact, because
/// the grant's object or role is not known.
fn format_grants(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    owner: &str,
    loops: &[LoopVar],
) -> StmtResult<Vec<Fact>> {
    if !view.is_kw(1, "format") || !view.is_punct(2, "(") {
        return Ok(Vec::new());
    }
    let Some(text) = view
        .token(3)
        .filter(|token| token.kind == TokKind::Str)
        .and_then(|token| crate::lex::sql::unquote(token.text(view.src)))
    else {
        return Ok(Vec::new());
    };
    let mut args: Vec<FormatArg> = Vec::new();
    let mut at: usize = 4;
    while view.is_punct(at, ",") {
        let arg = at.saturating_add(1);
        let token = view.token(arg);
        if let Some(value) = token
            .filter(|token| token.kind == TokKind::Str)
            .and_then(|token| crate::lex::sql::unquote(token.text(view.src)))
        {
            args.push(FormatArg::Text(value));
        } else {
            let name = view.text(arg).to_ascii_lowercase();
            // The innermost loop with that variable name.
            let Some(index) = loops.iter().rposition(|(var, _)| *var == name) else {
                return Ok(Vec::new());
            };
            args.push(FormatArg::Loop(index));
        }
        at = arg.saturating_add(1);
    }
    if !view.is_punct(at, ")") {
        return Ok(Vec::new());
    }
    // Every combination of values of the loops the arguments use.
    let mut used: Vec<usize> = Vec::new();
    for arg in &args {
        if let FormatArg::Loop(index) = arg {
            if !used.contains(index) {
                used.push(*index);
            }
        }
    }
    let mut choices: Vec<Vec<(usize, &str)>> = vec![Vec::new()];
    for index in used {
        let Some((_, values)) = loops.get(index) else {
            return Ok(Vec::new());
        };
        let mut next = Vec::new();
        for choice in &choices {
            for value in values {
                let mut extended = choice.clone();
                extended.push((index, value.as_str()));
                next.push(extended);
            }
        }
        choices = next;
    }
    let mut facts = Vec::new();
    for choice in &choices {
        let Some(sql) = format_text(&text, &args, choice) else {
            return Ok(Vec::new());
        };
        let lexed = crate::lex::sql::lex(&sql);
        let inner = View {
            src: &sql,
            tokens: &lexed.tokens,
        };
        if !inner.is_kw(0, "GRANT") && !inner.is_kw(0, "REVOKE") {
            return Ok(Vec::new());
        }
        for data in grant_data(inner, Some(owner.to_owned()))? {
            facts.push(make_fact(file, line, data.key(), &data)?);
        }
    }
    Ok(facts)
}

/// `text` with `%I`, `%s` and `%L` replaced by the arguments in order (`%L` quoted) and `%%`
/// by `%`; `None` when the placeholders and arguments do not pair up.
fn format_text(text: &str, args: &[FormatArg], choice: &[(usize, &str)]) -> Option<String> {
    let mut out = String::new();
    let mut args = args.iter();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = chars.next()?;
        if spec == '%' {
            out.push('%');
            continue;
        }
        let value = match args.next()? {
            FormatArg::Text(value) => value.as_str(),
            FormatArg::Loop(index) => choice.iter().find(|(loop_index, _)| loop_index == index)?.1,
        };
        match spec {
            'I' | 's' => out.push_str(value),
            'L' => {
                out.push('\'');
                out.push_str(&value.replace('\'', "''"));
                out.push('\'');
            }
            _ => return None,
        }
    }
    if args.next().is_some() {
        return None;
    }
    Some(out)
}

/// Grant facts of statements in a lexed function or `DO` body.
fn body_grants(
    file: &SourceFile,
    body: &str,
    base_line: u32,
    owner: &str,
) -> StmtResult<Vec<Fact>> {
    let lexed = crate::lex::sql::lex(body);
    let mut facts = Vec::new();
    // Loops open so far, innermost last: `LOOP` opens one, `END LOOP` closes one. A
    // `FOREACH` over a constant array keeps its variable and values.
    let mut loops: Vec<Option<LoopVar>> = Vec::new();
    let mut foreach: Option<LoopVar> = None;
    // `IF` blocks open so far, innermost last, with the values a branch condition such as
    // `IF r IN ('a', 'b') THEN` leaves to a variable; `None` for any other condition.
    let mut ifs: Vec<Option<LoopVar>> = Vec::new();
    for statement in statements(body, &lexed.tokens) {
        let whole = View {
            src: body,
            tokens: statement,
        };
        // A grant opens the statement or follows the keyword that opens a block or branch
        // (`BEGIN GRANT …`, `IF … THEN GRANT …`, `ELSE GRANT …`, `LOOP GRANT …`); the same
        // for `EXECUTE`, with the `FOREACH` variables open there, limited by the open `IF`
        // conditions on them.
        let mut start: Option<(usize, Vec<LoopVar>)> = None;
        for index in 0..whole.len() {
            let before = index.checked_sub(1);
            if whole.is_kw(index, "FOREACH") {
                foreach = foreach_values(whole, index);
            }
            if whole.is_kw(index, "LOOP") {
                if before.is_some_and(|at| whole.is_kw(at, "END")) {
                    loops.pop();
                } else {
                    loops.push(foreach.take());
                }
            }
            let opens = before.is_none_or(|at| {
                ["BEGIN", "THEN", "ELSE", "LOOP"]
                    .iter()
                    .any(|keyword| whole.is_kw(at, keyword))
            });
            if whole.is_kw(index, "IF") {
                if before.is_some_and(|at| whole.is_kw(at, "END")) {
                    ifs.pop();
                } else if opens {
                    // Not `DROP TABLE IF EXISTS` and the like: those do not open a block.
                    ifs.push(if_values(whole, index));
                }
            }
            if opens && whole.is_kw(index, "ELSIF") {
                if let Some(branch) = ifs.last_mut() {
                    *branch = if_values(whole, index);
                }
            }
            if opens && whole.is_kw(index, "ELSE") {
                if let Some(branch) = ifs.last_mut() {
                    *branch = None;
                }
            }
            let keyword = ["GRANT", "REVOKE", "EXECUTE"]
                .iter()
                .any(|keyword| whole.is_kw(index, keyword));
            if start.is_none() && opens && keyword {
                let mut open: Vec<LoopVar> = loops.iter().flatten().cloned().collect();
                for (var, allowed) in ifs.iter().flatten() {
                    for (loop_var, values) in &mut open {
                        if *loop_var == *var {
                            values.retain(|value| allowed.contains(value));
                        }
                    }
                }
                start = Some((index, open));
            }
        }
        let Some((start, open_loops)) = start else {
            continue;
        };
        let Some(tokens) = statement.get(start..) else {
            continue;
        };
        let view = View { src: body, tokens };
        let line = base_line.saturating_add(view.line(0)).saturating_sub(1);
        let parsed = if view.is_kw(0, "EXECUTE") {
            format_grants(file, view, line, owner, &open_loops)
        } else {
            parse_grant(file, view, line, Some(owner.to_owned()))
        };
        match parsed {
            Ok(found) => facts.extend(found),
            // A grant that cannot be read does not hide the enclosing definition.
            Err(StmtError::Bad(_)) => {}
            Err(fatal) => return Err(fatal),
        }
    }
    Ok(facts)
}

// ---------------------------------------------------------------------------------------------
// Functions, procedures, DO blocks and triggers

struct BodyInfo {
    text: String,
    line: u32,
}

fn body_from_token(view: View<'_>, index: usize) -> Option<BodyInfo> {
    let token = view.token(index)?;
    let text = token.text(view.src);
    let value = match token.kind {
        TokKind::DollarStr => crate::lex::sql::dollar_body(text).map(|s| s.to_owned()),
        TokKind::Str => crate::lex::sql::unquote(text),
        _ => None,
    }?;
    Some(BodyInfo {
        text: value,
        line: token.line,
    })
}

fn language_from_token(view: View<'_>, index: usize) -> Option<String> {
    let token = view.token(index)?;
    match token.kind {
        TokKind::Ident | TokKind::QuotedIdent => {
            Some(crate::lex::sql::ident_name(token.text(view.src)).to_ascii_lowercase())
        }
        TokKind::Str => {
            crate::lex::sql::unquote(token.text(view.src)).map(|value| value.to_ascii_lowercase())
        }
        _ => None,
    }
}

fn scannable_language(language: &Option<String>) -> bool {
    match language.as_deref() {
        None => true,
        Some("sql") | Some("plpgsql") => true,
        Some(_) => false,
    }
}

fn scan_function_tail(view: View<'_>, start: usize) -> (Option<BodyInfo>, Option<String>) {
    let mut body = None;
    let mut language = None;
    let mut index = start;
    while index < view.len() {
        if view.is_kw(index, "AS") && body.is_none() {
            body = body_from_token(view, index.saturating_add(1));
            index = index.saturating_add(2);
        } else if view.is_kw(index, "LANGUAGE") && language.is_none() {
            language = language_from_token(view, index.saturating_add(1));
            index = index.saturating_add(2);
        } else {
            index = index.saturating_add(1);
        }
    }
    (body, language)
}

fn body_access_facts(
    file: &SourceFile,
    line: u32,
    owner: &str,
    body: &BodyInfo,
) -> StmtResult<Vec<Fact>> {
    let mut facts = Vec::new();
    for (table, op) in sql_text_accesses(&body.text) {
        facts.push(make_fact(
            file,
            line,
            table.clone(),
            &TableAccessData {
                owner: owner.to_owned(),
                table,
                op,
            },
        )?);
    }
    facts.extend(body_grants(file, &body.text, body.line, owner)?);
    Ok(facts)
}

fn create_function(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    name_start: usize,
) -> StmtResult<Vec<Fact>> {
    let Some(name) = parse_qualified(view, name_start) else {
        return bad("missing function name in CREATE FUNCTION");
    };
    let function_name = name.last.clone();
    let key = format!("sql:{}", name.key);
    let mut index = name.next;
    if !view.is_punct(index, "(") {
        return bad("missing argument list in CREATE FUNCTION");
    }
    let Some(close) = find_matching(view, index) else {
        return bad("unbalanced argument list in CREATE FUNCTION");
    };
    index = close.saturating_add(1);
    let (body, language) = scan_function_tail(view, index);
    let mut facts = Vec::new();
    facts.push(make_fact(
        file,
        line,
        key.clone(),
        &SqlFunctionData {
            name: function_name,
            language: language.clone(),
            action: DdlAction::Create,
            end_line: view.last_line(),
            hash: statement_hash(view),
        },
    )?);
    if scannable_language(&language) {
        if let Some(body) = &body {
            facts.extend(body_access_facts(file, line, &key, body)?);
        }
    }
    Ok(facts)
}

fn process_do(file: &SourceFile, view: View<'_>, line: u32) -> StmtResult<Vec<Fact>> {
    let owner = format!("{}#do@{}", file.path.as_str(), line);
    let mut body = None;
    let mut language = None;
    let mut index = 1usize;
    while index < view.len() {
        if view.is_kw(index, "LANGUAGE") && language.is_none() {
            language = language_from_token(view, index.saturating_add(1));
            index = index.saturating_add(2);
        } else if body.is_none() {
            match body_from_token(view, index) {
                Some(found) => {
                    body = Some(found);
                    index = index.saturating_add(1);
                }
                None => {
                    index = index.saturating_add(1);
                }
            }
        } else {
            index = index.saturating_add(1);
        }
    }
    let Some(body) = body else {
        return bad("missing body in DO statement");
    };
    if scannable_language(&language) {
        body_access_facts(file, line, &owner, &body)
    } else {
        Ok(Vec::new())
    }
}

fn create_trigger(
    file: &SourceFile,
    view: View<'_>,
    line: u32,
    trigger_index: usize,
) -> StmtResult<Vec<Fact>> {
    let Some(name) = parse_qualified(view, trigger_index.saturating_add(1)) else {
        return bad("missing trigger name in CREATE TRIGGER");
    };
    let trigger_name = name.last.clone();
    let mut cursor = name.next;
    let mut on_index = None;
    let mut index = cursor;
    while index < view.len() {
        if view.is_kw(index, "ON") {
            on_index = Some(index);
            break;
        }
        index = index.saturating_add(1);
    }
    let Some(on_index) = on_index else {
        return bad("missing ON in CREATE TRIGGER");
    };
    let mut timing = String::new();
    let mut events: Vec<String> = Vec::new();
    let mut scan = cursor;
    while scan < on_index {
        let upper = view.upper(scan);
        match upper.as_str() {
            "BEFORE" | "AFTER" => timing = upper,
            "INSTEAD" => {
                if view.is_kw(scan.saturating_add(1), "OF") {
                    timing = "INSTEAD OF".to_owned();
                }
            }
            "INSERT" | "UPDATE" | "DELETE" | "TRUNCATE" if !events.contains(&upper) => {
                events.push(upper);
            }
            _ => {}
        }
        scan = scan.saturating_add(1);
    }
    if timing.is_empty() {
        return bad("missing timing in CREATE TRIGGER");
    }
    if events.is_empty() {
        return bad("missing event in CREATE TRIGGER");
    }
    cursor = skip_lateral_only(view, on_index.saturating_add(1));
    let Some(table_name) = parse_qualified(view, cursor) else {
        return bad("missing table name in CREATE TRIGGER");
    };
    let table = table_name.key.clone();
    let mut execute_index = None;
    let mut search = table_name.next;
    while search < view.len() {
        if view.is_kw(search, "EXECUTE") {
            execute_index = Some(search);
            break;
        }
        search = search.saturating_add(1);
    }
    let Some(execute_index) = execute_index else {
        return bad("missing EXECUTE in CREATE TRIGGER");
    };
    let function_start = execute_index.saturating_add(1);
    if !view.is_kw(function_start, "FUNCTION") && !view.is_kw(function_start, "PROCEDURE") {
        return bad("missing function in CREATE TRIGGER");
    }
    let Some(function) = parse_qualified(view, function_start.saturating_add(1)) else {
        return bad("missing function in CREATE TRIGGER");
    };
    Ok(vec![make_fact(
        file,
        line,
        format!("{table}.{trigger_name}"),
        &TriggerData {
            name: trigger_name,
            table,
            function: function.key,
            timing,
            events,
            action: DdlAction::Create,
        },
    )?])
}

#[cfg(test)]
mod tests {
    use super::sql_preview;
    use crate::extract::test_support::{facts, keys};
    use crate::model::FactKind;

    #[test]
    fn preview_masks_literals_and_collapses_whitespace() {
        assert_eq!(
            sql_preview("SELECT *\n  FROM users WHERE pass = 'x y' AND n = 'it''s'"),
            "SELECT * FROM users WHERE pass = '?' AND n = '?''?'"
        );
        assert_eq!(sql_preview(&"a ".repeat(200)).chars().count(), 120);
    }

    #[test]
    fn statements_that_are_not_unparsed() {
        let facts = facts(
            "db/1.sql",
            "GRANT reader TO alice;\nALTER TABLE orders RENAME CONSTRAINT a TO b;\nGRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;\n",
        );
        assert!(keys(&facts, FactKind::Unparsed).is_empty(), "{facts:?}");
        let kinds: Vec<String> = facts
            .iter()
            .filter(|fact| fact.kind == FactKind::Grant)
            .map(|fact| fact.data_text("object_kind").unwrap_or_default())
            .collect();
        assert_eq!(kinds, vec!["ALL TABLES IN SCHEMA".to_owned()]);
    }

    fn grant_keys(facts: &[crate::model::Fact]) -> Vec<String> {
        let mut found = keys(facts, FactKind::Grant);
        found.sort();
        found
    }

    #[test]
    fn column_grants_and_grants_after_then() {
        let facts = facts(
            "db/grants.sql",
            "GRANT SELECT (id, total) ON orders TO reader;\n\
             CREATE FUNCTION setup() RETURNS void AS $$\n\
             BEGIN\n\
               IF true THEN GRANT UPDATE ON stock TO worker; ELSE GRANT DELETE ON stock TO worker; END IF;\n\
             END;\n\
             $$ LANGUAGE plpgsql;\n",
        );
        assert_eq!(
            grant_keys(&facts),
            vec![
                "DELETE stock worker".to_owned(),
                "SELECT(id) orders reader".to_owned(),
                "SELECT(total) orders reader".to_owned(),
                "UPDATE stock worker".to_owned(),
            ]
        );
    }

    #[test]
    fn execute_format_grants_expand_constant_loops() {
        let facts = facts(
            "db/grants.sql",
            "CREATE FUNCTION grant_all() RETURNS void AS $$\n\
             DECLARE t text; r text;\n\
             BEGIN\n\
               EXECUTE format('GRANT TRUNCATE ON %I TO worker', 'stock');\n\
               FOR t IN SELECT tablename FROM pg_tables LOOP\n\
                 EXECUTE format('GRANT SELECT, INSERT ON %I TO worker', t);\n\
               END LOOP;\n\
               FOREACH r IN ARRAY ARRAY['api', 'clock'] LOOP\n\
                 IF true THEN EXECUTE format('GRANT SELECT ON orders TO %I', r); END IF;\n\
                 EXECUTE format('GRANT USAGE ON SEQUENCE %s TO %I', pg_get_serial_sequence('a', 'b'), r);\n\
               END LOOP;\n\
             END;\n\
             $$ LANGUAGE plpgsql;\n",
        );
        // A query loop variable or a function call as an argument gives no grant.
        assert_eq!(
            grant_keys(&facts),
            vec![
                "SELECT orders api".to_owned(),
                "SELECT orders clock".to_owned(),
                "TRUNCATE stock worker".to_owned(),
            ]
        );
    }

    #[test]
    fn from_inside_expressions_is_not_a_table() {
        let facts = facts(
            "db/2.sql",
            "SELECT EXTRACT(year FROM created_at), a IS DISTINCT FROM b FROM orders;\n",
        );
        assert_eq!(
            keys(&facts, FactKind::TableAccess),
            vec!["orders".to_owned()]
        );
    }
}
