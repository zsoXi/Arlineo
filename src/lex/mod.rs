//! Shared token types for the three hand-written lexers (`SPEC.md`, "Lexers").
//!
//! Lexers never fail: they always return tokens and collect problems as [`LexError`].
//! Whitespace and comments are not emitted.

pub mod rust;
pub mod sql;
pub mod ts;

/// Kind of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokKind {
    /// Identifier or keyword; keywords are not separated, extractors compare text.
    Ident,
    /// Rust lifetime or label: `'a`, `'static`.
    Lifetime,
    /// String literal including quotes and prefix.
    Str,
    /// Rust character or byte literal.
    Char,
    /// SQL dollar-quoted string `$tag$...$tag$`.
    DollarStr,
    /// SQL quoted identifier `"Name"`.
    QuotedIdent,
    /// TypeScript/JavaScript template literal including backticks and nested `${...}`.
    Template,
    /// TypeScript/JavaScript regular expression literal including flags.
    Regex,
    /// Number literal.
    Number,
    /// Punctuation or operator; multi-character operators are one token (`SPEC.md`).
    Punct,
}

/// One token: kind, byte span in the source and 1-based line of its first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

impl Token {
    /// Source text of the token; empty when the span does not fit `src`.
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start..self.end).unwrap_or("")
    }

    /// Whether the token has this kind and exactly this text.
    pub fn is(&self, src: &str, kind: TokKind, text: &str) -> bool {
        self.kind == kind && self.text(src) == text
    }
}

/// Lexer output.
#[derive(Clone, Debug, Default)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub errors: Vec<LexError>,
}

/// A problem the lexer recovered from (unterminated string or comment, stray byte).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    pub line: u32,
    pub message: String,
}
