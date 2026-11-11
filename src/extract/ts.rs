//! TypeScript and JavaScript extractor (`SPEC.md`, "Extraction", "TypeScript").

use super::ExtractCtx;
use crate::config::{ApiClientPattern, PatternLang};
use crate::error::Result;
use crate::lex::{Lexed, TokKind};
use crate::model::{
    ApiCallData, CallData, Confidence, EnvVarData, Fact, FactData, FnDefData, ImportData,
    ModuleData, SqlInCodeData, TableAccessData, TypeDefData, TypeKind, UnparsedData, norm_path,
    norm_path_in,
};
use crate::source::SourceFile;
use crate::util::fnv;
use std::collections::BTreeSet;

/// Facts of a TypeScript or JavaScript file (everything except the `file` fact and lexer
/// errors).
pub fn extract_ts(file: &SourceFile, lexed: &Lexed, ctx: &ExtractCtx<'_>) -> Result<Vec<Fact>> {
    let mut parser = Parser::new(file, lexed, ctx);
    parser.run()?;
    Ok(parser.facts)
}

const KEYWORD_CALLEES: [&str; 13] = [
    "if", "for", "while", "switch", "catch", "function", "return", "typeof", "await", "yield",
    "super", "import", "new",
];

#[derive(Clone, Debug)]
struct PendingFn {
    name: String,
    key: String,
    owner_type: Option<String>,
    is_async: bool,
    exported: bool,
    use_export_list: bool,
    line: u32,
    start: usize,
}

#[derive(Clone, Debug)]
struct DoneFn {
    pending: PendingFn,
    end_line: u32,
    hash: String,
}

#[derive(Clone, Debug)]
struct PendingType {
    name: String,
    key: String,
    kind: TypeKind,
    line: u32,
    start: usize,
}

#[derive(Clone, Debug)]
enum Scope {
    Module,
    Class {
        class_name: String,
        pending_type: Option<PendingType>,
        expect_member: bool,
    },
    Function {
        pending: Option<PendingFn>,
        expr_end: Option<usize>,
    },
    Object {
        expect_key: bool,
    },
    Block,
    MethodBody,
}

#[derive(Clone, Debug)]
struct Continuation {
    resume_at: usize,
    stmt_start: usize,
    exported: bool,
}

#[derive(Clone, Debug)]
struct CallInfo {
    callee: String,
    name: String,
    segments: Vec<String>,
    is_method: bool,
    receiver: Option<String>,
    line: u32,
    owner: String,
    paren: usize,
}

enum BodyAfter {
    Brace(usize),
    Signature(usize),
    None,
}

