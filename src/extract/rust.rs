//! Rust extractor (`SPEC.md`, "Extraction", "Rust").

use super::{CrateRoot, ExtractCtx};
use crate::config::Config;
use crate::error::Result;
use crate::lex::{Lexed, TokKind, Token};
use crate::model::{
    CallData, Confidence, ConstDefData, EnvVarData, Fact, FnDefData, ImplData, ImportData,
    ModuleData, RelPath, RouteData, SqlInCodeData, TableAccessData, TypeDefData, TypeKind,
    UnparsedData, norm_path, norm_path_in,
};
use crate::source::SourceFile;

/// Facts of a Rust file (everything except the `file` fact and lexer errors).
pub fn extract_rust(file: &SourceFile, lexed: &Lexed, ctx: &ExtractCtx<'_>) -> Result<Vec<Fact>> {
    let (crate_name, module_path) = match rust_location(&file.path, ctx.crates) {
        Some(location) => location,
        None => (String::from("_"), fallback_module_path(file.path.as_str())),
    };
    let module_key = if module_path.is_empty() {
        crate_name.clone()
    } else {
        format!("{crate_name}::{module_path}")
    };
    let module_name = if module_path.is_empty() {
        crate_name.clone()
    } else {
        match module_path.rsplit("::").next() {
            Some(last) => last.to_owned(),
            None => crate_name.clone(),
        }
    };
    let mut extractor = Extractor {
        file,
        src: &file.text,
        toks: &lexed.tokens,
        config: ctx.config,
        facts: Vec::new(),
        scopes: vec![Scope::Module { key: module_key }],
        blocks: Vec::new(),
        pending: None,
        const_init_depth: None,
        bracket_stack: Vec::new(),
        unbalanced: false,
        extra_close: false,
    };
    extractor.facts.push(Fact::new(
        &file.path,
        0,
        extractor.module_key(),
        Confidence::Exact,
        &ModuleData { name: module_name },
    )?);
    extractor.run()?;
    Ok(extractor.facts)
}

/// Crate name and module path of a Rust file, or `None` when the file is not under any
/// crate's `src/`, `tests/`, `examples/` or `benches/` directory. The module path is `""` for a
/// crate root (`SPEC.md`, "Rust keys").
pub fn rust_location(path: &RelPath, crates: &[CrateRoot]) -> Option<(String, String)> {
    let path_str = path.as_str();
    let mut best: Option<&CrateRoot> = None;
    for krate in crates {
        let dir = krate.dir.as_str();
        let matches = dir.is_empty()
            || path_str
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'));
        if !matches {
            continue;
        }
        let better = match best {
            None => true,
            Some(current) => dir.len() > current.dir.len(),
        };
        if better {
            best = Some(krate);
        }
    }
    let krate = best?;
    let relative: &str = if krate.dir.is_empty() {
        path_str
    } else {
        path_str.get(krate.dir.len().checked_add(1)?..)?
    };
    let (first, rest) = relative.split_once('/')?;
    let module_path = match first {
        "src" => src_module_path(rest)?,
        "tests" | "examples" | "benches" => prefixed_module_path(first, rest)?,
        _ => return None,
    };
    Some((krate.name.clone(), module_path))
}

/// Package name from `Cargo.toml` text (`name = "..."` in the `[package]` table), with `-`
/// replaced by `_`; `None` for a workspace-only manifest.
pub fn crate_name_from_manifest(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]"
                || trimmed.strip_prefix("[package]").is_some_and(|rest| {
                    let rest = rest.trim();
                    rest.is_empty() || rest.starts_with('#')
                });
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let inner = rest.get(1..)?;
        let end = inner.find(quote)?;
        let name = inner.get(..end)?;
        if name.is_empty() {
            return None;
        }
        return Some(name.replace('-', "_"));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Location helpers

/// Path segments of a file below a crate directory: directories as they are, the file name
/// without `.rs`. `None` when the file is not a `.rs` file.
fn rs_segments(rest: &str) -> Option<Vec<&str>> {
    let (dirs, file) = match rest.rsplit_once('/') {
        Some((dirs, file)) => (Some(dirs), file),
        None => (None, rest),
    };
    let mut segments: Vec<&str> = match dirs {
        Some(dirs) => dirs.split('/').collect(),
        None => Vec::new(),
    };
    segments.push(file.strip_suffix(".rs")?);
    Some(segments)
}

fn src_module_path(rest: &str) -> Option<String> {
    if rest == "lib.rs" || rest == "main.rs" {
        return Some(String::new());
    }
    let mut segments = rs_segments(rest)?;
    let is_bin = segments.first().is_some_and(|first| *first == "bin");
    if segments.len() > 1 {
        let drop_last = match segments.last() {
            Some(last) => *last == "mod" || (is_bin && *last == "main"),
            None => false,
        };
        if drop_last {
            segments.pop();
        }
    } else if segments.first().is_some_and(|first| *first == "mod") {
        segments.pop();
    }
    Some(segments.join("::"))
}

fn prefixed_module_path(prefix: &str, rest: &str) -> Option<String> {
    let mut segments: Vec<&str> = vec![prefix];
    segments.extend(rs_segments(rest)?);
    if segments.len() > 1 && segments.last().is_some_and(|last| *last == "mod") {
        segments.pop();
    }
    Some(segments.join("::"))
}

fn fallback_module_path(path: &str) -> String {
    let without_extension = match path.rfind('.') {
        Some(dot) => {
            let last_slash = path.rfind('/');
            let dot_in_name = match last_slash {
                Some(slash) => dot > slash,
                None => true,
            };
            if dot_in_name {
                match path.get(..dot) {
                    Some(stem) => stem,
                    None => path,
                }
            } else {
                path
            }
        }
        None => path,
    };
    let mut segments: Vec<String> = Vec::new();
    for segment in without_extension.split('/') {
        let mut cleaned = String::new();
        for ch in segment.chars() {
            if ch.is_alphanumeric() || ch == '_' {
                cleaned.push(ch);
            } else {
                cleaned.push('_');
            }
        }
        segments.push(cleaned);
    }
    segments.join("::")
}

// ---------------------------------------------------------------------------------------------
// Extractor state

#[derive(Clone, Debug)]
enum Scope {
    Module {
        key: String,
    },
    Impl {
        self_type: String,
        trait_name: Option<String>,
    },
    Trait {
        name: String,
    },
    Function {
        key: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    Items,
    FunctionBody,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Block {
    scope_len_before: Option<usize>,
    kind: BlockKind,
}

#[derive(Clone, Copy, Debug)]
enum Outcome {
    NotItem,
    Next(usize),
}

struct CallInfo {
    start: usize,
    name: String,
    callee: String,
    qualifier: Vec<String>,
    receiver: Option<String>,
    is_method: bool,
    is_macro: bool,
    open: Option<usize>,
    close: Option<usize>,
    segments: Vec<String>,
}

struct Extractor<'a> {
    file: &'a SourceFile,
    src: &'a str,
    toks: &'a [Token],
    config: &'a Config,
    facts: Vec<Fact>,
    scopes: Vec<Scope>,
    blocks: Vec<Block>,
    pending: Option<Scope>,
    const_init_depth: Option<usize>,
    bracket_stack: Vec<&'a str>,
    unbalanced: bool,
    extra_close: bool,
}

const SKIP_CALLEES: [&str; 24] = [
    "if", "while", "match", "for", "loop", "return", "let", "in", "as", "fn", "move", "async",
    "await", "unsafe", "impl", "dyn", "where", "Some", "None", "mut", "ref", "break", "else",
    "continue",
];

// `Ok` and `Err` are skipped only as bare callees; they are handled with the names above when
// unqualified, so they stay in a separate list checked together with the first list.
const SKIP_BARE_EXTRA: [&str; 2] = ["Ok", "Err"];

impl<'a> Extractor<'a> {
    // Token helpers ----------------------------------------------------------------------------