struct Parser<'a> {
    file: &'a SourceFile,
    text: &'a str,
    toks: &'a [crate::lex::Token],
    matching: Vec<Option<usize>>,
    api_clients: &'a [ApiClientPattern],
    facts: Vec<Fact>,
    done_fns: Vec<DoneFn>,
    scopes: Vec<Scope>,
    exported: BTreeSet<String>,
    continuations: Vec<Continuation>,
    module_key: String,
    is_ts: bool,
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(file: &'a SourceFile, lexed: &'a Lexed, ctx: &'a ExtractCtx<'a>) -> Parser<'a> {
        Parser {
            file,
            text: &file.text,
            toks: &lexed.tokens,
            matching: compute_matching(&lexed.tokens, &file.text),
            api_clients: &ctx.config.api_clients,
            facts: Vec::new(),
            done_fns: Vec::new(),
            scopes: vec![Scope::Module],
            exported: BTreeSet::new(),
            continuations: Vec::new(),
            module_key: file.path.as_str().to_owned(),
            is_ts: matches!(file.lang, crate::model::Lang::TypeScript),
            i: 0,
        }
    }

    fn run(&mut self) -> Result<()> {
        let module_name = module_name(self.file.path.file_name());
        self.push(
            0,
            self.module_key.clone(),
            Confidence::Exact,
            &ModuleData { name: module_name },
        )?;
        while self.i < self.toks.len() {
            self.finish_expr_scopes()?;
            if self.handle_continuation()? {
                continue;
            }
            let Some(tok) = self.toks.get(self.i) else {
                break;
            };
            let line = tok.line;
            let text = tok.text(self.text).to_owned();
            let kind = tok.kind;
            if text == "}" {
                if self.scopes.len() > 1 {
                    self.pop_scope(self.i)?;
                }
                self.i = self.i.saturating_add(1);
                continue;
            }
            // A class field without `;` ends at a line break that does not continue it.
            if self.in_class() && !self.in_class_expect() && self.ends_at_newline(self.i) {
                self.set_class_expect(true);
            }
            if self.in_class_expect() {
                if self.parse_class_member()? {
                    continue;
                }
            } else if self.in_class() && text == ";" {
                self.set_class_expect(true);
                self.i = self.i.saturating_add(1);
                continue;
            }
            if self.in_object_expect() {
                if self.parse_object_member()? {
                    continue;
                }
            } else if self.in_object() && text == "," {
                self.set_object_expect(true);
                self.i = self.i.saturating_add(1);
                continue;
            }
            if kind == TokKind::Str || kind == TokKind::Template {
                self.check_sql(self.i)?;
                self.i = self.i.saturating_add(1);
                continue;
            }
            if kind == TokKind::Ident && self.dispatch_ident(&text, line)? {
                continue;
            }
            if text == "=>" {
                self.start_anonymous_arrow()?;
                continue;
            }
            if text == "{" {
                self.push_plain_brace();
                self.i = self.i.saturating_add(1);
                continue;
            }
            self.i = self.i.saturating_add(1);
        }
        while self.scopes.len() > 1 {
            let last = self.toks.len().saturating_sub(1);
            self.pop_scope(last)?;
        }
        self.emit_done_fns()?;
        Ok(())
    }

    // ----- small helpers -----

    fn txt(&self, idx: usize) -> &str {
        self.toks
            .get(idx)
            .map(|tok| tok.text(self.text))
            .unwrap_or("")
    }

    fn kind(&self, idx: usize) -> Option<TokKind> {
        self.toks.get(idx).map(|tok| tok.kind)
    }

    fn line(&self, idx: usize) -> u32 {
        self.toks.get(idx).map(|tok| tok.line).unwrap_or(0)
    }

    fn is_ident(&self, idx: usize, want: &str) -> bool {
        self.kind(idx) == Some(TokKind::Ident) && self.txt(idx) == want
    }

    fn match_at(&self, idx: usize) -> Option<usize> {
        self.matching.get(idx).copied().flatten()
    }

    fn push<D: FactData>(
        &mut self,
        line: u32,
        key: String,
        confidence: Confidence,
        data: &D,
    ) -> Result<()> {
        let fact = Fact::new(&self.file.path, line, key, confidence, data)?;
        self.facts.push(fact);
        Ok(())
    }

    fn unparsed(&mut self, idx: usize, reason: &str) -> Result<()> {
        self.push(
            self.line(idx),
            self.file.path.as_str().to_owned(),
            Confidence::Exact,
            &UnparsedData {
                reason: reason.to_owned(),
            },
        )
    }

    fn hash_tokens(&self, start: usize, end: usize) -> String {
        if start > end || end >= self.toks.len() {
            return fnv::hex(b"");
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut idx = start;
        while idx <= end {
            parts.push(self.txt(idx));
            idx = idx.saturating_add(1);
        }
        fnv::hex(parts.join(" ").as_bytes())
    }

    fn current_owner(&self) -> String {
        for scope in self.scopes.iter().rev() {
            if let Scope::Function {
                pending: Some(pending),
                ..
            } = scope
            {
                return pending.key.clone();
            }
        }
        self.module_key.clone()
    }

    fn in_class(&self) -> bool {
        matches!(self.scopes.last(), Some(Scope::Class { .. }))
    }

    fn in_class_expect(&self) -> bool {
        matches!(
            self.scopes.last(),
            Some(Scope::Class {
                expect_member: true,
                ..
            })
        )
    }

    fn in_object(&self) -> bool {
        matches!(self.scopes.last(), Some(Scope::Object { .. }))
    }

    fn in_object_expect(&self) -> bool {
        matches!(
            self.scopes.last(),
            Some(Scope::Object {
                expect_key: true,
                ..
            })
        )
    }

    fn set_class_expect(&mut self, value: bool) {
        if let Some(Scope::Class { expect_member, .. }) = self.scopes.last_mut() {
            *expect_member = value;
        }
    }

    fn set_object_expect(&mut self, value: bool) {
        if let Some(Scope::Object { expect_key, .. }) = self.scopes.last_mut() {
            *expect_key = value;
        }
    }

    /// Automatic semicolon insertion, approximated: a line break before `idx` ends the
    /// statement unless the token before it or the token at `idx` continues an expression.
    fn ends_at_newline(&self, idx: usize) -> bool {
        let Some(prev) = idx.checked_sub(1) else {
            return false;
        };
        if idx >= self.toks.len() || self.line(idx) <= self.line(prev) {
            return false;
        }
        let continues_after = match self.kind(prev) {
            Some(TokKind::Punct) => !matches!(self.txt(prev), ")" | "]" | "}" | "++" | "--"),
            Some(TokKind::Ident) => matches!(
                self.txt(prev),
                "new"
                    | "typeof"
                    | "await"
                    | "void"
                    | "delete"
                    | "instanceof"
                    | "in"
                    | "of"
                    | "as"
                    | "satisfies"
                    | "keyof"
                    | "extends"
                    | "yield"
                    | "async"
            ),
            _ => false,
        };
        if continues_after {
            return false;
        }
        match self.kind(idx) {
            Some(TokKind::Punct) => false,
            Some(TokKind::Ident) => !matches!(
                self.txt(idx),
                "instanceof" | "in" | "of" | "as" | "satisfies" | "extends"
            ),
            Some(_) => true,
            None => false,
        }
    }

    fn skip_recovery(&self, start: usize) -> usize {
        let mut depth: usize = 0;
        let mut idx = start;
        while idx < self.toks.len() {
            let text = self.txt(idx);
            if depth == 0 && text == ";" {
                return idx.saturating_add(1);
            }
            if depth == 0 && idx > start && self.ends_at_newline(idx) {
                return idx;
            }
            if text == "(" || text == "[" || text == "{" {
                depth = depth.saturating_add(1);
            } else if text == ")" || text == "]" || text == "}" {
                if depth == 0 {
                    return idx;
                }
                depth = depth.saturating_sub(1);
            }
            idx = idx.saturating_add(1);
        }
        self.toks.len()
    }

    fn extent_end(&self, start: usize) -> usize {
        let mut depth: usize = 0;
        let mut idx = start;
        while idx < self.toks.len() {
            let text = self.txt(idx);
            if depth == 0 && (text == "," || text == ";") {
                break;
            }
            if depth == 0 && idx > start && self.ends_at_newline(idx) {
                break;
            }
            if text == "(" || text == "[" || text == "{" {
                depth = depth.saturating_add(1);
            } else if text == ")" || text == "]" || text == "}" {
                if depth == 0 {
                    break;
                }
                depth = depth.saturating_sub(1);
            }
            idx = idx.saturating_add(1);
        }
        idx
    }

    /// End of generic arguments starting at the `<` at `start`. Bracketed groups inside
    /// (`<() => void>`, `<T extends { id: string }>`) are skipped whole; a token that cannot
    /// occur in a type, or a scan longer than 256 tokens, means the `<` is a comparison.
    fn skip_angles(&self, start: usize) -> Option<usize> {
        if !self.is_ts || self.txt(start) != "<" {
            return None;
        }
        let mut depth: usize = 0;
        let mut idx = start;
        let limit = start.saturating_add(256);
        while idx < self.toks.len() && idx < limit {
            match self.txt(idx) {
                ";" | ")" | "]" | "}" | "&&" | "||" | "==" | "===" | "!=" | "!==" => return None,
                "(" | "[" | "{" => idx = self.match_at(idx)?,
                "<" => depth = depth.saturating_add(1),
                ">" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(idx.saturating_add(1));
                    }
                }
                _ => {}
            }
            idx = idx.saturating_add(1);
        }
        None
    }

    /// What follows the parameter list closed just before `start`. `Signature` carries the
    /// index where parsing resumes.
    fn body_after_params(&self, start: usize) -> BodyAfter {
        match self.txt(start) {
            "{" => BodyAfter::Brace(start),
            ";" => BodyAfter::Signature(start.saturating_add(1)),
            ":" => self.body_after_type(start.saturating_add(1)),
            _ => BodyAfter::None,
        }
    }

    /// The body after a return type starting at `start`. Bracketed groups are skipped whole,
    /// so the scan ends at the first `;`, statement-ending line break or closing bracket of
    /// the enclosing block.
    fn body_after_type(&self, start: usize) -> BodyAfter {
        let mut angle: usize = 0;
        let mut seen = false;
        let mut idx = start;
        while idx < self.toks.len() {
            let text = self.txt(idx);
            if angle == 0 && seen {
                // An object type (`Promise<{ a: string }>`, `A | { b: string }`) is not the
                // body: the body brace follows a complete type, never an operator or an open `<`.
                let after_operator = matches!(
                    self.txt(idx.saturating_sub(1)),
                    "<" | "," | "|" | "&" | ":" | "=>"
                );
                if text == "{" && !after_operator {
                    return BodyAfter::Brace(idx);
                }
                if self.ends_at_newline(idx) {
                    return BodyAfter::Signature(idx);
                }
            }
            match text {
                ";" => return BodyAfter::Signature(idx.saturating_add(1)),
                ")" | "]" | "}" if seen && angle == 0 => return BodyAfter::Signature(idx),
                ")" | "]" | "}" => return BodyAfter::None,
                "(" | "[" | "{" => match self.match_at(idx) {
                    Some(close) => idx = close,
                    None => return BodyAfter::None,
                },
                "<" => angle = angle.saturating_add(1),
                ">" => angle = angle.saturating_sub(1),
                _ => {}
            }
            seen = true;
            idx = idx.saturating_add(1);
        }
        BodyAfter::None
    }

    /// The `=>` after an arrow function's return type starting at `start`. Bracketed groups
    /// are skipped whole; `;`, `,`, a closing bracket or a statement-ending line break means
    /// there is no arrow.
    fn skip_type_until_arrow(&self, start: usize) -> Option<usize> {
        let mut idx = start;
        while idx < self.toks.len() {
            if idx > start && self.ends_at_newline(idx) {
                return None;
            }
            match self.txt(idx) {
                "=>" => return Some(idx),
                ";" | "," | ")" | "]" | "}" => return None,
                "(" | "[" | "{" => idx = self.match_at(idx)?,
                _ => {}
            }
            idx = idx.saturating_add(1);
        }
        None
    }

    /// The body brace of a class, interface or enum header starting at `start`. Bracketed
    /// groups are skipped whole; a `;`, a closing bracket or more than 256 tokens outside
    /// groups means there is no body.
    fn find_clause_brace(&self, start: usize) -> Option<usize> {
        let mut angle: usize = 0;
        let mut idx = start;
        for _ in 0..256 {
            if idx >= self.toks.len() {
                return None;
            }
            match self.txt(idx) {
                "{" if angle == 0 => return Some(idx),
                ";" | ")" | "]" | "}" => return None,
                "(" | "[" | "{" => idx = self.match_at(idx)?,
                "<" => angle = angle.saturating_add(1),
                ">" => angle = angle.saturating_sub(1),
                _ => {}
            }
            idx = idx.saturating_add(1);
        }
        None
    }

    // ----- scopes -----

    fn finish_expr_scopes(&mut self) -> Result<()> {
        loop {
            let end = match self.scopes.last() {
                Some(Scope::Function {
                    expr_end: Some(end),
                    ..
                }) if *end <= self.i => *end,
                _ => break,
            };
            let end_idx = end.saturating_sub(1);
            self.pop_scope(end_idx)?;
        }
        Ok(())
    }

    fn pop_scope(&mut self, end_idx: usize) -> Result<()> {
        let Some(scope) = self.scopes.pop() else {
            return Ok(());
        };
        let was_method_body = matches!(scope, Scope::MethodBody);
        match scope {
            Scope::Function {
                pending: Some(pending),
                ..
            } => {
                let end_line = self.line(end_idx);
                let hash = self.hash_tokens(pending.start, end_idx);
                self.done_fns.push(DoneFn {
                    pending,
                    end_line,
                    hash,
                });
            }
            Scope::Class {
                pending_type: Some(pending),
                ..
            } => {
                let end_line = self.line(end_idx);
                let hash = self.hash_tokens(pending.start, end_idx);
                self.push(
                    pending.line,
                    pending.key.clone(),
                    Confidence::Exact,
                    &TypeDefData {
                        name: pending.name.clone(),
                        type_kind: pending.kind,
                        end_line,
                        hash,
                    },
                )?;
            }
            _ => {}
        }
        match self.scopes.last_mut() {
            Some(Scope::Class { expect_member, .. }) => *expect_member = true,
            Some(Scope::Object { expect_key, .. }) if was_method_body => *expect_key = true,
            _ => {}
        }
        Ok(())
    }

    fn emit_done_fns(&mut self) -> Result<()> {
        let done = std::mem::take(&mut self.done_fns);
        for item in &done {
            let pending = &item.pending;
            let is_pub = pending.exported
                || (pending.use_export_list && self.exported.contains(&pending.name));
            self.push(
                pending.line,
                pending.key.clone(),
                Confidence::Exact,
                &FnDefData {
                    name: pending.name.clone(),
                    owner_type: pending.owner_type.clone(),
                    trait_name: None,
                    is_pub,
                    is_async: pending.is_async,
                    end_line: item.end_line,
                    hash: item.hash.clone(),
                },
            )?;
        }
        Ok(())
    }

    fn push_plain_brace(&mut self) {
        let prev = if self.i == 0 {
            ""
        } else {
            self.txt(self.i.saturating_sub(1))
        };
        let object_prev = matches!(
            prev,
            "=" | "("
                | ","
                | ":"
                | "["
                | "=>"
                | "?"
                | "&&"
                | "||"
                | "!"
                | "..."
                | "return"
                | "case"
        ) || (self.in_object() && prev == "{");
        // `case X: {` and `default: {` open a block, not an object literal.
        let case_block =
            prev == ":" && !self.in_object() && self.is_case_label(self.i.saturating_sub(1));
        if object_prev && !case_block {
            self.scopes.push(Scope::Object { expect_key: true });
        } else {
            self.scopes.push(Scope::Block);
        }
    }

    /// Whether the `:` at `colon` ends a `case ...:` or `default:` label. The scan goes back
    /// within the statement and stops at another `:` so a ternary inside a case is not a label.
    fn is_case_label(&self, colon: usize) -> bool {
        let mut depth: usize = 0;
        let mut idx = colon;
        let floor = colon.saturating_sub(256);
        while let Some(prev) = idx.checked_sub(1) {
            if prev < floor {
                return false;
            }
            idx = prev;
            match self.txt(idx) {
                ")" | "]" => depth = depth.saturating_add(1),
                "(" | "[" => depth = depth.saturating_sub(1),
                ";" | "{" | "}" | ":" if depth == 0 => return false,
                "case" | "default" if depth == 0 && self.kind(idx) == Some(TokKind::Ident) => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    // ----- continuations for multi-declarator variable statements -----

    fn handle_continuation(&mut self) -> Result<bool> {
        let Some(cont) = self.continuations.last().cloned() else {
            return Ok(false);
        };
        if cont.resume_at < self.i {
            self.continuations.pop();
            return Ok(true);
        }
        if cont.resume_at != self.i {
            return Ok(false);
        }
        self.continuations.pop();
        if self.txt(self.i) == "," {
            let next = self.i.saturating_add(1);
            self.parse_declarator(next, cont.stmt_start, cont.exported)?;
            return Ok(true);
        }
        Ok(false)
    }

    // ----- identifier dispatch -----

    fn dispatch_ident(&mut self, text: &str, _line: u32) -> Result<bool> {
        match text {
            "import" => {
                if self.txt(self.i.saturating_add(1)) == "." {
                    if self.try_env_var()? {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                if self.txt(self.i.saturating_add(1)) == "(" {
                    return Ok(false);
                }
                self.parse_import(self.i)?;
                Ok(true)
            }
            "export" => {
                self.parse_export(self.i)?;
                Ok(true)
            }
            "function" => {
                self.parse_function_decl(self.i, self.i, false, false)?;
                Ok(true)
            }
            "async" if self.is_ident(self.i.saturating_add(1), "function") => {
                self.parse_function_decl(self.i, self.i, false, false)?;
                Ok(true)
            }
            "declare" => {
                let next = self.i.saturating_add(1);
                match self.txt(next) {
                    "function" => {
                        self.parse_function_decl(self.i, next, false, false)?;
                        Ok(true)
                    }
                    "class" => {
                        self.parse_class(self.i, next, false, false)?;
                        Ok(true)
                    }
                    "interface" => {
                        self.parse_interface(self.i, next)?;
                        Ok(true)
                    }
                    "type" => {
                        self.parse_type_alias(self.i, next)?;
                        Ok(true)
                    }
                    "enum" => {
                        self.parse_enum(self.i, next)?;
                        Ok(true)
                    }
                    "abstract" => {
                        self.parse_class(self.i, next.saturating_add(1), false, false)?;
                        Ok(true)
                    }
                    "const" | "let" | "var" => {
                        self.parse_var(self.i, next, false)?;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            "abstract" if self.is_ident(self.i.saturating_add(1), "class") => {
                self.parse_class(self.i, self.i.saturating_add(1), false, false)?;
                Ok(true)
            }
            // `class="x"` is a JSX attribute and `a.class` a property, not a declaration.
            "class"
                if self.txt(self.i.saturating_add(1)) != "="
                    && !matches!(self.txt(self.i.wrapping_sub(1)), "." | "?.") =>
            {
                self.parse_class(self.i, self.i, false, false)?;
                Ok(true)
            }
            "interface" if self.kind(self.i.saturating_add(1)) == Some(TokKind::Ident) => {
                self.parse_interface(self.i, self.i)?;
                Ok(true)
            }
            "type" => {
                let name_idx = self.i.saturating_add(1);
                if self.kind(name_idx) == Some(TokKind::Ident)
                    && (self.txt(name_idx.saturating_add(1)) == "="
                        || self.txt(name_idx.saturating_add(1)) == "<")
                {
                    self.parse_type_alias(self.i, self.i)?;
                    return Ok(true);
                }
                Ok(false)
            }
            "enum" if self.kind(self.i.saturating_add(1)) == Some(TokKind::Ident) => {
                self.parse_enum(self.i, self.i)?;
                Ok(true)
            }
            "const" | "let" | "var" => {
                let next = self.i.saturating_add(1);
                if self.kind(next) == Some(TokKind::Ident)
                    || self.txt(next) == "{"
                    || self.txt(next) == "["
                {
                    self.parse_var(self.i, self.i, false)?;
                    return Ok(true);
                }
                Ok(false)
            }
            "process" => {
                if self.try_env_var()? {
                    return Ok(true);
                }
                self.try_call()
            }
            _ => self.try_call(),
        }
    }

    // ----- imports and exports -----

    fn push_import(
        &mut self,
        line: u32,
        spec: &str,
        name: &str,
        alias: Option<String>,
    ) -> Result<()> {
        let key = format!("{spec}#{name}");
        self.push(
            line,
            key,
            Confidence::Exact,
            &ImportData {
                owner: self.module_key.clone(),
                path: spec.to_owned(),
                name: name.to_owned(),
                alias,
            },
        )
    }

    fn parse_import(&mut self, start: usize) -> Result<()> {
        let line = self.line(start);
        let mut idx = start.saturating_add(1);
        if self.kind(idx) == Some(TokKind::Str) {
            match crate::lex::ts::unquote(self.txt(idx)) {
                Some(spec) => self.push_import(line, &spec, "*", None)?,
                None => self.unparsed(start, "cannot read import statement")?,
            }
            self.i = idx.saturating_add(1);
            return Ok(());
        }
        if self.is_ident(idx, "type") {
            let after = idx.saturating_add(1);
            if !self.is_ident(after, "from") && self.txt(after) != "," {
                idx = after;
            }
        }
        let mut bindings: Vec<(String, Option<String>)> = Vec::new();
        let mut ok = true;
        loop {
            match self.kind(idx) {
                Some(TokKind::Ident) if !self.is_ident(idx, "from") => {
                    let name = self.txt(idx).to_owned();
                    idx = idx.saturating_add(1);
                    let mut alias = None;
                    if self.is_ident(idx, "as") {
                        idx = idx.saturating_add(1);
                        if self.kind(idx) == Some(TokKind::Ident) {
                            alias = Some(self.txt(idx).to_owned());
                            idx = idx.saturating_add(1);
                        }
                    }
                    // A default binding is reported with name `default`.
                    if bindings.is_empty() && self.txt(idx) != "{" && self.txt(idx) != "*" {
                        bindings.push(("default".to_owned(), Some(name)));
                    } else {
                        bindings.push((name, alias));
                    }
                    if self.txt(idx) == "," {
                        idx = idx.saturating_add(1);
                    }
                }
                _ if self.txt(idx) == "*" => {
                    idx = idx.saturating_add(1);
                    let mut alias = None;
                    if self.is_ident(idx, "as") {
                        idx = idx.saturating_add(1);
                        if self.kind(idx) == Some(TokKind::Ident) {
                            alias = Some(self.txt(idx).to_owned());
                            idx = idx.saturating_add(1);
                        }
                    }
                    bindings.push(("*".to_owned(), alias));
                    if self.txt(idx) == "," {
                        idx = idx.saturating_add(1);
                    }
                }
                _ if self.txt(idx) == "{" => {
                    match self.match_at(idx) {
                        Some(close) => {
                            let mut inner = idx.saturating_add(1);
                            while inner < close {
                                if self.is_ident(inner, "type")
                                    && self.kind(inner.saturating_add(1)) == Some(TokKind::Ident)
                                    && !self.is_ident(inner.saturating_add(1), "as")
                                {
                                    inner = inner.saturating_add(1);
                                }
                                if self.kind(inner) == Some(TokKind::Ident) {
                                    let name = self.txt(inner).to_owned();
                                    inner = inner.saturating_add(1);
                                    let mut alias = None;
                                    if self.is_ident(inner, "as") {
                                        inner = inner.saturating_add(1);
                                        if self.kind(inner) == Some(TokKind::Ident) {
                                            alias = Some(self.txt(inner).to_owned());
                                            inner = inner.saturating_add(1);
                                        }
                                    }
                                    bindings.push((name, alias));
                                } else {
                                    inner = inner.saturating_add(1);
                                }
                            }
                            idx = close.saturating_add(1);
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                    if self.txt(idx) == "," {
                        idx = idx.saturating_add(1);
                    }
                }
                _ => break,
            }
            if self.is_ident(idx, "from") || self.kind(idx) == Some(TokKind::Str) {
                break;
            }
            if idx >= self.toks.len() {
                break;
            }
        }
        if ok && self.is_ident(idx, "from") {
            idx = idx.saturating_add(1);
        }
        if !ok || self.kind(idx) != Some(TokKind::Str) {
            self.unparsed(start, "cannot read import statement")?;
            self.i = self.skip_recovery(start);
            return Ok(());
        }
        match crate::lex::ts::unquote(self.txt(idx)) {
            Some(spec) => {
                if bindings.is_empty() {
                    self.push_import(line, &spec, "*", None)?;
                }
                for (name, alias) in bindings {
                    // Default bindings were stored as name `default` with the local name as alias.
                    self.push_import(line, &spec, &name, alias)?;
                }
            }
            None => self.unparsed(start, "cannot read import statement")?,
        }
        self.i = idx.saturating_add(1);
        Ok(())
    }

    fn parse_export(&mut self, start: usize) -> Result<()> {
        let line = self.line(start);
        let mut idx = start.saturating_add(1);
        if self.is_ident(idx, "type") {
            let after = idx.saturating_add(1);
            if self.txt(after) == "{" || self.txt(after) == "*" {
                idx = after;
                return self.parse_export_list(start, idx, line);
            }
        }
        if self.txt(idx) == "{" || self.txt(idx) == "*" {
            return self.parse_export_list(start, idx, line);
        }
        if self.is_ident(idx, "default") {
            let after = idx.saturating_add(1);
            match self.txt(after) {
                "function" => return self.parse_function_decl(start, after, true, true),
                "class" => return self.parse_class(start, after, true, true),
                "async" if self.is_ident(after.saturating_add(1), "function") => {
                    return self.parse_function_decl(start, after, true, true);
                }
                "abstract" if self.is_ident(after.saturating_add(1), "class") => {
                    return self.parse_class(start, after.saturating_add(1), true, true);
                }
                _ if self.arrow_body(after, self.toks.len()).is_some() => {
                    return self.parse_default_arrow(start, after);
                }
                _ => {
                    self.i = after;
                    return Ok(());
                }
            }
        }
        if self.is_ident(idx, "declare") {
            idx = idx.saturating_add(1);
        }
        match self.txt(idx) {
            "function" => self.parse_function_decl(start, idx, true, false),
            "async" if self.is_ident(idx.saturating_add(1), "function") => {
                self.parse_function_decl(start, idx, true, false)
            }
            "class" => self.parse_class(start, idx, true, false),
            "abstract" if self.is_ident(idx.saturating_add(1), "class") => {
                self.parse_class(start, idx.saturating_add(1), true, false)
            }
            "interface" => self.parse_interface(start, idx),
            "type" => self.parse_type_alias(start, idx),
            "enum" => self.parse_enum(start, idx),
            "const" | "let" | "var" => self.parse_var(start, idx, true),
            _ => {
                self.i = idx;
                Ok(())
            }
        }
    }

    fn parse_export_list(&mut self, start: usize, idx: usize, line: u32) -> Result<()> {
        let mut entries: Vec<(String, Option<String>)> = Vec::new();
        let mut idx = idx;
        let mut star_alias: Option<String> = None;
        let mut is_star = false;
        if self.txt(idx) == "*" {
            is_star = true;
            idx = idx.saturating_add(1);
            if self.is_ident(idx, "as") {
                idx = idx.saturating_add(1);
                if self.kind(idx) == Some(TokKind::Ident) {
                    star_alias = Some(self.txt(idx).to_owned());
                    idx = idx.saturating_add(1);
                }
            }
        } else if self.txt(idx) == "{" {
            let Some(close) = self.match_at(idx) else {
                self.unparsed(start, "cannot read export statement")?;
                self.i = self.skip_recovery(start);
                return Ok(());
            };
            let mut inner = idx.saturating_add(1);
            while inner < close {
                if self.is_ident(inner, "type")
                    && self.kind(inner.saturating_add(1)) == Some(TokKind::Ident)
                    && !self.is_ident(inner.saturating_add(1), "as")
                {
                    inner = inner.saturating_add(1);
                }
                if self.kind(inner) == Some(TokKind::Ident)
                    || self.kind(inner) == Some(TokKind::Str)
                {
                    let name = if self.kind(inner) == Some(TokKind::Str) {
                        crate::lex::ts::unquote(self.txt(inner))
                            .unwrap_or_else(|| self.txt(inner).to_owned())
                    } else {
                        self.txt(inner).to_owned()
                    };
                    inner = inner.saturating_add(1);
                    let mut alias = None;
                    if self.is_ident(inner, "as") {
                        inner = inner.saturating_add(1);
                        if self.kind(inner) == Some(TokKind::Ident) {
                            alias = Some(self.txt(inner).to_owned());
                            inner = inner.saturating_add(1);
                        }
                    }
                    entries.push((name, alias));
                } else {
                    inner = inner.saturating_add(1);
                }
            }
            idx = close.saturating_add(1);
        } else {
            self.unparsed(start, "cannot read export statement")?;
            self.i = self.skip_recovery(start);
            return Ok(());
        }
        if self.is_ident(idx, "from") {
            let spec_idx = idx.saturating_add(1);
            if self.kind(spec_idx) != Some(TokKind::Str) {
                self.unparsed(start, "cannot read export statement")?;
                self.i = self.skip_recovery(start);
                return Ok(());
            }
            match crate::lex::ts::unquote(self.txt(spec_idx)) {
                Some(spec) => {
                    if is_star {
                        self.push_import(line, &spec, "*", star_alias)?;
                    } else if entries.is_empty() {
                        self.push_import(line, &spec, "*", None)?;
                    } else {
                        for (name, alias) in entries {
                            self.push_import(line, &spec, &name, alias)?;
                        }
                    }
                }
                None => self.unparsed(start, "cannot read export statement")?,
            }
            self.i = spec_idx.saturating_add(1);
            return Ok(());
        }
        if is_star {
            self.unparsed(start, "cannot read export statement")?;
            self.i = self.skip_recovery(start);
            return Ok(());
        }
        for (name, alias) in entries {
            self.exported.insert(name);
            if let Some(alias) = alias {
                self.exported.insert(alias);
            }
        }
        self.i = idx;
        Ok(())
    }

    // ----- variable declarations -----

    fn parse_var(&mut self, stmt_start: usize, keyword_idx: usize, exported: bool) -> Result<()> {
        let first = keyword_idx.saturating_add(1);
        if self.is_ident(keyword_idx, "const") && self.is_ident(first, "enum") {
            return self.parse_enum(stmt_start, first);
        }
        self.parse_declarator(first, stmt_start, exported)
    }

    fn parse_declarator(&mut self, start: usize, stmt_start: usize, exported: bool) -> Result<()> {
        let mut binding_name: Option<String> = None;
        let pattern_start = start;
        let pattern_end: usize;
        let mut is_object_pattern = false;
        match self.kind(start) {
            Some(TokKind::Ident) => {
                binding_name = Some(self.txt(start).to_owned());
                pattern_end = start.saturating_add(1);
            }
            _ if self.txt(start) == "{" || self.txt(start) == "[" => {
                is_object_pattern = self.txt(start) == "{";
                match self.match_at(start) {
                    Some(close) => pattern_end = close.saturating_add(1),
                    None => {
                        self.unparsed(stmt_start, "cannot read variable declaration")?;
                        self.i = self.skip_recovery(stmt_start);
                        return Ok(());
                    }
                }
            }
            _ => {
                self.unparsed(stmt_start, "cannot read variable declaration")?;
                self.i = self.skip_recovery(stmt_start);
                return Ok(());
            }
        }
        let mut idx = pattern_end;
        if self.txt(idx) == ":" {
            idx = self.skip_annotation(idx.saturating_add(1));
        }
        if self.txt(idx) != "=" {
            if self.txt(idx) == "," {
                self.continuations.push(Continuation {
                    resume_at: idx,
                    stmt_start,
                    exported,
                });
            }
            self.i = idx;
            return Ok(());
        }
        let init_start = idx.saturating_add(1);
        let decl_end = self.extent_end(init_start);
        if decl_end <= init_start {
            self.unparsed(stmt_start, "cannot read variable declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        // `require('m')` as the whole initializer is an import, not a call.
        if let Some(spec) = self.require_spec(init_start, decl_end) {
            let line = self.line(stmt_start);
            if is_object_pattern {
                for (name, alias) in self.object_pattern_entries(pattern_start, pattern_end) {
                    self.push_import(line, &spec, &name, alias)?;
                }
            } else if let Some(name) = &binding_name {
                self.push_import(line, &spec, "*", Some(name.clone()))?;
            } else {
                self.push_import(line, &spec, "*", None)?;
            }
            self.continuations.push(Continuation {
                resume_at: decl_end,
                stmt_start,
                exported,
            });
            self.i = decl_end;
            return Ok(());
        }
        if let Some(direct) = self.direct_function(init_start, decl_end) {
            let (is_async, fn_line, body_brace, expr_body) = direct;
            self.continuations.push(Continuation {
                resume_at: decl_end,
                stmt_start,
                exported,
            });
            match (&binding_name, body_brace, expr_body) {
                (Some(name), Some(brace), _) => {
                    let owner = self.current_owner();
                    let key = format!("{owner}::{name}");
                    let line = if fn_line == 0 {
                        self.line(start)
                    } else {
                        fn_line
                    };
                    self.scopes.push(Scope::Function {
                        pending: Some(PendingFn {
                            name: name.clone(),
                            key,
                            owner_type: None,
                            is_async,
                            exported,
                            use_export_list: true,
                            line,
                            start: stmt_start,
                        }),
                        expr_end: None,
                    });
                    self.i = brace.saturating_add(1);
                }
                (Some(name), None, Some(body)) => {
                    let owner = self.current_owner();
                    let key = format!("{owner}::{name}");
                    self.scopes.push(Scope::Function {
                        pending: Some(PendingFn {
                            name: name.clone(),
                            key,
                            owner_type: None,
                            is_async,
                            exported,
                            use_export_list: true,
                            line: self.line(start),
                            start: stmt_start,
                        }),
                        expr_end: Some(decl_end),
                    });
                    self.i = body;
                }
                (_, Some(brace), _) => {
                    self.scopes.push(Scope::Function {
                        pending: None,
                        expr_end: None,
                    });
                    self.i = brace.saturating_add(1);
                }
                (_, None, Some(body)) => {
                    self.scopes.push(Scope::Function {
                        pending: None,
                        expr_end: Some(decl_end),
                    });
                    self.i = body;
                }
                _ => {
                    self.i = init_start;
                }
            }
            return Ok(());
        }
        self.continuations.push(Continuation {
            resume_at: decl_end,
            stmt_start,
            exported,
        });
        self.i = init_start;
        Ok(())
    }

    /// End of a variable's type annotation starting at `start`. Bracketed groups are skipped
    /// whole; the annotation ends at `=`, `,`, `of`, `in`, `;`, a closing bracket or a
    /// statement-ending line break.
    fn skip_annotation(&self, start: usize) -> usize {
        let mut angle: usize = 0;
        let mut idx = start;
        while idx < self.toks.len() {
            let text = self.txt(idx);
            if matches!(text, ";" | ")" | "]" | "}") {
                break;
            }
            if angle == 0 {
                if text == "=" || text == "," {
                    break;
                }
                if (text == "of" || text == "in") && self.kind(idx) == Some(TokKind::Ident) {
                    break;
                }
                if idx > start && self.ends_at_newline(idx) {
                    break;
                }
            }
            if text == "<"
                && (self.kind(idx.saturating_sub(1)) == Some(TokKind::Ident)
                    || self.txt(idx.saturating_sub(1)) == ">")
            {
                angle = angle.saturating_add(1);
            } else if text == ">" && angle > 0 {
                angle = angle.saturating_sub(1);
            } else if matches!(text, "(" | "[" | "{") {
                match self.match_at(idx) {
                    Some(close) => idx = close,
                    None => return self.toks.len(),
                }
            }
            idx = idx.saturating_add(1);
        }
        idx
    }

    fn require_spec(&self, init_start: usize, decl_end: usize) -> Option<String> {
        if !self.is_ident(init_start, "require") {
            return None;
        }
        let open = init_start.saturating_add(1);
        if self.txt(open) != "(" {
            return None;
        }
        let close = self.match_at(open)?;
        if close.saturating_add(1) != decl_end {
            return None;
        }
        let arg = open.saturating_add(1);
        if arg >= close || self.kind(arg) != Some(TokKind::Str) {
            return None;
        }
        if arg.saturating_add(1) != close {
            return None;
        }
        crate::lex::ts::unquote(self.txt(arg))
    }

    fn object_pattern_entries(&self, start: usize, end: usize) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        let inner_end = end.saturating_sub(1);
        let mut idx = start.saturating_add(1);
        while idx < inner_end {
            let entry_start = idx;
            let entry_end = {
                let mut depth: usize = 0;
                let mut j = idx;
                while j < inner_end {
                    let text = self.txt(j);
                    if depth == 0 && text == "," {
                        break;
                    }
                    if text == "(" || text == "[" || text == "{" {
                        depth = depth.saturating_add(1);
                    } else if text == ")" || text == "]" || text == "}" {
                        if depth == 0 {
                            break;
                        }
                        depth = depth.saturating_sub(1);
                    }
                    j = j.saturating_add(1);
                }
                j
            };
            let mut first: Option<String> = None;
            let mut after_colon: Option<String> = None;
            let mut colon_seen = false;
            let mut j = entry_start;
            while j < entry_end {
                if self.txt(j) == ":" && !colon_seen {
                    colon_seen = true;
                    j = j.saturating_add(1);
                    continue;
                }
                if self.txt(j) == "=" {
                    break;
                }
                if self.kind(j) == Some(TokKind::Ident) {
                    if colon_seen {
                        if after_colon.is_none() {
                            after_colon = Some(self.txt(j).to_owned());
                        }
                    } else if first.is_none() {
                        first = Some(self.txt(j).to_owned());
                    }
                }
                j = j.saturating_add(1);
            }
            if let Some(name) = first {
                let alias = match &after_colon {
                    Some(local) if *local != name => Some(local.clone()),
                    _ => None,
                };
                out.push((name, alias));
            }
            idx = entry_end.saturating_add(1);
        }
        out
    }

    /// A direct arrow or function expression: (is_async, function-keyword line or 0, brace body, expression body).
    fn direct_function(
        &self,
        start: usize,
        end: usize,
    ) -> Option<(bool, u32, Option<usize>, Option<usize>)> {
        // Arrow form.
        if let Some((is_async, body)) = self.arrow_body(start, end) {
            if self.txt(body) == "{" {
                let close = self.match_at(body)?;
                if close.saturating_add(1) == end {
                    return Some((is_async, 0, Some(body), None));
                }
                return None;
            }
            return Some((is_async, 0, None, Some(body)));
        }
        // Function expression form.
        let mut idx = start;
        let mut is_async = false;
        if self.is_ident(idx, "async") {
            is_async = true;
            idx = idx.saturating_add(1);
        }
        if !self.is_ident(idx, "function") {
            return None;
        }
        let fn_line = self.line(idx);
        idx = idx.saturating_add(1);
        if self.txt(idx) == "*" {
            idx = idx.saturating_add(1);
        }
        if self.kind(idx) == Some(TokKind::Ident) {
            idx = idx.saturating_add(1);
        }
        if self.txt(idx) == "<" {
            idx = self.skip_angles(idx)?;
        }
        if self.txt(idx) != "(" {
            return None;
        }
        let close = self.match_at(idx)?;
        match self.body_after_params(close.saturating_add(1)) {
            BodyAfter::Brace(brace) => {
                let body_close = self.match_at(brace)?;
                if body_close.saturating_add(1) == end {
                    Some((is_async, fn_line, Some(brace), None))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn arrow_body(&self, start: usize, end: usize) -> Option<(bool, usize)> {
        let mut idx = start;
        let mut is_async = false;
        if self.is_ident(idx, "async") {
            let next = idx.saturating_add(1);
            if self.txt(next) == "(" || self.kind(next) == Some(TokKind::Ident) {
                is_async = true;
                idx = next;
            }
        }
        if self.is_ts && self.txt(idx) == "<" {
            idx = self.skip_angles(idx)?;
        }
        if self.txt(idx) == "(" {
            let close = self.match_at(idx)?;
            idx = close.saturating_add(1);
        } else if self.kind(idx) == Some(TokKind::Ident) {
            idx = idx.saturating_add(1);
        } else {
            return None;
        }
        if idx >= end {
            return None;
        }
        if self.txt(idx) == ":" {
            idx = self.skip_type_until_arrow(idx.saturating_add(1))?;
        }
        if self.txt(idx) != "=>" {
            return None;
        }
        let body = idx.saturating_add(1);
        if body >= end {
            return None;
        }
        Some((is_async, body))
    }

    fn parse_default_arrow(&mut self, stmt_start: usize, cursor: usize) -> Result<()> {
        let Some((is_async, body)) = self.arrow_body(cursor, self.toks.len()) else {
            self.i = cursor;
            return Ok(());
        };
        let pending = PendingFn {
            name: "default".to_owned(),
            key: format!("{}::default", self.module_key),
            owner_type: None,
            is_async,
            exported: true,
            use_export_list: false,
            line: self.line(stmt_start),
            start: stmt_start,
        };
        if self.txt(body) == "{" {
            self.scopes.push(Scope::Function {
                pending: Some(pending),
                expr_end: None,
            });
            self.i = body.saturating_add(1);
        } else {
            let end = self.extent_end(body);
            self.scopes.push(Scope::Function {
                pending: Some(pending),
                expr_end: Some(end),
            });
            self.i = body;
        }
        Ok(())
    }

    fn start_anonymous_arrow(&mut self) -> Result<()> {
        let body = self.i.saturating_add(1);
        if self.txt(body) == "{" {
            self.scopes.push(Scope::Function {
                pending: None,
                expr_end: None,
            });
            self.i = body.saturating_add(1);
        } else if body < self.toks.len() {
            let end = self.extent_end(body);
            self.scopes.push(Scope::Function {
                pending: None,
                expr_end: Some(end),
            });
            self.i = body;
        } else {
            self.i = body;
        }
        Ok(())
    }

    // ----- functions, classes, types -----

    fn parse_function_decl(
        &mut self,
        stmt_start: usize,
        cursor: usize,
        exported: bool,
        is_default: bool,
    ) -> Result<()> {
        let mut idx = cursor;
        let mut is_async = false;
        if self.is_ident(idx, "async") {
            is_async = true;
            idx = idx.saturating_add(1);
        }
        if !self.is_ident(idx, "function") {
            self.unparsed(stmt_start, "cannot read function declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let fn_line = self.line(idx);
        idx = idx.saturating_add(1);
        if self.txt(idx) == "*" {
            idx = idx.saturating_add(1);
        }
        let mut name: Option<String> = None;
        if self.kind(idx) == Some(TokKind::Ident) {
            name = Some(self.txt(idx).to_owned());
            idx = idx.saturating_add(1);
        }
        if name.is_none() && is_default {
            name = Some("default".to_owned());
        }
        if self.txt(idx) == "<" {
            match self.skip_angles(idx) {
                Some(after) => idx = after,
                None => {
                    self.unparsed(stmt_start, "cannot read function declaration")?;
                    self.i = self.skip_recovery(stmt_start);
                    return Ok(());
                }
            }
        }
        if self.txt(idx) != "(" {
            self.unparsed(stmt_start, "cannot read function declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let Some(close) = self.match_at(idx) else {
            self.unparsed(stmt_start, "cannot read function declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        };
        match self.body_after_params(close.saturating_add(1)) {
            BodyAfter::Signature(resume) => {
                self.i = resume;
                Ok(())
            }
            BodyAfter::Brace(brace) => {
                let pending = name.map(|name| {
                    let owner = self.current_owner();
                    PendingFn {
                        key: format!("{owner}::{name}"),
                        name,
                        owner_type: None,
                        is_async,
                        exported,
                        use_export_list: true,
                        line: fn_line,
                        start: stmt_start,
                    }
                });
                self.scopes.push(Scope::Function {
                    pending,
                    expr_end: None,
                });
                self.i = brace.saturating_add(1);
                Ok(())
            }
            BodyAfter::None => {
                self.unparsed(stmt_start, "cannot read function declaration")?;
                self.i = self.skip_recovery(stmt_start);
                Ok(())
            }
        }
    }

    fn parse_class(
        &mut self,
        stmt_start: usize,
        cursor: usize,
        _exported: bool,
        is_default: bool,
    ) -> Result<()> {
        let class_idx = cursor;
        let line = self.line(class_idx);
        let mut idx = class_idx.saturating_add(1);
        let mut name: Option<String> = None;
        if self.kind(idx) == Some(TokKind::Ident) && !self.is_ident(idx, "extends") {
            name = Some(self.txt(idx).to_owned());
            idx = idx.saturating_add(1);
        }
        if name.is_none() && is_default {
            name = Some("default".to_owned());
        }
        if self.txt(idx) == "<" {
            if let Some(after) = self.skip_angles(idx) {
                idx = after;
            }
        }
        let Some(brace) = self.find_clause_brace(idx) else {
            self.unparsed(stmt_start, "cannot read class declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        };
        let pending_type = name.as_ref().map(|name| PendingType {
            name: name.clone(),
            key: format!("{}::{name}", self.module_key),
            kind: TypeKind::Class,
            line,
            start: stmt_start,
        });
        self.scopes.push(Scope::Class {
            class_name: name.unwrap_or_default(),
            pending_type,
            expect_member: true,
        });
        self.i = brace.saturating_add(1);
        Ok(())
    }

    fn parse_interface(&mut self, stmt_start: usize, cursor: usize) -> Result<()> {
        let line = self.line(cursor);
        let mut idx = cursor.saturating_add(1);
        if self.kind(idx) != Some(TokKind::Ident) {
            self.unparsed(stmt_start, "cannot read interface declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let name = self.txt(idx).to_owned();
        idx = idx.saturating_add(1);
        if self.txt(idx) == "<" {
            if let Some(after) = self.skip_angles(idx) {
                idx = after;
            }
        }
        let Some(brace) = self.find_clause_brace(idx) else {
            self.unparsed(stmt_start, "cannot read interface declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        };
        let end_idx = self
            .match_at(brace)
            .unwrap_or_else(|| self.toks.len().saturating_sub(1));
        let key = format!("{}::{name}", self.module_key);
        self.push(
            line,
            key,
            Confidence::Exact,
            &TypeDefData {
                name,
                type_kind: TypeKind::Interface,
                end_line: self.line(end_idx),
                hash: self.hash_tokens(stmt_start, end_idx),
            },
        )?;
        self.i = self
            .match_at(brace)
            .map(|close| close.saturating_add(1))
            .unwrap_or(self.toks.len());
        Ok(())
    }

    fn parse_enum(&mut self, stmt_start: usize, cursor: usize) -> Result<()> {
        let line = self.line(cursor);
        let mut idx = cursor.saturating_add(1);
        if self.is_ident(idx, "const") {
            idx = idx.saturating_add(1);
        }
        if self.kind(idx) != Some(TokKind::Ident) {
            self.unparsed(stmt_start, "cannot read enum declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let name = self.txt(idx).to_owned();
        idx = idx.saturating_add(1);
        let Some(brace) = self.find_clause_brace(idx) else {
            self.unparsed(stmt_start, "cannot read enum declaration")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        };
        let end_idx = self
            .match_at(brace)
            .unwrap_or_else(|| self.toks.len().saturating_sub(1));
        let key = format!("{}::{name}", self.module_key);
        self.push(
            line,
            key,
            Confidence::Exact,
            &TypeDefData {
                name,
                type_kind: TypeKind::Enum,
                end_line: self.line(end_idx),
                hash: self.hash_tokens(stmt_start, end_idx),
            },
        )?;
        self.i = self
            .match_at(brace)
            .map(|close| close.saturating_add(1))
            .unwrap_or(self.toks.len());
        Ok(())
    }

    fn parse_type_alias(&mut self, stmt_start: usize, cursor: usize) -> Result<()> {
        let line = self.line(cursor);
        let mut idx = cursor.saturating_add(1);
        if self.kind(idx) != Some(TokKind::Ident) {
            self.unparsed(stmt_start, "cannot read type alias")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let name = self.txt(idx).to_owned();
        idx = idx.saturating_add(1);
        if self.txt(idx) == "<" {
            match self.skip_angles(idx) {
                Some(after) => idx = after,
                None => {
                    self.unparsed(stmt_start, "cannot read type alias")?;
                    self.i = self.skip_recovery(stmt_start);
                    return Ok(());
                }
            }
        }
        if self.txt(idx) != "=" {
            self.unparsed(stmt_start, "cannot read type alias")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let type_start = idx.saturating_add(1);
        let end_exclusive = self.type_statement_end(type_start);
        if end_exclusive <= type_start {
            self.unparsed(stmt_start, "cannot read type alias")?;
            self.i = self.skip_recovery(stmt_start);
            return Ok(());
        }
        let last = end_exclusive.saturating_sub(1);
        let key = format!("{}::{name}", self.module_key);
        self.push(
            line,
            key,
            Confidence::Exact,
            &TypeDefData {
                name,
                type_kind: TypeKind::TypeAlias,
                end_line: self.line(last),
                hash: self.hash_tokens(stmt_start, last),
            },
        )?;
        self.i = if self.txt(end_exclusive) == ";" {
            end_exclusive.saturating_add(1)
        } else {
            end_exclusive
        };
        Ok(())
    }

    fn type_statement_end(&self, start: usize) -> usize {
        let mut brackets: Vec<&str> = Vec::new();
        let mut angle: usize = 0;
        let mut idx = start;
        while idx < self.toks.len() {
            let text = self.txt(idx);
            if brackets.is_empty() && angle == 0 && text == ";" {
                return idx;
            }
            if brackets.is_empty() && angle == 0 && idx > start && self.ends_at_newline(idx) {
                return idx;
            }
            if text == "<"
                && (self.kind(idx.saturating_sub(1)) == Some(TokKind::Ident)
                    || self.txt(idx.saturating_sub(1)) == ">")
            {
                angle = angle.saturating_add(1);
            } else if text == ">" && angle > 0 {
                angle = angle.saturating_sub(1);
            }
            if text == "(" || text == "[" || text == "{" {
                brackets.push(text);
            } else if text == ")" || text == "]" || text == "}" {
                if brackets.is_empty() && angle == 0 {
                    return idx;
                }
                brackets.pop();
            }
            idx = idx.saturating_add(1);
        }
        idx
    }

    // ----- class and object members -----

    fn parse_class_member(&mut self) -> Result<bool> {
        let (class_name, _) = match self.scopes.last() {
            Some(Scope::Class { class_name, .. }) => (class_name.clone(), ()),
            _ => return Ok(false),
        };
        let start = self.i;
        if self.txt(start) == ";" {
            self.set_class_expect(true);
            self.i = start.saturating_add(1);
            return Ok(true);
        }
        if self.txt(start) == "@" {
            // A decorator (`@Get(':id')`, `@a.b`) is skipped whole; the member follows it.
            let mut idx = start.saturating_add(1);
            if let Some((_, last)) = self.parse_chain(idx) {
                idx = last.saturating_add(1);
                if self.txt(idx) == "(" {
                    if let Some(close) = self.match_at(idx) {
                        idx = close.saturating_add(1);
                    }
                }
            }
            self.i = idx;
            return Ok(true);
        }
        let mut idx = start;
        let mut is_async = false;
        loop {
            let text = self.txt(idx);
            let next = idx.saturating_add(1);
            match text {
                "public" | "private" | "protected" | "readonly" | "abstract" | "declare"
                | "override" | "accessor" => idx = next,
                "static" => {
                    if self.txt(next) == "{" {
                        self.scopes.push(Scope::Block);
                        self.i = next.saturating_add(1);
                        return Ok(true);
                    }
                    if self.txt(next) == "("
                        || self.txt(next) == "="
                        || self.txt(next) == ";"
                        || self.txt(next) == ":"
                    {
                        break;
                    }
                    idx = next;
                }
                "async" => {
                    if self.txt(next) == "(" || self.txt(next) == "=" || self.txt(next) == ";" {
                        break;
                    }
                    is_async = true;
                    idx = next;
                }
                "get" | "set" => {
                    let after_name = next.saturating_add(1);
                    if (self.kind(next) == Some(TokKind::Ident)
                        || self.kind(next) == Some(TokKind::Str))
                        && (self.txt(after_name) == "(" || self.txt(after_name) == "<")
                    {
                        idx = next;
                    }
                    break;
                }
                _ => break,
            }
            if idx >= self.toks.len() {
                return Ok(false);
            }
        }
        if self.txt(idx) == "*" {
            idx = idx.saturating_add(1);
        }
        let (name, name_idx) = match self.kind(idx) {
            Some(TokKind::Ident) => (self.txt(idx).to_owned(), idx),
            Some(TokKind::Str) => {
                let value = crate::lex::ts::unquote(self.txt(idx))
                    .unwrap_or_else(|| self.txt(idx).to_owned());
                (value, idx)
            }
            _ => return Ok(false),
        };
        let mut after = name_idx.saturating_add(1);
        if self.txt(after) == "?" || self.txt(after) == "!" {
            after = after.saturating_add(1);
        }
        if self.txt(after) == "<" {
            if let Some(next) = self.skip_angles(after) {
                after = next;
            }
        }
        if self.txt(after) != "(" {
            // A field: scan its value as ordinary code until the closing semicolon.
            self.set_class_expect(false);
            self.i = name_idx.saturating_add(1);
            return Ok(true);
        }
        let Some(close) = self.match_at(after) else {
            self.unparsed(start, "cannot read class method")?;
            self.i = self.skip_recovery(start);
            return Ok(true);
        };
        match self.body_after_params(close.saturating_add(1)) {
            BodyAfter::Signature(resume) => {
                self.set_class_expect(true);
                self.i = resume;
                Ok(true)
            }
            BodyAfter::Brace(brace) => {
                let key = format!("{}::{class_name}.{name}", self.module_key);
                self.scopes.push(Scope::Function {
                    pending: Some(PendingFn {
                        name,
                        key,
                        owner_type: Some(class_name),
                        is_async,
                        exported: false,
                        use_export_list: false,
                        line: self.line(name_idx),
                        start,
                    }),
                    expr_end: None,
                });
                self.i = brace.saturating_add(1);
                Ok(true)
            }
            BodyAfter::None => {
                self.unparsed(start, "cannot read class method")?;
                self.i = self.skip_recovery(start);
                Ok(true)
            }
        }
    }

    fn parse_object_member(&mut self) -> Result<bool> {
        let start = self.i;
        let mut idx = start;
        if self.txt(idx) == "*" {
            idx = idx.saturating_add(1);
        }
        // Accessors and async methods: the member name follows the modifier.
        if (self.is_ident(idx, "get") || self.is_ident(idx, "set") || self.is_ident(idx, "async"))
            && (self.kind(idx.saturating_add(1)) == Some(TokKind::Ident)
                || self.kind(idx.saturating_add(1)) == Some(TokKind::Str))
        {
            let after_name = idx.saturating_add(2);
            if self.txt(after_name) == "(" || self.txt(after_name) == "<" {
                idx = idx.saturating_add(1);
            }
        }
        let name_idx = idx;
        if self.kind(name_idx) != Some(TokKind::Ident)
            && self.kind(name_idx) != Some(TokKind::Str)
            && self.kind(name_idx) != Some(TokKind::Number)
        {
            return Ok(false);
        }
        let mut after = name_idx.saturating_add(1);
        if self.txt(after) == "<" {
            if let Some(next) = self.skip_angles(after) {
                after = next;
            }
        }
        if self.txt(after) == "(" {
            let Some(close) = self.match_at(after) else {
                return Ok(false);
            };
            match self.body_after_params(close.saturating_add(1)) {
                BodyAfter::Brace(brace) => {
                    self.scopes.push(Scope::MethodBody);
                    self.i = brace.saturating_add(1);
                    Ok(true)
                }
                _ => Ok(false),
            }
        } else if self.txt(after) == ":" {
            self.set_object_expect(false);
            self.i = after.saturating_add(1);
            Ok(true)
        } else if self.txt(after) == "," || self.txt(after) == "}" || self.txt(after) == "=" {
            if self.txt(after) == "=" {
                self.set_object_expect(false);
            }
            self.i = after;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ----- environment variables and SQL -----

    fn try_env_var(&mut self) -> Result<bool> {
        let start = self.i;
        // Reject a property named `process` or `import` used as a receiver segment.
        if start > 0
            && (self.txt(start.saturating_sub(1)) == "."
                || self.txt(start.saturating_sub(1)) == "?.")
        {
            return Ok(false);
        }
        let owner = self.current_owner();
        let line = self.line(start);
        if self.is_ident(start, "process")
            && self.txt(start.saturating_add(1)) == "."
            && self.is_ident(start.saturating_add(2), "env")
        {
            let after_env = start.saturating_add(3);
            if self.txt(after_env) == "."
                && self.kind(after_env.saturating_add(1)) == Some(TokKind::Ident)
            {
                let name = self.txt(after_env.saturating_add(1)).to_owned();
                self.push(line, name, Confidence::Exact, &EnvVarData { owner })?;
                self.i = after_env.saturating_add(2);
                return Ok(true);
            }
            if self.txt(after_env) == "["
                && self.kind(after_env.saturating_add(1)) == Some(TokKind::Str)
            {
                if let Some(name) = crate::lex::ts::unquote(self.txt(after_env.saturating_add(1))) {
                    self.push(line, name, Confidence::Exact, &EnvVarData { owner })?;
                    let close = after_env.saturating_add(2);
                    self.i = if self.txt(close) == "]" {
                        close.saturating_add(1)
                    } else {
                        close
                    };
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        if self.is_ident(start, "import")
            && self.txt(start.saturating_add(1)) == "."
            && self.is_ident(start.saturating_add(2), "meta")
            && self.txt(start.saturating_add(3)) == "."
            && self.is_ident(start.saturating_add(4), "env")
            && self.txt(start.saturating_add(5)) == "."
            && self.kind(start.saturating_add(6)) == Some(TokKind::Ident)
        {
            let name = self.txt(start.saturating_add(6)).to_owned();
            self.push(line, name, Confidence::Exact, &EnvVarData { owner })?;
            self.i = start.saturating_add(7);
            return Ok(true);
        }
        Ok(false)
    }

    fn check_sql(&mut self, idx: usize) -> Result<()> {
        let value = match self.kind(idx) {
            Some(TokKind::Str) => crate::lex::ts::unquote(self.txt(idx)),
            Some(TokKind::Template) => crate::lex::ts::template_pattern(self.txt(idx)),
            _ => None,
        };
        let Some(value) = value else { return Ok(()) };
        if !crate::extract::sql::looks_like_sql(&value) {
            return Ok(());
        }
        let owner = self.current_owner();
        let line = self.line(idx);
        let preview = crate::extract::sql::sql_preview(&value);
        self.push(
            line,
            owner.clone(),
            Confidence::Heuristic,
            &SqlInCodeData {
                owner: owner.clone(),
                preview,
            },
        )?;
        for (table, op) in crate::extract::sql::sql_text_accesses(&value) {
            self.push(
                line,
                table.clone(),
                Confidence::Heuristic,
                &TableAccessData {
                    owner: owner.clone(),
                    table,
                    op,
                },
            )?;
        }
        Ok(())
    }

    // ----- calls, routes and API calls -----

    fn parse_chain(&self, start: usize) -> Option<(Vec<String>, usize)> {
        if self.kind(start) != Some(TokKind::Ident) {
            return None;
        }
        let mut segments = vec![self.txt(start).to_owned()];
        let mut idx = start;
        loop {
            let sep = idx.saturating_add(1);
            let next = idx.saturating_add(2);
            if (self.txt(sep) == "." || self.txt(sep) == "?.")
                && self.kind(next) == Some(TokKind::Ident)
            {
                segments.push(self.txt(next).to_owned());
                idx = next;
            } else {
                break;
            }
        }
        Some((segments, idx))
    }

    fn call_paren(&self, after_segment: usize) -> Option<usize> {
        let next = after_segment.saturating_add(1);
        if self.txt(next) == "(" {
            return Some(next);
        }
        if self.txt(next) == "?." && self.txt(next.saturating_add(1)) == "(" {
            return Some(next.saturating_add(1));
        }
        if self.txt(next) == "<" {
            let after = self.skip_angles(next)?;
            if self.txt(after) == "(" {
                return Some(after);
            }
        }
        None
    }

    fn try_call(&mut self) -> Result<bool> {
        let start = self.i;
        let text = self.txt(start).to_owned();
        if text == "new" {
            return self.try_new_call();
        }
        if self.kind(start) != Some(TokKind::Ident) {
            return Ok(false);
        }
        let prev = if start == 0 {
            ""
        } else {
            self.txt(start.saturating_sub(1))
        };
        let segments: Vec<String>;
        let last_idx: usize;
        let mut chained = false;
        if prev == "." || prev == "?." {
            // A segment after a closing bracket is a chained call; a segment after an
            // identifier was already read as part of that chain.
            let before_sep = if start >= 2 {
                self.txt(start.saturating_sub(2))
            } else {
                ""
            };
            let before_kind = if start >= 2 {
                self.kind(start.saturating_sub(2))
            } else {
                None
            };
            let chained_receiver = matches!(before_sep, ")" | "]" | "}")
                || matches!(
                    before_kind,
                    Some(TokKind::Template) | Some(TokKind::Str) | Some(TokKind::Number)
                );
            if !chained_receiver {
                return Ok(false);
            }
            segments = vec![text.clone()];
            last_idx = start;
            chained = true;
        } else {
            let Some((parsed, last)) = self.parse_chain(start) else {
                return Ok(false);
            };
            segments = parsed;
            last_idx = last;
        }
        let Some(paren) = self.call_paren(last_idx) else {
            return Ok(false);
        };
        let Some((name, qualifier)) = segments
            .split_last()
            .map(|(last, rest)| (last.clone(), rest.to_vec()))
        else {
            return Ok(false);
        };
        if segments.len() == 1 && KEYWORD_CALLEES.contains(&name.as_str()) {
            return Ok(false);
        }
        // `async (x) => ...` and `async (x): T => ...` are arrow functions, not calls.
        if segments.len() == 1 && name == "async" {
            let after = self.match_at(paren).map(|close| close.saturating_add(1));
            if after.is_some_and(|after| matches!(self.txt(after), "=>" | ":")) {
                return Ok(false);
            }
        }
        if let Some(first) = segments.first() {
            if KEYWORD_CALLEES.contains(&first.as_str()) {
                return Ok(false);
            }
        }
        let is_method = chained || segments.len() > 1;
        let receiver = if !chained && segments.len() == 2 {
            segments.first().cloned()
        } else {
            None
        };
        let callee = segments.join(".");
        let info = CallInfo {
            callee: callee.clone(),
            name: name.clone(),
            segments: segments.clone(),
            is_method,
            receiver,
            line: self.line(start),
            owner: self.current_owner(),
            paren,
        };
        self.push(
            info.line,
            callee,
            Confidence::Exact,
            &CallData {
                owner: info.owner.clone(),
                callee: info.callee.clone(),
                name,
                qualifier,
                receiver: info.receiver.clone(),
                is_method,
                is_macro: false,
            },
        )?;
        self.check_patterns(&info)?;
        self.i = paren;
        Ok(true)
    }

    fn try_new_call(&mut self) -> Result<bool> {
        let start = self.i;
        let first = start.saturating_add(1);
        let Some((segments, last_idx)) = self.parse_chain(first) else {
            return Ok(false);
        };
        let Some(paren) = self.call_paren(last_idx) else {
            return Ok(false);
        };
        let Some((name, qualifier)) = segments
            .split_last()
            .map(|(last, rest)| (last.clone(), rest.to_vec()))
        else {
            return Ok(false);
        };
        let callee = format!("new {}", segments.join("."));
        let info = CallInfo {
            callee: callee.clone(),
            name: name.clone(),
            segments: segments.clone(),
            is_method: false,
            receiver: None,
            line: self.line(start),
            owner: self.current_owner(),
            paren,
        };
        self.push(
            info.line,
            callee,
            Confidence::Exact,
            &CallData {
                owner: info.owner.clone(),
                callee: info.callee.clone(),
                name,
                qualifier,
                receiver: None,
                is_method: false,
                is_macro: false,
            },
        )?;
        self.check_patterns(&info)?;
        self.i = paren;
        Ok(true)
    }

    fn arg_ranges(&self, paren: usize) -> Vec<(usize, usize)> {
        let close = self.match_at(paren).unwrap_or(self.toks.len());
        let mut ranges = Vec::new();
        let mut depth: usize = 0;
        let mut arg_start = paren.saturating_add(1);
        let mut idx = arg_start;
        while idx < close {
            let text = self.txt(idx);
            if depth == 0 && text == "," {
                if arg_start < idx {
                    ranges.push((arg_start, idx));
                }
                arg_start = idx.saturating_add(1);
            } else if text == "(" || text == "[" || text == "{" {
                depth = depth.saturating_add(1);
            } else if text == ")" || text == "]" || text == "}" {
                depth = depth.saturating_sub(1);
            }
            idx = idx.saturating_add(1);
        }
        if arg_start < close {
            ranges.push((arg_start, close));
        }
        ranges
    }

    fn arg_path(&self, args: &[(usize, usize)], arg_idx: usize) -> Option<String> {
        let (start, end) = *args.get(arg_idx)?;
        if end != start.saturating_add(1) {
            return None;
        }
        match self.kind(start) {
            Some(TokKind::Str) => crate::lex::ts::unquote(self.txt(start)),
            Some(TokKind::Template) => crate::lex::ts::template_pattern(self.txt(start)),
            _ => None,
        }
    }

    fn arg_string(&self, args: &[(usize, usize)], arg_idx: usize) -> Option<String> {
        let (start, end) = *args.get(arg_idx)?;
        if end != start.saturating_add(1) || self.kind(start) != Some(TokKind::Str) {
            return None;
        }
        crate::lex::ts::unquote(self.txt(start))
    }

    fn check_patterns(&mut self, info: &CallInfo) -> Result<()> {
        let args = self.arg_ranges(info.paren);
        // The patterns borrow the configuration, not the parser, so no copy is needed.
        let clients = self.api_clients;
        for pattern in clients {
            if pattern.lang != PatternLang::Ts {
                continue;
            }
            self.check_api_pattern(info, &args, pattern)?;
        }
        Ok(())
    }

    fn check_api_pattern(
        &mut self,
        info: &CallInfo,
        args: &[(usize, usize)],
        pattern: &ApiClientPattern,
    ) -> Result<()> {
        if info.name != pattern.call {
            return Ok(());
        }
        let star_receiver = pattern.receivers.len() == 1
            && pattern.receivers.first().map(|r| r.as_str()) == Some("*");
        if pattern.receivers.is_empty() {
            if info.is_method || info.segments.len() != 1 {
                return Ok(());
            }
        } else if star_receiver {
            if !info.is_method {
                return Ok(());
            }
        } else {
            if !info.is_method {
                return Ok(());
            }
            let receiver_ok = info
                .receiver
                .as_ref()
                .map(|r| pattern.receivers.contains(r))
                .unwrap_or(false)
                || info
                    .segments
                    .first()
                    .map(|r| pattern.receivers.contains(r))
                    .unwrap_or(false);
            if !receiver_ok {
                return Ok(());
            }
        }
        let Some(raw_path) = self.arg_path(args, pattern.path_arg) else {
            return Ok(());
        };
        let method = if let Some(method) = &pattern.method {
            method.clone()
        } else if let Some(method_arg) = pattern.method_arg {
            match self.arg_string(args, method_arg) {
                Some(value) => value.to_ascii_uppercase(),
                None => self.api_fallback_method(args, pattern),
            }
        } else {
            self.api_fallback_method(args, pattern)
        };
        let stripped = match &pattern.strip_prefix {
            Some(prefix) => raw_path
                .strip_prefix(prefix.as_str())
                .unwrap_or(&raw_path)
                .to_owned(),
            None => raw_path.clone(),
        };
        let normalized = match &pattern.base_path {
            Some(base) => norm_path_in(base, &stripped),
            None => norm_path(&stripped),
        };
        let key = format!("{method} {normalized}");
        self.push(
            info.line,
            key,
            Confidence::Heuristic,
            &ApiCallData {
                method,
                path: raw_path,
                norm_path: normalized,
                via: info.callee.clone(),
                pattern: pattern.id.clone(),
                owner: info.owner.clone(),
            },
        )
    }

    fn api_fallback_method(&self, args: &[(usize, usize)], pattern: &ApiClientPattern) -> String {
        if let Some(option_name) = &pattern.method_option {
            if let Some(value) = self.method_option_value(args, option_name) {
                return value.to_ascii_uppercase();
            }
        }
        match &pattern.default_method {
            Some(method) => method.to_ascii_uppercase(),
            None => "GET".to_owned(),
        }
    }

    fn method_option_value(&self, args: &[(usize, usize)], option_name: &str) -> Option<String> {
        for (start, end) in args {
            if self.txt(*start) != "{" {
                continue;
            }
            let mut idx = *start;
            while idx < *end {
                let is_key = (self.kind(idx) == Some(TokKind::Ident)
                    && self.txt(idx) == option_name)
                    || (self.kind(idx) == Some(TokKind::Str)
                        && crate::lex::ts::unquote(self.txt(idx))
                            .map(|v| v == option_name)
                            .unwrap_or(false));
                if is_key && self.txt(idx.saturating_add(1)) == ":" {
                    let value_idx = idx.saturating_add(2);
                    if self.kind(value_idx) == Some(TokKind::Str) {
                        return crate::lex::ts::unquote(self.txt(value_idx));
                    }
                }
                idx = idx.saturating_add(1);
            }
        }
        None
    }
}

fn compute_matching(tokens: &[crate::lex::Token], text: &str) -> Vec<Option<usize>> {
    let mut matching: Vec<Option<usize>> = vec![None; tokens.len()];
    let mut stack: Vec<(&str, usize)> = Vec::new();
    for (idx, tok) in tokens.iter().enumerate() {
        let tok_text = tok.text(text);
        if tok_text == "(" || tok_text == "[" || tok_text == "{" {
            stack.push((tok_text, idx));
        } else if tok_text == ")" || tok_text == "]" || tok_text == "}" {
            let want = match tok_text {
                ")" => "(",
                "]" => "[",
                _ => "{",
            };
            let mut pos: Option<usize> = None;
            for (stack_idx, (open, _)) in stack.iter().enumerate().rev() {
                if *open == want {
                    pos = Some(stack_idx);
                    break;
                }
            }
            if let Some(stack_idx) = pos {
                if let Some((_, open_idx)) = stack.get(stack_idx).copied() {
                    if let Some(slot) = matching.get_mut(open_idx) {
                        *slot = Some(idx);
                    }
                    if let Some(slot) = matching.get_mut(idx) {
                        *slot = Some(open_idx);
                    }
                }
                stack.truncate(stack_idx);
            }
        }
    }
    matching
}

fn module_name(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
        _ => file_name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::extract::test_support::{calls, facts, facts_with, keys};
    use crate::model::FactKind;

    #[test]
    fn abstract_method_without_semicolon_does_not_take_the_next_body() {
        let facts = facts(
            "src/repo.ts",
            "abstract class Repo {\n  abstract load(): string\n  save(): void { persist(); }\n}\n",
        );
        let fns = keys(&facts, FactKind::FnDef);
        assert!(
            fns.contains(&"src/repo.ts::Repo.save".to_owned()),
            "{fns:?}"
        );
        assert!(
            calls(&facts).contains(&("src/repo.ts::Repo.save".to_owned(), "persist".to_owned())),
            "{:?}",
            calls(&facts)
        );
        assert!(keys(&facts, FactKind::Unparsed).is_empty());
    }

    #[test]
    fn annotation_ends_at_a_line_break() {
        let facts = facts("src/a.ts", "let a: number\nstart()\nlet b = 1\n");
        assert!(
            calls(&facts).iter().any(|(_, name)| name == "start"),
            "{:?}",
            calls(&facts)
        );
    }

    #[test]
    fn generic_object_type_in_class_header() {
        let facts = facts(
            "src/a.ts",
            "class Store extends Base<{ id: string }> {\n  get(): void { fetchAll(); }\n}\n",
        );
        assert_eq!(
            keys(&facts, FactKind::TypeDef),
            vec!["src/a.ts::Store".to_owned()]
        );
        assert!(keys(&facts, FactKind::FnDef).contains(&"src/a.ts::Store.get".to_owned()));
    }

    #[test]
    fn configured_client_calls_are_api_calls_not_routes() {
        let config = Config::from_bytes(
            br#"{ "api_clients": [ {
                "id": "client", "lang": "ts", "call": "request", "receivers": ["*"],
                "method_arg": 0, "path_arg": 1, "base_path": "/api/v1"
            } ] }"#,
        )
        .expect("test configuration");
        let facts = facts_with(
            &config,
            "src/orders.ts",
            "export async function save(d: Draft) {\n  await api.request('POST', '/orders', d);\n}\napp.get('/health', health);\n",
        );
        assert_eq!(
            keys(&facts, FactKind::ApiCall),
            vec!["POST /api/v1/orders".to_owned()]
        );
        assert!(keys(&facts, FactKind::Route).is_empty());
    }

    #[test]
    fn decorators_are_skipped() {
        let facts = facts(
            "src/c.ts",
            "class C {\n  @Get(':id')\n  find(id: string) { return load(id); }\n}\n",
        );
        assert!(keys(&facts, FactKind::FnDef).contains(&"src/c.ts::C.find".to_owned()));
        assert!(keys(&facts, FactKind::Unparsed).is_empty());
    }
}