    fn text(&self, index: usize) -> &'a str {
        match self.toks.get(index) {
            Some(token) => token.text(self.src),
            None => "",
        }
    }

    fn kind(&self, index: usize) -> Option<TokKind> {
        self.toks.get(index).map(|token| token.kind)
    }

    fn line(&self, index: usize) -> u32 {
        match self.toks.get(index) {
            Some(token) => token.line,
            None => match self.toks.last() {
                Some(token) => token.line,
                None => 0,
            },
        }
    }

    fn is_punct(&self, index: usize, text: &str) -> bool {
        self.kind(index) == Some(TokKind::Punct) && self.text(index) == text
    }

    fn is_word(&self, index: usize, word: &str) -> bool {
        self.kind(index) == Some(TokKind::Ident) && self.text(index) == word
    }

    fn ident_name(&self, index: usize) -> Option<String> {
        if self.kind(index) != Some(TokKind::Ident) {
            return None;
        }
        let raw = self.text(index);
        let name = match raw.strip_prefix("r#") {
            Some(stripped) => stripped,
            None => raw,
        };
        Some(name.to_owned())
    }

    fn find_matching(&self, open_index: usize, open: &str, close: &str) -> Option<usize> {
        let mut depth: usize = 0;
        let mut index = open_index;
        while index < self.toks.len() {
            let text = self.text(index);
            if text == open {
                depth = depth.checked_add(1)?;
            } else if text == close {
                if depth <= 1 {
                    return Some(index);
                }
                depth = depth.checked_sub(1)?;
            }
            index = index.checked_add(1)?;
        }
        None
    }

    fn find_angle_end(&self, open_index: usize) -> Option<usize> {
        let mut depth: usize = 0;
        let mut index = open_index;
        // Generic arguments never contain these tokens and are never this long; stopping here
        // keeps `a < b && c > (d)` from looking like a call and bounds the scan.
        let limit = open_index.saturating_add(256);
        while index < self.toks.len() && index < limit {
            let text = self.text(index);
            if matches!(text, ";" | "{" | "}" | "&&" | "||" | "==" | "!=") {
                return None;
            }
            if text == "<" {
                depth = depth.checked_add(1)?;
            } else if text == ">" {
                if depth <= 1 {
                    return index.checked_add(1);
                }
                depth = depth.checked_sub(1)?;
            }
            index = index.checked_add(1)?;
        }
        None
    }

    /// True when the `>` at `close` ends a qualified path start such as `<T as Trait>` or
    /// `<Vec<u8>>`, and false when it ends turbofish arguments (`Vec::<u8>`).
    fn closes_qualified_path(&self, close: usize) -> bool {
        let mut depth: usize = 0;
        let mut index = close;
        let floor = close.saturating_sub(256);
        loop {
            match self.text(index) {
                ">" => depth = depth.saturating_add(1),
                "<" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return index == 0 || !self.is_punct(index.wrapping_sub(1), "::");
                    }
                }
                ";" | "{" | "}" => return false,
                _ => {}
            }
            match index.checked_sub(1) {
                Some(previous) if previous >= floor => index = previous,
                _ => return false,
            }
        }
    }

    fn skip_attributes(&self, start: usize) -> usize {
        let mut index = start;
        loop {
            if !self.is_punct(index, "#") {
                return index;
            }
            let mut bracket = match index.checked_add(1) {
                Some(next) => next,
                None => return self.toks.len(),
            };
            if self.is_punct(bracket, "!") {
                bracket = match bracket.checked_add(1) {
                    Some(next) => next,
                    None => return self.toks.len(),
                };
            }
            if !self.is_punct(bracket, "[") {
                return index;
            }
            match self.find_matching(bracket, "[", "]") {
                Some(close) => match close.checked_add(1) {
                    Some(next) => index = next,
                    None => return self.toks.len(),
                },
                None => return self.toks.len(),
            }
        }
    }

    fn hash_range(&self, start: usize, end: usize) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut index = start;
        while index <= end && index < self.toks.len() {
            parts.push(self.text(index));
            match index.checked_add(1) {
                Some(next) => index = next,
                None => break,
            }
        }
        crate::util::fnv::hex(parts.join(" ").as_bytes())
    }

    fn skip_malformed(&self, from: usize) -> usize {
        let mut brace: usize = 0;
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut seen_open = false;
        let mut index = from;
        while index < self.toks.len() {
            match self.text(index) {
                "{" => {
                    brace = brace.saturating_add(1);
                    seen_open = true;
                }
                "(" => {
                    paren = paren.saturating_add(1);
                    seen_open = true;
                }
                "[" => {
                    bracket = bracket.saturating_add(1);
                    seen_open = true;
                }
                "}" => {
                    if brace == 0 {
                        return index;
                    }
                    brace -= 1;
                    if seen_open && brace == 0 && paren == 0 && bracket == 0 {
                        return index.saturating_add(1);
                    }
                }
                ")" => {
                    if paren == 0 {
                        return index;
                    }
                    paren -= 1;
                    if seen_open && brace == 0 && paren == 0 && bracket == 0 {
                        return index.saturating_add(1);
                    }
                }
                "]" => {
                    if bracket == 0 {
                        return index;
                    }
                    bracket -= 1;
                    if seen_open && brace == 0 && paren == 0 && bracket == 0 {
                        return index.saturating_add(1);
                    }
                }
                ";" if brace == 0 && paren == 0 && bracket == 0 => {
                    return index.saturating_add(1);
                }
                _ => {}
            }
            index = index.saturating_add(1);
        }
        self.toks.len()
    }

    // Fact helpers -----------------------------------------------------------------------------

    fn push_unparsed(&mut self, line: u32, reason: &str) -> Result<()> {
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            self.file.path.as_str(),
            Confidence::Exact,
            &UnparsedData {
                reason: reason.to_owned(),
            },
        )?);
        Ok(())
    }

    fn push_unbalanced(&mut self, line: u32) -> Result<()> {
        if !self.unbalanced {
            self.unbalanced = true;
            self.push_unparsed(line, "unbalanced brackets")?;
        }
        Ok(())
    }

    fn module_key(&self) -> String {
        for scope in self.scopes.iter().rev() {
            if let Scope::Module { key } = scope {
                return key.clone();
            }
        }
        String::new()
    }

    fn function_key(&self) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Scope::Function { key } = scope {
                return Some(key.clone());
            }
        }
        None
    }

    fn in_function(&self) -> bool {
        self.function_key().is_some()
    }

    fn owner_key(&self) -> String {
        match self.function_key() {
            Some(key) => key,
            None => self.module_key(),
        }
    }

    fn callable_context(&self) -> bool {
        self.in_function() || self.const_init_depth.is_some()
    }

    // Main loop --------------------------------------------------------------------------------

    fn run(&mut self) -> Result<()> {
        let mut index: usize = 0;
        while index < self.toks.len() {
            if self.is_punct(index, "{") {
                self.open_block();
                index = index.saturating_add(1);
                continue;
            }
            if self.is_punct(index, "}") {
                self.close_block();
                index = index.saturating_add(1);
                continue;
            }
            if self.maybe_item_start(index) {
                match self.parse_item(index)? {
                    Outcome::Next(next) => {
                        index = next.max(index.saturating_add(1));
                        continue;
                    }
                    Outcome::NotItem => {}
                }
            }
            self.scan_token(index)?;
            if self.is_punct(index, "(") || self.is_punct(index, "[") {
                self.bracket_stack.push(self.text(index));
            } else if self.is_punct(index, ")") || self.is_punct(index, "]") {
                let want = if self.is_punct(index, ")") { "(" } else { "[" };
                if self.bracket_stack.last().is_some_and(|top| *top == want) {
                    self.bracket_stack.pop();
                } else {
                    self.extra_close = true;
                }
            }
            if self.is_punct(index, ";") {
                if let Some(depth) = self.const_init_depth {
                    if self.blocks.len() == depth {
                        self.const_init_depth = None;
                    }
                }
            }
            index = index.saturating_add(1);
        }
        if !self.blocks.is_empty() || !self.bracket_stack.is_empty() || self.extra_close {
            let line = self.line(self.toks.len());
            self.push_unbalanced(line)?;
        }
        Ok(())
    }

    fn open_block(&mut self) {
        let pending = self.pending.take();
        let (scope_len_before, kind) = match &pending {
            Some(scope) => {
                let before = self.scopes.len();
                let kind = match scope {
                    Scope::Function { .. } => BlockKind::FunctionBody,
                    Scope::Module { .. } | Scope::Impl { .. } | Scope::Trait { .. } => {
                        BlockKind::Items
                    }
                };
                self.scopes.push(scope.clone());
                (Some(before), kind)
            }
            None => (None, BlockKind::Other),
        };
        self.blocks.push(Block {
            scope_len_before,
            kind,
        });
    }

    fn close_block(&mut self) {
        match self.blocks.pop() {
            Some(block) => {
                if let Some(before) = block.scope_len_before {
                    self.scopes.truncate(before);
                }
            }
            None => {
                self.extra_close = true;
            }
        }
        if let Some(depth) = self.const_init_depth {
            if self.blocks.len() < depth {
                self.const_init_depth = None;
            }
        }
    }

    fn maybe_item_start(&self, index: usize) -> bool {
        if self.in_function() {
            if index == 0 {
                return true;
            }
            match self.toks.get(index.wrapping_sub(1)) {
                None => true,
                Some(prev) => {
                    let text = prev.text(self.src);
                    text == "{" || text == ";" || text == "}"
                }
            }
        } else if let Some(block) = self.blocks.last() {
            block.kind == BlockKind::Items
        } else {
            true
        }
    }

    // Item parsing -----------------------------------------------------------------------------

    fn parse_item(&mut self, start: usize) -> Result<Outcome> {
        let first = self.skip_attributes(start);
        if first >= self.toks.len() {
            return Ok(Outcome::Next(self.toks.len()));
        }
        if let Some(keyword) = self.decl_keyword(first) {
            return self.parse_const_item(first, keyword);
        }
        if self.kind(first) != Some(TokKind::Ident) {
            return Ok(Outcome::NotItem);
        }
        match self.text(first) {
            "pub" | "const" | "async" | "unsafe" | "extern" | "default" | "fn" | "struct"
            | "enum" | "union" | "trait" | "type" | "impl" | "mod" | "use" => {}
            _ => return Ok(Outcome::NotItem),
        }
        // Prefix: visibility and qualifiers.
        let mut index = first;
        let mut is_pub = false;
        let mut is_async = false;
        loop {
            match self.text(index) {
                "pub" => {
                    is_pub = true;
                    index = index.saturating_add(1);
                    if self.is_punct(index, "(") {
                        match self.find_matching(index, "(", ")") {
                            Some(close) => index = close.saturating_add(1),
                            None => {
                                self.push_unbalanced(self.line(index))?;
                                return Ok(Outcome::Next(self.toks.len()));
                            }
                        }
                    }
                }
                "async" => {
                    is_async = true;
                    index = index.saturating_add(1);
                }
                "const" | "unsafe" | "default" => {
                    index = index.saturating_add(1);
                }
                "extern" => {
                    if self.kind(index.saturating_add(1)) == Some(TokKind::Str) {
                        index = index.saturating_add(2);
                    } else if self.is_word(index.saturating_add(1), "crate") {
                        return Ok(Outcome::NotItem);
                    } else {
                        index = index.saturating_add(1);
                    }
                }
                _ => break,
            }
            if index >= self.toks.len() {
                return Ok(Outcome::Next(self.toks.len()));
            }
        }
        if self.kind(index) != Some(TokKind::Ident) {
            return Ok(Outcome::NotItem);
        }
        match self.text(index) {
            "fn" => self.parse_fn(first, index, is_pub, is_async),
            "struct" => self.parse_type(first, index, TypeKind::Struct),
            "enum" => self.parse_type(first, index, TypeKind::Enum),
            "union" => self.parse_type(first, index, TypeKind::Union),
            "trait" => self.parse_type(first, index, TypeKind::Trait),
            // `type X = ...;` inside an `impl` or `trait` is an associated type, not a module
            // level type alias.
            "type"
                if matches!(
                    self.scopes.last(),
                    Some(Scope::Impl { .. } | Scope::Trait { .. })
                ) =>
            {
                Ok(Outcome::NotItem)
            }
            "type" => self.parse_type(first, index, TypeKind::TypeAlias),
            "impl" => self.parse_impl(first, index),
            "mod" => self.parse_mod(first, index),
            "use" => self.parse_use(index),
            _ => Ok(Outcome::NotItem),
        }
    }

    /// Index of a `const`/`static` declaration keyword after visibility/qualifiers, if any.
    fn decl_keyword(&self, first: usize) -> Option<usize> {
        let mut index = first;
        loop {
            match self.text(index) {
                "pub" => {
                    index = index.checked_add(1)?;
                    if self.is_punct(index, "(") {
                        index = self.find_matching(index, "(", ")")?.checked_add(1)?;
                    }
                }
                "async" | "unsafe" | "default" => {
                    index = index.checked_add(1)?;
                }
                "extern" => {
                    if self.kind(index.checked_add(1)?) == Some(TokKind::Str) {
                        index = index.checked_add(2)?;
                    } else {
                        return None;
                    }
                }
                _ => break,
            }
        }
        if (self.is_word(index, "const") || self.is_word(index, "static")) && self.is_decl_at(index)
        {
            return Some(index);
        }
        None
    }

    fn is_decl_at(&self, keyword: usize) -> bool {
        let mut name = keyword.saturating_add(1);
        if self.is_word(keyword, "static") && self.is_word(name, "mut") {
            name = name.saturating_add(1);
        }
        if self.kind(name) != Some(TokKind::Ident) {
            return false;
        }
        let after = name.saturating_add(1);
        self.is_punct(after, ":") || self.is_punct(after, "=")
    }

    fn parse_const_item(&mut self, first: usize, keyword: usize) -> Result<Outcome> {
        self.push_const_def(first, keyword)?;
        let mut brace: usize = 0;
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut index = keyword;
        while index < self.toks.len() {
            let text = self.text(index);
            match text {
                "{" => brace = brace.saturating_add(1),
                "(" => paren = paren.saturating_add(1),
                "[" => bracket = bracket.saturating_add(1),
                "}" => {
                    if brace == 0 {
                        return Ok(Outcome::Next(index));
                    }
                    brace -= 1;
                }
                ")" => paren = paren.saturating_sub(1),
                "]" => bracket = bracket.saturating_sub(1),
                "=" => {
                    if brace == 0 && paren == 0 && bracket == 0 {
                        self.const_init_depth = Some(self.blocks.len());
                        return Ok(Outcome::Next(index));
                    }
                }
                ";" if brace == 0 && paren == 0 && bracket == 0 => {
                    return Ok(Outcome::Next(index.saturating_add(1)));
                }
                _ => {}
            }
            index = index.saturating_add(1);
        }
        Ok(Outcome::Next(self.toks.len()))
    }

    /// A `const_def` fact for the `const` or `static` item whose keyword is at `keyword`.
    /// An item without its terminating `;` gives no fact.
    fn push_const_def(&mut self, first: usize, keyword: usize) -> Result<()> {
        let is_static = self.is_word(keyword, "static");
        let mut name_index = keyword.saturating_add(1);
        if is_static && self.is_word(name_index, "mut") {
            name_index = name_index.saturating_add(1);
        }
        let Some(name) = self.ident_name(name_index) else {
            return Ok(());
        };
        let Some(end) = self.item_semicolon(keyword) else {
            return Ok(());
        };
        let (key, _, _) = self.fn_key(&name);
        self.facts.push(Fact::new(
            &self.file.path,
            self.line(keyword),
            key,
            Confidence::Exact,
            &ConstDefData {
                name,
                is_static,
                end_line: self.line(end),
                hash: self.hash_range(first, end),
            },
        )?);
        Ok(())
    }

    /// The `;` that ends the item starting at `from`, outside brackets; `None` when a closing
    /// bracket of an enclosing block or the end of the file comes first.
    fn item_semicolon(&self, from: usize) -> Option<usize> {
        let mut depth: usize = 0;
        let mut index = from;
        while index < self.toks.len() {
            match self.text(index) {
                "{" | "(" | "[" => depth = depth.saturating_add(1),
                "}" | ")" | "]" => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                }
                ";" if depth == 0 => return Some(index),
                _ => {}
            }
            index = index.saturating_add(1);
        }
        None
    }

    fn parse_fn(
        &mut self,
        first: usize,
        keyword: usize,
        is_pub: bool,
        is_async: bool,
    ) -> Result<Outcome> {
        let line = self.line(keyword);
        let name_index = keyword.saturating_add(1);
        let Some(name) = self.ident_name(name_index) else {
            self.push_unparsed(line, "expected function name")?;
            return Ok(Outcome::Next(self.skip_malformed(name_index)));
        };
        let mut index = name_index.saturating_add(1);
        if self.is_punct(index, "<") {
            match self.find_angle_end(index) {
                Some(next) => index = next,
                None => {
                    self.push_unbalanced(line)?;
                    return Ok(Outcome::Next(self.toks.len()));
                }
            }
        }
        if !self.is_punct(index, "(") {
            self.push_unparsed(line, "expected function parameters")?;
            return Ok(Outcome::Next(self.skip_malformed(index)));
        }
        let params_close = match self.find_matching(index, "(", ")") {
            Some(close) => close,
            None => {
                self.push_unbalanced(line)?;
                return Ok(Outcome::Next(self.toks.len()));
            }
        };
        // Scan the return type and where clause for the body or the terminating `;`.
        let mut body: Option<usize> = None;
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut scan = params_close.saturating_add(1);
        while scan < self.toks.len() {
            let text = self.text(scan);
            match text {
                "(" => paren = paren.saturating_add(1),
                "[" => bracket = bracket.saturating_add(1),
                ")" => paren = paren.saturating_sub(1),
                "]" => bracket = bracket.saturating_sub(1),
                "{" => {
                    if paren == 0 && bracket == 0 {
                        body = Some(scan);
                        break;
                    }
                }
                ";" => {
                    if paren == 0 && bracket == 0 {
                        // A bodiless declaration (trait or extern): no fact.
                        return Ok(Outcome::Next(scan.saturating_add(1)));
                    }
                }
                "}" if paren == 0 && bracket == 0 => {
                    self.push_unparsed(line, "expected function body")?;
                    return Ok(Outcome::Next(scan));
                }
                _ => {}
            }
            scan = scan.saturating_add(1);
        }
        let Some(body_start) = body else {
            self.push_unparsed(line, "expected function body")?;
            return Ok(Outcome::Next(self.toks.len()));
        };
        let body_close = match self.find_matching(body_start, "{", "}") {
            Some(close) => close,
            None => {
                self.push_unbalanced(self.line(body_start))?;
                return Ok(Outcome::Next(self.toks.len()));
            }
        };
        let (key, owner_type, trait_name) = self.fn_key(&name);
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            key.clone(),
            Confidence::Exact,
            &FnDefData {
                name,
                owner_type,
                trait_name,
                is_pub,
                is_async,
                end_line: self.line(body_close),
                hash: self.hash_range(first, body_close),
            },
        )?);
        self.pending = Some(Scope::Function { key });
        Ok(Outcome::Next(body_start))
    }

    /// Key, owner type and trait of a function named `name`, from the innermost scope.
    fn fn_key(&self, name: &str) -> (String, Option<String>, Option<String>) {
        match self.scopes.last() {
            Some(Scope::Function { key }) => (format!("{key}::{name}"), None, None),
            Some(Scope::Impl {
                self_type,
                trait_name,
            }) => {
                let key = format!("{}::{self_type}::{name}", self.module_key());
                (key, Some(self_type.clone()), trait_name.clone())
            }
            Some(Scope::Trait { name: trait_name }) => {
                let key = format!("{}::{trait_name}::{name}", self.module_key());
                (key, None, Some(trait_name.clone()))
            }
            Some(Scope::Module { .. }) | None => {
                (format!("{}::{name}", self.module_key()), None, None)
            }
        }
    }

    fn parse_type(&mut self, first: usize, keyword: usize, kind: TypeKind) -> Result<Outcome> {
        let line = self.line(keyword);
        let name_index = keyword.saturating_add(1);
        let Some(name) = self.ident_name(name_index) else {
            self.push_unparsed(line, "expected type name")?;
            return Ok(Outcome::Next(self.skip_malformed(name_index)));
        };
        let key = format!("{}::{name}", self.module_key());
        if kind == TypeKind::TypeAlias {
            return self.finish_semi_type(first, name_index, name, key, kind, line);
        }
        // Find the body `{`, the terminating `;`, or a tuple body `(` outside generics.
        let mut angle: usize = 0;
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut scan = name_index.saturating_add(1);
        while scan < self.toks.len() {
            let text = self.text(scan);
            match text {
                "<" => angle = angle.saturating_add(1),
                ">" => angle = angle.saturating_sub(1),
                "(" => {
                    if angle == 0 && paren == 0 && bracket == 0 && kind != TypeKind::Trait {
                        return self.finish_tuple_type(first, scan, name, key, kind, line);
                    }
                    paren = paren.saturating_add(1);
                }
                "[" => bracket = bracket.saturating_add(1),
                ")" => paren = paren.saturating_sub(1),
                "]" => bracket = bracket.saturating_sub(1),
                "{" => {
                    if angle == 0 && paren == 0 && bracket == 0 {
                        return self.finish_body_type(first, scan, name, key, kind, line);
                    }
                }
                ";" => {
                    if angle == 0 && paren == 0 && bracket == 0 {
                        self.push_type_fact(first, scan, &name, &key, kind, line)?;
                        return Ok(Outcome::Next(scan.saturating_add(1)));
                    }
                }
                "}" if angle == 0 && paren == 0 && bracket == 0 => {
                    self.push_unparsed(line, "expected type body")?;
                    return Ok(Outcome::Next(scan));
                }
                _ => {}
            }
            scan = scan.saturating_add(1);
        }
        self.push_unparsed(line, "expected type body")?;
        Ok(Outcome::Next(self.toks.len()))
    }

    fn finish_semi_type(
        &mut self,
        first: usize,
        name_index: usize,
        name: String,
        key: String,
        kind: TypeKind,
        line: u32,
    ) -> Result<Outcome> {
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut brace: usize = 0;
        let mut scan = name_index.saturating_add(1);
        while scan < self.toks.len() {
            match self.text(scan) {
                "(" => paren = paren.saturating_add(1),
                "[" => bracket = bracket.saturating_add(1),
                "{" => brace = brace.saturating_add(1),
                ")" => paren = paren.saturating_sub(1),
                "]" => bracket = bracket.saturating_sub(1),
                "}" => brace = brace.saturating_sub(1),
                ";" if paren == 0 && bracket == 0 && brace == 0 => {
                    self.push_type_fact(first, scan, &name, &key, kind, line)?;
                    return Ok(Outcome::Next(scan.saturating_add(1)));
                }
                _ => {}
            }
            scan = scan.saturating_add(1);
        }
        self.push_unparsed(line, "expected ; after type alias")?;
        Ok(Outcome::Next(self.toks.len()))
    }

    fn finish_tuple_type(
        &mut self,
        first: usize,
        open: usize,
        name: String,
        key: String,
        kind: TypeKind,
        line: u32,
    ) -> Result<Outcome> {
        let close = match self.find_matching(open, "(", ")") {
            Some(close) => close,
            None => {
                self.push_unbalanced(line)?;
                return Ok(Outcome::Next(self.toks.len()));
            }
        };
        // A tuple type may carry a where clause before its terminating `;`.
        let mut end = close;
        let mut scan = close.saturating_add(1);
        while scan < self.toks.len() {
            if self.is_punct(scan, ";") {
                end = scan;
                break;
            }
            if self.is_punct(scan, "{") || self.is_punct(scan, "}") {
                break;
            }
            scan = scan.saturating_add(1);
        }
        self.push_type_fact(first, end, &name, &key, kind, line)?;
        Ok(Outcome::Next(end.saturating_add(1)))
    }

    fn finish_body_type(
        &mut self,
        first: usize,
        body_start: usize,
        name: String,
        key: String,
        kind: TypeKind,
        line: u32,
    ) -> Result<Outcome> {
        let close = match self.find_matching(body_start, "{", "}") {
            Some(close) => close,
            None => {
                self.push_unbalanced(self.line(body_start))?;
                return Ok(Outcome::Next(self.toks.len()));
            }
        };
        self.push_type_fact(first, close, &name, &key, kind, line)?;
        if kind == TypeKind::Trait {
            self.pending = Some(Scope::Trait { name });
        }
        Ok(Outcome::Next(body_start))
    }

    fn push_type_fact(
        &mut self,
        first: usize,
        end: usize,
        name: &str,
        key: &str,
        kind: TypeKind,
        line: u32,
    ) -> Result<()> {
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            key,
            Confidence::Exact,
            &TypeDefData {
                name: name.to_owned(),
                type_kind: kind,
                end_line: self.line(end),
                hash: self.hash_range(first, end),
            },
        )?);
        Ok(())
    }

    fn parse_impl(&mut self, _first: usize, keyword: usize) -> Result<Outcome> {
        let line = self.line(keyword);
        let mut index = keyword.saturating_add(1);
        if self.is_punct(index, "<") {
            match self.find_angle_end(index) {
                Some(next) => index = next,
                None => {
                    self.push_unbalanced(line)?;
                    return Ok(Outcome::Next(self.toks.len()));
                }
            }
        }
        let header_start = index;
        let mut angle: usize = 0;
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut body: Option<usize> = None;
        let mut scan = index;
        while scan < self.toks.len() {
            match self.text(scan) {
                "<" => angle = angle.saturating_add(1),
                ">" => angle = angle.saturating_sub(1),
                "(" => paren = paren.saturating_add(1),
                "[" => bracket = bracket.saturating_add(1),
                ")" => paren = paren.saturating_sub(1),
                "]" => bracket = bracket.saturating_sub(1),
                "{" => {
                    if angle == 0 && paren == 0 && bracket == 0 {
                        body = Some(scan);
                        break;
                    }
                }
                ";" if angle == 0 && paren == 0 && bracket == 0 => {
                    self.push_unparsed(line, "expected impl body")?;
                    return Ok(Outcome::Next(scan.saturating_add(1)));
                }
                _ => {}
            }
            scan = scan.saturating_add(1);
        }
        let Some(body_start) = body else {
            self.push_unparsed(line, "expected impl body")?;
            return Ok(Outcome::Next(self.toks.len()));
        };
        let mut header: Vec<usize> = Vec::new();
        let mut cursor = header_start;
        while cursor < body_start {
            header.push(cursor);
            cursor = cursor.saturating_add(1);
        }
        if header.is_empty() {
            self.push_unparsed(line, "expected impl type")?;
            return Ok(Outcome::Next(
                self.skip_malformed(keyword.saturating_add(1)),
            ));
        }
        let body_close = match self.find_matching(body_start, "{", "}") {
            Some(close) => close,
            None => {
                self.push_unbalanced(self.line(body_start))?;
                return Ok(Outcome::Next(self.toks.len()));
            }
        };
        let for_pos = self.find_for_keyword(&header);
        let (trait_name, self_part): (Option<String>, &[usize]) = match for_pos {
            Some(pos) => {
                let (trait_part, rest) = header.split_at(pos);
                let self_part = match rest.get(1..) {
                    Some(part) => part,
                    None => &[],
                };
                (self.path_last(trait_part), self_part)
            }
            None => (None, &header),
        };
        if for_pos.is_some() && trait_name.is_none() {
            self.push_unparsed(line, "expected trait name in impl")?;
            return Ok(Outcome::Next(
                self.skip_malformed(keyword.saturating_add(1)),
            ));
        }
        let Some(self_type) = self.self_type_name(self_part) else {
            self.push_unparsed(line, "expected impl type")?;
            return Ok(Outcome::Next(
                self.skip_malformed(keyword.saturating_add(1)),
            ));
        };
        let module = self.module_key();
        let key = match &trait_name {
            Some(trait_name) => format!("{module}::impl {trait_name} for {self_type}"),
            None => format!("{module}::impl {self_type}"),
        };
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            key,
            Confidence::Exact,
            &ImplData {
                self_type: self_type.clone(),
                trait_name: trait_name.clone(),
                end_line: self.line(body_close),
            },
        )?);
        self.pending = Some(Scope::Impl {
            self_type,
            trait_name,
        });
        Ok(Outcome::Next(body_start))
    }

    fn find_for_keyword(&self, header: &[usize]) -> Option<usize> {
        let mut angle: usize = 0;
        for (pos, token_index) in header.iter().enumerate() {
            match self.text(*token_index) {
                "<" => angle = angle.saturating_add(1),
                ">" => angle = angle.saturating_sub(1),
                _ => {
                    if angle == 0 && self.is_word(*token_index, "for") {
                        return Some(pos);
                    }
                }
            }
        }
        None
    }

    /// Last path segment before generics in a token range, skipping a leading `!`.
    fn path_last(&self, part: &[usize]) -> Option<String> {
        let mut start: usize = 0;
        if part.first().is_some_and(|index| self.is_punct(*index, "!")) {
            start = 1;
        }
        let mut last: Option<String> = None;
        let mut pos = start;
        while let Some(token_index) = part.get(pos) {
            if self.is_punct(*token_index, "<") {
                break;
            }
            let is_segment = self.kind(*token_index) == Some(TokKind::Ident)
                && (pos == start
                    || part
                        .get(pos.wrapping_sub(1))
                        .is_some_and(|prev| self.is_punct(*prev, "::")));
            if is_segment {
                last = self.ident_name(*token_index);
            }
            pos = pos.saturating_add(1);
        }
        last
    }

    fn type_text(&self, part: &[usize]) -> String {
        let mut out = String::new();
        for token_index in part {
            out.push_str(self.text(*token_index));
        }
        out
    }

    fn self_type_name(&self, part: &[usize]) -> Option<String> {
        let mut start: usize = 0;
        loop {
            let token_index = part.get(start)?;
            let text = self.text(*token_index);
            if text == "&"
                || self.kind(*token_index) == Some(TokKind::Lifetime)
                || text == "mut"
                || text == "dyn"
            {
                start = start.checked_add(1)?;
            } else {
                break;
            }
        }
        let rest = part.get(start..)?;
        let first = rest.first()?;
        let special = self.is_punct(*first, "(")
            || self.is_punct(*first, "[")
            || self.is_punct(*first, "*")
            || self.is_punct(*first, "<")
            || self.is_word(*first, "fn")
            || self.is_word(*first, "extern")
            || self.is_word(*first, "unsafe");
        if special {
            let text = self.type_text(rest);
            if text.is_empty() { None } else { Some(text) }
        } else {
            self.path_last(rest).or_else(|| {
                let text = self.type_text(rest);
                if text.is_empty() { None } else { Some(text) }
            })
        }
    }

    fn parse_mod(&mut self, _first: usize, keyword: usize) -> Result<Outcome> {
        let line = self.line(keyword);
        let name_index = keyword.saturating_add(1);
        let Some(name) = self.ident_name(name_index) else {
            self.push_unparsed(line, "expected module name")?;
            return Ok(Outcome::Next(self.skip_malformed(name_index)));
        };
        let after = name_index.saturating_add(1);
        if self.is_punct(after, ";") {
            return Ok(Outcome::Next(after.saturating_add(1)));
        }
        if self.is_punct(after, "{") {
            let key = format!("{}::{name}", self.module_key());
            self.facts.push(Fact::new(
                &self.file.path,
                0,
                key.clone(),
                Confidence::Exact,
                &ModuleData { name: name.clone() },
            )?);
            self.pending = Some(Scope::Module { key });
            return Ok(Outcome::Next(after));
        }
        self.push_unparsed(line, "expected module body or ;")?;
        Ok(Outcome::Next(self.skip_malformed(after)))
    }

    fn parse_use(&mut self, keyword: usize) -> Result<Outcome> {
        let line = self.line(keyword);
        let owner = self.module_key();
        let mut prefixes: Vec<Vec<String>> = vec![Vec::new()];
        let mut current: Vec<String> = Vec::new();
        let mut alias: Option<String> = None;
        let mut alias_next = false;
        let mut index = keyword.saturating_add(1);
        while index < self.toks.len() {
            let kind = self.kind(index);
            let text = self.text(index);
            if alias_next {
                alias_next = false;
                match self.ident_name(index) {
                    Some(name) => alias = Some(name),
                    None => {
                        self.push_unparsed(line, "expected import alias")?;
                        return Ok(Outcome::Next(self.skip_malformed(index)));
                    }
                }
                index = index.saturating_add(1);
                continue;
            }
            if kind == Some(TokKind::Ident) {
                if text == "as" {
                    alias_next = true;
                } else {
                    match self.ident_name(index) {
                        Some(name) => current.push(name),
                        None => current.push(text.to_owned()),
                    }
                }
            } else if kind == Some(TokKind::Punct) {
                match text {
                    "::" => {}
                    "*" => current.push(String::from("*")),
                    "," => {
                        self.emit_use(&prefixes, &current, &alias, &owner, line)?;
                        current = Vec::new();
                        alias = None;
                    }
                    "{" => {
                        let mut prefix = match prefixes.last() {
                            Some(top) => top.clone(),
                            None => Vec::new(),
                        };
                        prefix.extend(current.iter().cloned());
                        prefixes.push(prefix);
                        current = Vec::new();
                        alias = None;
                    }
                    "}" => {
                        if prefixes.len() <= 1 {
                            self.push_unparsed(line, "expected ; after use")?;
                            return Ok(Outcome::Next(index));
                        }
                        self.emit_use(&prefixes, &current, &alias, &owner, line)?;
                        prefixes.pop();
                        current = Vec::new();
                        alias = None;
                    }
                    ";" => {
                        self.emit_use(&prefixes, &current, &alias, &owner, line)?;
                        return Ok(Outcome::Next(index.saturating_add(1)));
                    }
                    _ => {
                        self.push_unparsed(line, "unexpected token in use")?;
                        return Ok(Outcome::Next(self.skip_malformed(index)));
                    }
                }
            } else {
                self.push_unparsed(line, "unexpected token in use")?;
                return Ok(Outcome::Next(self.skip_malformed(index)));
            }
            index = index.saturating_add(1);
        }
        self.push_unparsed(line, "expected ; after use")?;
        Ok(Outcome::Next(self.toks.len()))
    }

    fn emit_use(
        &mut self,
        prefixes: &[Vec<String>],
        current: &[String],
        alias: &Option<String>,
        owner: &str,
        line: u32,
    ) -> Result<()> {
        if current.is_empty() {
            return Ok(());
        }
        let mut full: Vec<String> = match prefixes.last() {
            Some(top) => top.clone(),
            None => Vec::new(),
        };
        full.extend(current.iter().cloned());
        if full.last().is_some_and(|last| last == "self") {
            full.pop();
        }
        let Some((name, path_parts)) = full.split_last() else {
            return Ok(());
        };
        let key = full.join("::");
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            key,
            Confidence::Exact,
            &ImportData {
                owner: owner.to_owned(),
                path: path_parts.join("::"),
                name: name.clone(),
                alias: alias.clone(),
            },
        )?);
        Ok(())
    }

    // Calls, routes, environment variables and SQL ----------------------------------------------

    fn scan_token(&mut self, index: usize) -> Result<()> {
        match self.kind(index) {
            Some(TokKind::Str) => {
                if self.in_function() {
                    self.scan_sql_string(index)?;
                }
            }
            Some(TokKind::Ident) => {
                if self.callable_context() {
                    if let Some(info) = self.parse_call_start(index) {
                        self.emit_call(&info)?;
                    }
                }
            }
            Some(TokKind::Punct) if self.text(index) == "." && self.callable_context() => {
                if let Some(info) = self.parse_method_after_dot(index) {
                    self.emit_call(&info)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn parse_call_start(&self, start: usize) -> Option<CallInfo> {
        if start > 0 {
            let prev = start.wrapping_sub(1);
            if self.is_punct(prev, ".") {
                return None;
            }
            if self.is_punct(prev, "::") {
                // A leading path separator starts a path; a segment of a path parsed
                // from its first segment was already handled there. A path after a
                // qualified start such as `<T as Trait>::m` is read from `m`.
                let before = prev.wrapping_sub(1);
                let leading = start < 2
                    || (self.kind(before) != Some(TokKind::Ident)
                        && (!self.is_punct(before, ">") || self.closes_qualified_path(before)));
                if !leading {
                    return None;
                }
            }
        }
        let first = self.ident_name(start)?;
        let mut segments: Vec<String> = vec![first];
        let mut separators: Vec<String> = Vec::new();
        let mut index = start.checked_add(1)?;
        loop {
            if self.is_punct(index, "::") {
                let next = index.checked_add(1)?;
                if self.is_punct(next, "<") {
                    // Turbofish on a middle segment: skip the generic arguments and
                    // keep reading path segments when another segment follows.
                    match self.find_angle_end(next) {
                        Some(after) => {
                            let continues = self.is_punct(after, "::")
                                && self.ident_name(after.checked_add(1)?).is_some();
                            if continues {
                                index = after;
                                continue;
                            }
                            break;
                        }
                        None => break,
                    }
                }
                match self.ident_name(next) {
                    Some(name) => {
                        segments.push(name);
                        separators.push(String::from("::"));
                        index = next.checked_add(1)?;
                    }
                    None => break,
                }
            } else if self.is_punct(index, ".") {
                let next = index.checked_add(1)?;
                match self.ident_name(next) {
                    // `x.await.f()` is not a path: the call after `.await` is read once, by
                    // `parse_method_after_dot`.
                    Some(name) if name == "await" => break,
                    Some(name) => {
                        segments.push(name);
                        separators.push(String::from("."));
                        index = next.checked_add(1)?;
                    }
                    None => break,
                }
            } else {
                break;
            }
        }
        // Turbofish or direct generic arguments are dropped from the callee.
        if self.is_punct(index, "::") && self.is_punct(index.checked_add(1)?, "<") {
            index = self.find_angle_end(index.checked_add(1)?)?;
        } else if self.is_punct(index, "<") {
            if let Some(next) = self.find_angle_end(index) {
                if self.is_punct(next, "(") {
                    index = next;
                }
            }
        }
        let is_method = separators.last().is_some_and(|sep| sep == ".");
        if self.is_punct(index, "!") {
            let open = index.checked_add(1)?;
            let (open_text, close_text) = match self.text(open) {
                "(" => ("(", ")"),
                "[" => ("[", "]"),
                "{" => ("{", "}"),
                _ => return None,
            };
            let close = self.find_matching(open, open_text, close_text);
            return Some(self.make_call_info(start, segments, is_method, true, Some(open), close));
        }
        if self.is_punct(index, "(") {
            let close = self.find_matching(index, "(", ")");
            return Some(self.make_call_info(
                start,
                segments,
                is_method,
                false,
                Some(index),
                close,
            ));
        }
        None
    }

    fn parse_method_after_dot(&self, dot: usize) -> Option<CallInfo> {
        let prev = dot.checked_sub(1)?;
        let receiver_none = if self.is_punct(prev, ")")
            || self.is_punct(prev, "]")
            || self.is_punct(prev, "}")
            || self.is_punct(prev, "?")
            || matches!(
                self.kind(prev),
                Some(TokKind::Number) | Some(TokKind::Str) | Some(TokKind::Char)
            ) {
            true
        } else if self.is_word(prev, "await") {
            let before = prev.checked_sub(1)?;
            self.is_punct(before, ".")
        } else {
            false
        };
        if !receiver_none {
            return None;
        }
        let name_index = dot.checked_add(1)?;
        let name = self.ident_name(name_index)?;
        let mut index = name_index.checked_add(1)?;
        if self.is_punct(index, "::") && self.is_punct(index.checked_add(1)?, "<") {
            index = self.find_angle_end(index.checked_add(1)?)?;
        } else if self.is_punct(index, "<") {
            if let Some(next) = self.find_angle_end(index) {
                if self.is_punct(next, "(") {
                    index = next;
                }
            }
        }
        if !self.is_punct(index, "(") {
            return None;
        }
        let close = self.find_matching(index, "(", ")");
        Some(CallInfo {
            start: name_index,
            name: name.clone(),
            callee: name,
            qualifier: Vec::new(),
            receiver: None,
            is_method: true,
            is_macro: false,
            open: Some(index),
            close,
            segments: Vec::new(),
        })
    }

    fn make_call_info(
        &self,
        start: usize,
        segments: Vec<String>,
        is_method: bool,
        is_macro: bool,
        open: Option<usize>,
        close: Option<usize>,
    ) -> CallInfo {
        let name = match segments.last() {
            Some(last) => last.clone(),
            None => String::new(),
        };
        let (callee, qualifier, receiver) = if is_method {
            let receiver = if segments.len() == 2 {
                segments.first().cloned()
            } else {
                None
            };
            (name.clone(), Vec::new(), receiver)
        } else if segments.len() > 1 {
            let qualifier: Vec<String> = segments
                .iter()
                .take(segments.len().saturating_sub(1))
                .cloned()
                .collect();
            (segments.join("::"), qualifier, None)
        } else {
            (name.clone(), Vec::new(), None)
        };
        CallInfo {
            start,
            name,
            callee,
            qualifier,
            receiver,
            is_method,
            is_macro,
            open,
            close,
            segments,
        }
    }

    fn skipped_callee(info: &CallInfo) -> bool {
        if SKIP_CALLEES.contains(&info.name.as_str()) {
            return true;
        }
        if info.qualifier.is_empty() && !info.is_method {
            if SKIP_BARE_EXTRA.contains(&info.name.as_str()) {
                return true;
            }
            // Bare tuple-struct constructors and enum variants are not calls.
            if !info.is_macro && info.name.chars().next().is_some_and(|ch| ch.is_uppercase()) {
                return true;
            }
        }
        false
    }

    fn emit_call(&mut self, info: &CallInfo) -> Result<()> {
        if Self::skipped_callee(info) {
            return Ok(());
        }
        let owner = self.owner_key();
        let line = self.line(info.start);
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            info.callee.clone(),
            Confidence::Exact,
            &CallData {
                owner: owner.clone(),
                callee: info.callee.clone(),
                name: info.name.clone(),
                qualifier: info.qualifier.clone(),
                receiver: info.receiver.clone(),
                is_method: info.is_method,
                is_macro: info.is_macro,
            },
        )?);
        self.check_env(info, &owner, line)?;
        self.check_routes(info, &owner, line)?;
        Ok(())
    }

    fn arg_ranges(&self, open: usize, close: usize) -> Vec<(usize, usize)> {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut paren: usize = 0;
        let mut bracket: usize = 0;
        let mut brace: usize = 0;
        let mut start = open.saturating_add(1);
        let mut index = start;
        while index < close {
            match self.text(index) {
                "(" => paren = paren.saturating_add(1),
                "[" => bracket = bracket.saturating_add(1),
                "{" => brace = brace.saturating_add(1),
                ")" => paren = paren.saturating_sub(1),
                "]" => bracket = bracket.saturating_sub(1),
                "}" => brace = brace.saturating_sub(1),
                "," if paren == 0 && bracket == 0 && brace == 0 => {
                    ranges.push((start, index));
                    start = index.saturating_add(1);
                }
                _ => {}
            }
            index = index.saturating_add(1);
        }
        ranges.push((start, close));
        ranges
    }

    fn arg_string(&self, info: &CallInfo, arg: usize) -> Option<String> {
        let open = info.open?;
        let close = info.close?;
        let ranges = self.arg_ranges(open, close);
        let (start, end) = ranges.get(arg).copied()?;
        if end == start.saturating_add(1) && self.kind(start) == Some(TokKind::Str) {
            crate::lex::rust::unquote(self.text(start))
        } else {
            None
        }
    }

    fn range_text_no_spaces(&self, range: (usize, usize)) -> Option<String> {
        let (start, end) = range;
        if start >= end {
            return None;
        }
        let mut out = String::new();
        let mut index = start;
        while index < end {
            out.push_str(self.text(index));
            index = index.saturating_add(1);
        }
        if out.is_empty() { None } else { Some(out) }
    }

    fn check_env(&mut self, info: &CallInfo, owner: &str, line: u32) -> Result<()> {
        let wanted = if info.is_macro {
            info.segments.len() == 1 && (info.name == "env" || info.name == "option_env")
        } else {
            matches!(
                info.callee.as_str(),
                "env::var" | "std::env::var" | "env::var_os" | "std::env::var_os"
            )
        };
        if !wanted {
            return Ok(());
        }
        let Some(value) = self.arg_string(info, 0) else {
            return Ok(());
        };
        if value.is_empty() {
            return Ok(());
        }
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            value,
            Confidence::Exact,
            &EnvVarData {
                owner: owner.to_owned(),
            },
        )?);
        Ok(())
    }

    fn check_routes(&mut self, info: &CallInfo, owner: &str, line: u32) -> Result<()> {
        if info.is_macro {
            return Ok(());
        }
        // The configuration outlives the extractor, so its patterns are borrowed, not cloned.
        let config: &'a Config = self.config;
        for pattern in &config.routes {
            self.check_wrapped_route(pattern, info, owner, line)?;
        }
        Ok(())
    }

    fn check_wrapped_route(
        &mut self,
        pattern: &crate::config::RoutePattern,
        info: &CallInfo,
        owner: &str,
        line: u32,
    ) -> Result<()> {
        if info.name != pattern.call {
            return Ok(());
        }
        let Some(path) = self.arg_string(info, pattern.path_arg) else {
            return Ok(());
        };
        let (Some(open), Some(close)) = (info.open, info.close) else {
            return Ok(());
        };
        let ranges = self.arg_ranges(open, close);
        for (arg_index, range) in ranges.iter().enumerate() {
            if arg_index == pattern.path_arg {
                continue;
            }
            let (start, end) = *range;
            let mut index = start;
            while index < end {
                if self.kind(index) == Some(TokKind::Ident) {
                    if let Some(method) = self.ident_name(index).and_then(|name| {
                        pattern
                            .methods
                            .get(&name)
                            .map(|method| (name, method.clone()))
                    }) {
                        let (_wrapper, http_method) = method;
                        let mut after = index.saturating_add(1);
                        if self.is_punct(after, "::") && self.is_punct(after.saturating_add(1), "<")
                        {
                            if let Some(next) = self.find_angle_end(after.saturating_add(1)) {
                                after = next;
                            }
                        }
                        if self.is_punct(after, "(") {
                            if let Some(wrapper_close) = self.find_matching(after, "(", ")") {
                                let inner = self.arg_ranges(after, wrapper_close);
                                if let Some(first_range) = inner.first().copied() {
                                    if let Some(handler) = self.range_text_no_spaces(first_range) {
                                        self.push_route(
                                            pattern,
                                            &http_method,
                                            &path,
                                            Some(handler),
                                            owner,
                                            line,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
                index = index.saturating_add(1);
            }
        }
        Ok(())
    }

    fn push_route(
        &mut self,
        pattern: &crate::config::RoutePattern,
        method: &str,
        path: &str,
        handler: Option<String>,
        owner: &str,
        line: u32,
    ) -> Result<()> {
        let with_prefix = match &pattern.prefix {
            Some(prefix) => format!("{prefix}{path}"),
            None => path.to_owned(),
        };
        let normalized = match &pattern.base_path {
            Some(base) => norm_path_in(base, &with_prefix),
            None => norm_path(&with_prefix),
        };
        let key = format!("{method} {normalized}");
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            key,
            Confidence::Heuristic,
            &RouteData {
                method: method.to_owned(),
                path: path.to_owned(),
                norm_path: normalized,
                handler,
                pattern: pattern.id.clone(),
                owner: owner.to_owned(),
            },
        )?);
        Ok(())
    }

    fn scan_sql_string(&mut self, index: usize) -> Result<()> {
        let Some(value) = crate::lex::rust::unquote(self.text(index)) else {
            return Ok(());
        };
        if !crate::extract::sql::looks_like_sql(&value) {
            return Ok(());
        }
        let Some(owner) = self.function_key() else {
            return Ok(());
        };
        let line = self.line(index);
        let preview = crate::extract::sql::sql_preview(&value);
        self.facts.push(Fact::new(
            &self.file.path,
            line,
            owner.clone(),
            Confidence::Heuristic,
            &SqlInCodeData {
                owner: owner.clone(),
                preview,
            },
        )?);
        for (table, op) in crate::extract::sql::sql_text_accesses(&value) {
            self.facts.push(Fact::new(
                &self.file.path,
                line,
                table.clone(),
                Confidence::Heuristic,
                &TableAccessData {
                    owner: owner.clone(),
                    table,
                    op,
                },
            )?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::extract::test_support::{calls, facts, keys};
    use crate::model::FactKind;

    #[test]
    fn associated_types_are_not_module_types() {
        let facts = facts(
            "src/lib.rs",
            "pub trait Source { type Item; fn next(&mut self); }\npub struct Numbers;\nimpl Source for Numbers {\n    type Item = u8;\n    fn next(&mut self) {}\n}\n",
        );
        let types = keys(&facts, FactKind::TypeDef);
        assert!(
            !types.iter().any(|key| key.ends_with("::Item")),
            "{types:?}"
        );
        assert!(types.contains(&"app::Source".to_owned()), "{types:?}");
    }

    #[test]
    fn calls_after_await_and_qualified_self() {
        let facts = facts(
            "src/lib.rs",
            "async fn load(client: Client) {\n    client.send().await.json();\n    <Numbers as Source>::make();\n}\n",
        );
        let names: Vec<String> = calls(&facts).into_iter().map(|(_, name)| name).collect();
        for expected in ["send", "json", "make"] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} in {names:?}"
            );
        }
        assert!(!names.iter().any(|name| name == "await"), "{names:?}");
    }

    #[test]
    fn sql_preview_in_code_masks_literals() {
        let facts = facts(
            "src/lib.rs",
            "fn save(db: &Db) {\n    db.execute(\"INSERT INTO users (name, secret) VALUES ('ann', 'hunter2')\");\n}\n",
        );
        let sql: Vec<String> = facts
            .iter()
            .filter(|fact| fact.kind == FactKind::SqlInCode)
            .map(|fact| fact.data_text("preview").unwrap_or_default())
            .collect();
        assert_eq!(sql.len(), 1, "{facts:?}");
        assert!(!sql[0].contains("hunter2"), "{sql:?}");
        assert!(keys(&facts, FactKind::TableAccess).contains(&"users".to_owned()));
    }
}
