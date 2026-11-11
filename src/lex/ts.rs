//! TypeScript and JavaScript lexer, with optional JSX (`SPEC.md`, "Lexers", "TypeScript").

use super::{LexError, Lexed, TokKind, Token};

/// Tokenizes TypeScript or JavaScript source; `jsx` enables JSX handling (`.tsx`, `.jsx`).
pub fn lex(src: &str, jsx: bool) -> Lexed {
    let mut lexer = Lexer::new(src, jsx);
    lexer.run();
    Lexed {
        tokens: lexer.tokens,
        errors: lexer.errors,
    }
}

/// Value of a `'...'` or `"..."` string token text with escapes resolved, or `None`.
pub fn unquote(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let quote = match chars.first() {
        Some(&c) if c == '\'' || c == '"' => c,
        _ => return None,
    };
    let mut out = String::new();
    let mut i = 1usize;
    while let Some(&c) = chars.get(i) {
        if c == quote {
            if i.saturating_add(1) == chars.len() {
                return Some(out);
            }
            return None;
        }
        if c == '\n' || c == '\r' {
            return None;
        }
        if c != '\\' {
            out.push(c);
            i = i.saturating_add(1);
            continue;
        }
        i = i.saturating_add(1);
        let escaped = match chars.get(i) {
            Some(&c) => c,
            None => return None,
        };
        match escaped {
            'n' => {
                out.push('\n');
                i = i.saturating_add(1);
            }
            'r' => {
                out.push('\r');
                i = i.saturating_add(1);
            }
            't' => {
                out.push('\t');
                i = i.saturating_add(1);
            }
            'b' => {
                out.push('\u{0008}');
                i = i.saturating_add(1);
            }
            'f' => {
                out.push('\u{000c}');
                i = i.saturating_add(1);
            }
            'v' => {
                out.push('\u{000b}');
                i = i.saturating_add(1);
            }
            '0' => {
                out.push('\0');
                i = i.saturating_add(1);
            }
            '\\' => {
                out.push('\\');
                i = i.saturating_add(1);
            }
            '\'' => {
                out.push('\'');
                i = i.saturating_add(1);
            }
            '"' => {
                out.push('"');
                i = i.saturating_add(1);
            }
            'x' => {
                let high = hex_value(*chars.get(i.saturating_add(1))?)?;
                let low = hex_value(*chars.get(i.saturating_add(2))?)?;
                let value = high.saturating_mul(16).saturating_add(low);
                out.push(char::from_u32(value)?);
                i = i.saturating_add(3);
            }
            'u' => {
                if chars.get(i.saturating_add(1)) == Some(&'{') {
                    let mut j = i.saturating_add(2);
                    let mut value: u32 = 0;
                    let mut digits = 0usize;
                    loop {
                        let ch = match chars.get(j) {
                            Some(&c) => c,
                            None => return None,
                        };
                        if ch == '}' {
                            break;
                        }
                        let digit = hex_value(ch)?;
                        value = value.checked_mul(16)?.checked_add(digit)?;
                        if value > 0x10ffff {
                            return None;
                        }
                        digits = digits.saturating_add(1);
                        j = j.saturating_add(1);
                    }
                    if digits == 0 {
                        return None;
                    }
                    out.push(char::from_u32(value)?);
                    i = j.saturating_add(1);
                } else {
                    let value = read_u4(&chars, i.saturating_add(1))?;
                    if (0xd800..0xdc00).contains(&value) {
                        if chars.get(i.saturating_add(5)) == Some(&'\\')
                            && chars.get(i.saturating_add(6)) == Some(&'u')
                        {
                            let low = read_u4(&chars, i.saturating_add(7))?;
                            if !(0xdc00..0xe000).contains(&low) {
                                return None;
                            }
                            // Both halves are range-checked above, so the combined
                            // value is a valid scalar between 0x10000 and 0x10ffff.
                            let combined = 0x10000_u32
                                .saturating_add(value.saturating_sub(0xd800).saturating_mul(1024))
                                .saturating_add(low.saturating_sub(0xdc00));
                            out.push(char::from_u32(combined)?);
                            i = i.saturating_add(11);
                        } else {
                            return None;
                        }
                    } else if (0xdc00..0xe000).contains(&value) {
                        return None;
                    } else {
                        out.push(char::from_u32(value)?);
                        i = i.saturating_add(5);
                    }
                }
            }
            '\n' => {
                i = i.saturating_add(1);
            }
            '\r' => {
                i = i.saturating_add(1);
                if chars.get(i) == Some(&'\n') {
                    i = i.saturating_add(1);
                }
            }
            '\u{2028}' | '\u{2029}' => {
                i = i.saturating_add(1);
            }
            _ => {
                out.push(escaped);
                i = i.saturating_add(1);
            }
        }
    }
    None
}

/// Text of a template literal token without backticks, with every `${...}` replaced by
/// `${}`; `None` when the text is not a template literal.
pub fn template_pattern(text: &str) -> Option<String> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if char_at(&chars, 0) != Some('`') {
        return None;
    }
    let mut out = String::new();
    let scan = scan_template(&chars, 0, Some(&mut out));
    if !scan.terminated || scan.end != chars.len() {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Character helpers over char_indices positions. Indexes here are character indexes, not bytes.

fn char_at(chars: &[(usize, char)], index: usize) -> Option<char> {
    chars.get(index).map(|entry| entry.1)
}

fn byte_at(chars: &[(usize, char)], index: usize, fallback: usize) -> usize {
    chars.get(index).map(|entry| entry.0).unwrap_or(fallback)
}

fn is_ident_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_alphanumeric()
}

fn is_regex_keyword(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "new"
            | "delete"
            | "void"
            | "throw"
            | "case"
            | "do"
            | "else"
            | "yield"
            | "await"
    )
}

fn hex_value(c: char) -> Option<u32> {
    c.to_digit(16)
}

fn read_u4(chars: &[char], start: usize) -> Option<u32> {
    let mut value: u32 = 0;
    let mut offset = 0usize;
    while offset < 4 {
        let c = *chars.get(start.saturating_add(offset))?;
        let digit = hex_value(c)?;
        value = value.checked_mul(16)?.checked_add(digit)?;
        offset = offset.saturating_add(1);
    }
    Some(value)
}

/// Index just past a string starting at `start`, and whether its closing quote was found.
/// An unescaped newline or the end of input ends the string without a closing quote.
fn skip_string_chars(chars: &[(usize, char)], start: usize) -> (usize, bool) {
    let quote = match char_at(chars, start) {
        Some(c) => c,
        None => return (chars.len(), false),
    };
    let mut i = start.saturating_add(1);
    while let Some(c) = char_at(chars, i) {
        if c == '\\' {
            // A backslash escapes the next character; a CRLF continuation is one escape.
            if char_at(chars, i.saturating_add(1)) == Some('\r')
                && char_at(chars, i.saturating_add(2)) == Some('\n')
            {
                i = i.saturating_add(3);
            } else {
                i = i.saturating_add(2);
            }
        } else if c == quote {
            return (i.saturating_add(1), true);
        } else if c == '\n' || c == '\r' {
            return (i, false);
        } else {
            i = i.saturating_add(1);
        }
    }
    (chars.len(), false)
}

fn skip_line_comment_chars(chars: &[(usize, char)], start: usize) -> usize {
    let mut i = start;
    while let Some(c) = char_at(chars, i) {
        if c == '\n' {
            break;
        }
        i = i.saturating_add(1);
    }
    i
}

/// Index just past a block comment starting at `start`, and whether `*/` was found.
fn skip_block_comment_chars(chars: &[(usize, char)], start: usize) -> (usize, bool) {
    let mut i = start.saturating_add(2);
    while let Some(c) = char_at(chars, i) {
        if c == '*' && char_at(chars, i.saturating_add(1)) == Some('/') {
            return (i.saturating_add(2), true);
        }
        i = i.saturating_add(1);
    }
    (chars.len(), false)
}

/// Index just past a regex literal (including flags) starting at `start`, or `None` when no
/// closing slash appears before a newline or the end of input.
fn regex_end_chars(chars: &[(usize, char)], start: usize) -> Option<usize> {
    let mut i = start.saturating_add(1);
    let mut in_class = false;
    while let Some(c) = char_at(chars, i) {
        if c == '\n' || c == '\r' {
            return None;
        }
        if c == '\\' {
            match char_at(chars, i.saturating_add(1)) {
                Some('\n') | Some('\r') => return None,
                _ => {}
            }
            i = i.saturating_add(2);
            continue;
        }
        if c == '[' {
            in_class = true;
        } else if c == ']' {
            in_class = false;
        } else if c == '/' && !in_class {
            let mut j = i.saturating_add(1);
            while let Some(flag) = char_at(chars, j) {
                if is_ident_continue(flag) {
                    j = j.saturating_add(1);
                } else {
                    break;
                }
            }
            return Some(j);
        }
        i = i.saturating_add(1);
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Template literals. One iterative scan serves both the lexer (which only needs the span and
// the problems) and `template_pattern` (which also collects the literal text).

#[derive(Clone)]
enum Prev {
    None,
    Value,
    Punct(char),
    Ident(String),
}

#[derive(Clone)]
struct Frame {
    text: bool,
    braces: usize,
    prev: Prev,
}

struct TemplateScan {
    end: usize,
    terminated: bool,
    errors: Vec<(usize, &'static str)>,
}

fn regex_allowed_prev(prev: &Prev) -> bool {
    match prev {
        Prev::None => true,
        Prev::Value => false,
        Prev::Punct(c) => !matches!(c, ')' | ']' | '}'),
        Prev::Ident(word) => is_regex_keyword(word),
    }
}

fn scan_template(
    chars: &[(usize, char)],
    start: usize,
    mut pattern: Option<&mut String>,
) -> TemplateScan {
    let mut errors: Vec<(usize, &'static str)> = Vec::new();
    let mut stack: Vec<Frame> = vec![Frame {
        text: true,
        braces: 0,
        prev: Prev::None,
    }];
    let mut i = start.saturating_add(1);
    while let Some(c) = char_at(chars, i) {
        let frame = match stack.last().cloned() {
            Some(frame) => frame,
            None => break,
        };
        if frame.text {
            if c == '\\' {
                if stack.len() == 1 {
                    if let Some(out) = pattern.as_mut() {
                        out.push('\\');
                        if let Some(next) = char_at(chars, i.saturating_add(1)) {
                            out.push(next);
                        }
                    }
                }
                i = i.saturating_add(2);
                continue;
            }
            if c == '`' {
                i = i.saturating_add(1);
                stack.pop();
                if stack.is_empty() {
                    return TemplateScan {
                        end: i,
                        terminated: true,
                        errors,
                    };
                }
                continue;
            }
            if c == '$' && char_at(chars, i.saturating_add(1)) == Some('{') {
                if stack.len() == 1 {
                    if let Some(out) = pattern.as_mut() {
                        out.push_str("${}");
                    }
                }
                stack.push(Frame {
                    text: false,
                    braces: 1,
                    prev: Prev::None,
                });
                i = i.saturating_add(2);
                continue;
            }
            if stack.len() == 1 {
                if let Some(out) = pattern.as_mut() {
                    out.push(c);
                }
            }
            i = i.saturating_add(1);
            continue;
        }
        // Inside a ${ ... } expression: only the structure matters, nothing is collected.
        let top_index = stack.len().saturating_sub(1);
        let mut frame = frame;
        let mut pop_frame = false;
        let mut push_text = false;
        if c.is_whitespace() {
            i = i.saturating_add(1);
        } else if c == '\'' || c == '"' {
            let (end, closed) = skip_string_chars(chars, i);
            if !closed {
                errors.push((i, "unterminated string"));
            }
            i = end;
            frame.prev = Prev::Value;
        } else if c == '`' {
            let text_depth = stack.iter().filter(|entry| entry.text).count();
            if text_depth >= 64 {
                errors.push((i, "template literal nesting too deep"));
                // Treat the rest as plain text until the next backtick.
                let mut j = i.saturating_add(1);
                while let Some(next) = char_at(chars, j) {
                    if next == '\\' {
                        j = j.saturating_add(2);
                    } else if next == '`' {
                        return TemplateScan {
                            end: j.saturating_add(1),
                            terminated: true,
                            errors,
                        };
                    } else {
                        j = j.saturating_add(1);
                    }
                }
                return TemplateScan {
                    end: chars.len(),
                    terminated: false,
                    errors,
                };
            }
            push_text = true;
            i = i.saturating_add(1);
            frame.prev = Prev::Value;
        } else if c == '/' && char_at(chars, i.saturating_add(1)) == Some('/') {
            i = skip_line_comment_chars(chars, i);
        } else if c == '/' && char_at(chars, i.saturating_add(1)) == Some('*') {
            let (end, closed) = skip_block_comment_chars(chars, i);
            if !closed {
                errors.push((i, "unterminated block comment"));
            }
            i = end;
        } else if c == '/' {
            if regex_allowed_prev(&frame.prev) {
                match regex_end_chars(chars, i) {
                    Some(end) => {
                        i = end;
                        frame.prev = Prev::Value;
                    }
                    None => {
                        errors.push((i, "unterminated regex literal"));
                        i = i.saturating_add(1);
                        frame.prev = Prev::Punct('/');
                    }
                }
            } else {
                i = i.saturating_add(1);
                frame.prev = Prev::Punct('/');
            }
        } else if c == '{' {
            frame.braces = frame.braces.saturating_add(1);
            i = i.saturating_add(1);
            frame.prev = Prev::Punct('{');
        } else if c == '}' {
            frame.braces = frame.braces.saturating_sub(1);
            i = i.saturating_add(1);
            if frame.braces == 0 {
                pop_frame = true;
            } else {
                frame.prev = Prev::Punct('}');
            }
        } else if is_ident_start(c)
            || (c == '#'
                && matches!(char_at(chars, i.saturating_add(1)), Some(n) if is_ident_start(n)))
        {
            let mut j = if c == '#' {
                i.saturating_add(2)
            } else {
                i.saturating_add(1)
            };
            while let Some(next) = char_at(chars, j) {
                if is_ident_continue(next) {
                    j = j.saturating_add(1);
                } else {
                    break;
                }
            }
            let mut word = String::new();
            let mut k = i;
            while k < j {
                if let Some(next) = char_at(chars, k) {
                    if next != '#' {
                        word.push(next);
                    }
                }
                k = k.saturating_add(1);
            }
            i = j;
            frame.prev = Prev::Ident(word);
        } else if c.is_ascii_digit() {
            let mut j = i.saturating_add(1);
            while let Some(next) = char_at(chars, j) {
                if next.is_alphanumeric() || next == '.' || next == '_' || next == '$' {
                    j = j.saturating_add(1);
                } else {
                    break;
                }
            }
            i = j;
            frame.prev = Prev::Value;
        } else {
            i = i.saturating_add(1);
            frame.prev = Prev::Punct(c);
        }
        if let Some(slot) = stack.get_mut(top_index) {
            *slot = frame;
        }
        if pop_frame {
            stack.pop();
        } else if push_text {
            stack.push(Frame {
                text: true,
                braces: 0,
                prev: Prev::None,
            });
        }
    }
    TemplateScan {
        end: chars.len(),
        terminated: false,
        errors,
    }
}

// ---------------------------------------------------------------------------------------------
// The lexer itself.

struct Lexer<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
    line: u32,
    tokens: Vec<Token>,
    errors: Vec<LexError>,
    jsx: bool,
    /// Nesting of JSX regions inside `{...}`; bounded so a crafted file cannot overflow the stack.
    jsx_depth: usize,
}

/// Deepest JSX nesting (elements inside `{...}` inside elements) that is lexed as JSX.
const MAX_JSX_DEPTH: usize = 256;

const JSX_UNTERMINATED: &str = "unterminated JSX tag";

enum TagKind {
    Opening,
    SelfClosing,
    Closing,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, jsx: bool) -> Lexer<'a> {
        Lexer {
            src,
            chars: src.char_indices().collect(),
            pos: 0,
            line: 1,
            tokens: Vec::new(),
            errors: Vec::new(),
            jsx,
            jsx_depth: 0,
        }
    }

    fn run(&mut self) {
        if self.cur() == Some('#') && self.peek() == Some('!') {
            let end = skip_line_comment_chars(&self.chars, 0);
            self.advance_to(end);
        }
        while self.pos < self.chars.len() {
            self.skip_space_and_comments();
            if self.pos >= self.chars.len() {
                break;
            }
            if self.jsx && self.is_jsx_start() {
                self.lex_jsx();
                continue;
            }
            self.lex_one();
        }
    }

    fn cur(&self) -> Option<char> {
        char_at(&self.chars, self.pos)
    }

    fn peek(&self) -> Option<char> {
        char_at(&self.chars, self.pos.saturating_add(1))
    }

    fn ahead(&self, offset: usize) -> Option<char> {
        char_at(&self.chars, self.pos.saturating_add(offset))
    }

    fn byte_pos(&self) -> usize {
        byte_at(&self.chars, self.pos, self.src.len())
    }

    fn bump(&mut self) {
        if let Some(c) = self.cur() {
            // Count CRLF once: the line feed does the counting.
            if c == '\n' || (c == '\r' && self.peek() != Some('\n')) {
                self.line = self.line.saturating_add(1);
            }
            self.pos = self.pos.saturating_add(1);
        }
    }

    fn advance_to(&mut self, target: usize) {
        while self.pos < target && self.pos < self.chars.len() {
            self.bump();
        }
    }

    fn emit(&mut self, kind: TokKind, start: usize, line: u32) {
        let end = self.byte_pos();
        self.tokens.push(Token {
            kind,
            start,
            end,
            line,
        });
    }

    fn record_error(&mut self, line: u32, message: &str) {
        self.errors.push(LexError {
            line,
            message: message.to_owned(),
        });
    }

    fn line_at(&self, base_line: u32, from: usize, to: usize) -> u32 {
        let mut line = base_line;
        let mut i = from;
        while i < to && i < self.chars.len() {
            if char_at(&self.chars, i) == Some('\n')
                || (char_at(&self.chars, i) == Some('\r')
                    && char_at(&self.chars, i.saturating_add(1)) != Some('\n'))
            {
                line = line.saturating_add(1);
            }
            i = i.saturating_add(1);
        }
        line
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            match self.cur() {
                Some(c) if c.is_whitespace() => self.bump(),
                Some('/') if self.peek() == Some('/') => {
                    let end = skip_line_comment_chars(&self.chars, self.pos);
                    self.advance_to(end);
                }
                Some('/') if self.peek() == Some('*') => {
                    let start_line = self.line;
                    let (end, closed) = skip_block_comment_chars(&self.chars, self.pos);
                    self.advance_to(end);
                    if !closed {
                        self.record_error(start_line, "unterminated block comment");
                    }
                }
                _ => break,
            }
        }
    }

    /// Whether a regex (or JSX) may start here, from the previous emitted token.
    fn regex_allowed(&self) -> bool {
        let token = match self.tokens.last().copied() {
            Some(token) => token,
            None => return true,
        };
        match token.kind {
            TokKind::Punct => !matches!(token.text(self.src), ")" | "]" | "}"),
            TokKind::Ident => is_regex_keyword(token.text(self.src)),
            _ => false,
        }
    }

    fn is_jsx_start(&self) -> bool {
        if self.cur() != Some('<') || !self.regex_allowed() {
            return false;
        }
        match self.peek() {
            Some(c) if is_ident_start(c) => !self.generic_params_ahead(),
            Some('>') => true,
            Some('/') => match self.ahead(2) {
                Some(c) => is_ident_start(c) || c == '>',
                None => false,
            },
            _ => false,
        }
    }

    /// Whether the `<` here opens generic parameters of an arrow function (`<T,>(x) =>`,
    /// `<T extends U>(x) =>`), which `.tsx` allows and which is not JSX.
    fn generic_params_ahead(&self) -> bool {
        let mut offset = 1usize;
        while matches!(self.ahead(offset), Some(c) if is_ident_continue(c)) {
            offset = offset.saturating_add(1);
        }
        while matches!(self.ahead(offset), Some(c) if c.is_whitespace()) {
            offset = offset.saturating_add(1);
        }
        if self.ahead(offset) == Some(',') {
            return true;
        }
        for expected in "extends".chars() {
            if self.ahead(offset) != Some(expected) {
                return false;
            }
            offset = offset.saturating_add(1);
        }
        matches!(self.ahead(offset), Some(c) if c.is_whitespace())
    }

    fn lex_one(&mut self) {
        let c = match self.cur() {
            Some(c) => c,
            None => return,
        };
        if is_ident_start(c) || (c == '#' && matches!(self.peek(), Some(n) if is_ident_start(n))) {
            self.lex_ident();
        } else if c.is_ascii_digit()
            || (c == '.' && matches!(self.peek(), Some(n) if n.is_ascii_digit()))
        {
            self.lex_number();
        } else if c == '\'' || c == '"' {
            self.lex_string();
        } else if c == '`' {
            self.lex_template();
        } else if c == '/' {
            self.lex_slash();
        } else if c == '\\' {
            let start = self.byte_pos();
            let line = self.line;
            if self.peek() == Some('u') {
                self.record_error(line, "unsupported \\u escape in identifier");
            }
            self.bump();
            self.emit(TokKind::Punct, start, line);
        } else {
            self.lex_punct();
        }
    }

    fn lex_ident(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        if self.cur() == Some('#') {
            self.bump();
        }
        self.bump();
        while let Some(c) = self.cur() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }
        self.emit(TokKind::Ident, start, line);
    }

    fn lex_string(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        let (end, closed) = skip_string_chars(&self.chars, self.pos);
        self.advance_to(end);
        if !closed {
            self.record_error(line, "unterminated string");
        }
        self.emit(TokKind::Str, start, line);
    }

    fn lex_template(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        let start_index = self.pos;
        let scan = scan_template(&self.chars, start_index, None);
        let end = scan.end;
        let terminated = scan.terminated;
        for (index, message) in scan.errors {
            let error_line = self.line_at(line, start_index, index);
            self.record_error(error_line, message);
        }
        if !terminated {
            self.record_error(line, "unterminated template literal");
        }
        self.advance_to(end);
        self.emit(TokKind::Template, start, line);
    }

    fn lex_slash(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        if self.regex_allowed() {
            match regex_end_chars(&self.chars, self.pos) {
                Some(end) => {
                    self.advance_to(end);
                    self.emit(TokKind::Regex, start, line);
                }
                None => {
                    self.record_error(line, "unterminated regex literal");
                    self.bump();
                    self.emit(TokKind::Punct, start, line);
                }
            }
        } else if self.peek() == Some('=') {
            self.bump();
            self.bump();
            self.emit(TokKind::Punct, start, line);
        } else {
            self.bump();
            self.emit(TokKind::Punct, start, line);
        }
    }

    fn lex_number(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        let mut integer = true;
        let mut based = false;
        if self.cur() == Some('0') {
            if let Some(prefix) = self.peek() {
                if matches!(prefix, 'x' | 'X' | 'b' | 'B' | 'o' | 'O') {
                    based = true;
                    self.bump();
                    self.bump();
                    while let Some(c) = self.cur() {
                        if is_based_digit(c, prefix) || c == '_' {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        if !based {
            if self.cur() == Some('.') {
                integer = false;
                self.bump();
            }
            while let Some(c) = self.cur() {
                if c.is_ascii_digit() || c == '_' {
                    self.bump();
                } else {
                    break;
                }
            }
            if self.cur() == Some('.') && matches!(self.peek(), Some(n) if n.is_ascii_digit()) {
                integer = false;
                self.bump();
                while let Some(c) = self.cur() {
                    if c.is_ascii_digit() || c == '_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
            }
            if matches!(self.cur(), Some('e') | Some('E')) {
                let mut ahead = self.pos.saturating_add(1);
                if matches!(char_at(&self.chars, ahead), Some('+') | Some('-')) {
                    ahead = ahead.saturating_add(1);
                }
                if matches!(char_at(&self.chars, ahead), Some(n) if n.is_ascii_digit()) {
                    integer = false;
                    self.bump();
                    if matches!(self.cur(), Some('+') | Some('-')) {
                        self.bump();
                    }
                    while let Some(c) = self.cur() {
                        if c.is_ascii_digit() || c == '_' {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        if integer && self.cur() == Some('n') {
            self.bump();
        }
        self.emit(TokKind::Number, start, line);
    }

    fn lex_punct(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        if self.cur() == Some('>') {
            // Shift assignments are whole tokens; generics never close directly before `=`.
            let rest = self.src.get(start..).unwrap_or("");
            for operator in [">>>=", ">>="] {
                if rest.starts_with(operator) {
                    self.advance_to(self.pos.saturating_add(operator.len()));
                    self.emit(TokKind::Punct, start, line);
                    return;
                }
            }
            // A run of `>` characters is split into single tokens so generics can close;
            // a `>=` directly after such a run is split into `>` and `=` as well.
            if self.peek() == Some('>') {
                self.bump();
                self.emit(TokKind::Punct, start, line);
                return;
            }
            if self.peek() == Some('=') {
                let previous = if self.pos == 0 {
                    None
                } else {
                    char_at(&self.chars, self.pos.saturating_sub(1))
                };
                self.bump();
                if previous != Some('>') {
                    self.bump();
                }
                self.emit(TokKind::Punct, start, line);
                return;
            }
            self.bump();
            self.emit(TokKind::Punct, start, line);
            return;
        }
        let chosen = {
            let rest = self.src.get(start..).unwrap_or("");
            let candidates: &[&str] = &[
                "...", "===", "!==", "**=", "<<=", "&&=", "||=", "??=", "=>", "==", "!=", "<=",
                ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=",
                "^=", "**", "<<",
            ];
            let mut found: Option<usize> = None;
            for candidate in candidates {
                if !rest.starts_with(*candidate) {
                    continue;
                }
                // `?.` before a digit is a conditional, not an optional chain.
                if *candidate == "?."
                    && matches!(rest.chars().nth(2), Some(n) if n.is_ascii_digit())
                {
                    continue;
                }
                found = Some(candidate.chars().count());
                break;
            }
            found
        };
        match chosen {
            Some(count) => self.advance_to(self.pos.saturating_add(count)),
            None => self.bump(),
        }
        self.emit(TokKind::Punct, start, line);
    }

    fn emit_one_punct(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        self.bump();
        self.emit(TokKind::Punct, start, line);
    }

    // -----------------------------------------------------------------------------------------
    // JSX. `lex_jsx` is the entry point for a top-level region. A failure anywhere inside it,
    // nested regions included, rolls the whole region back and it is lexed as plain code. Nested
    // regions are not rolled back one by one: retrying every enclosing level from every position
    // takes time exponential in the nesting depth on unterminated input.

    fn lex_jsx(&mut self) {
        let saved_pos = self.pos;
        let saved_line = self.line;
        let saved_tokens = self.tokens.len();
        let saved_errors = self.errors.len();
        if let Err(reason) = self.try_lex_jsx_region() {
            self.tokens.truncate(saved_tokens);
            self.errors.truncate(saved_errors);
            self.pos = saved_pos;
            self.line = saved_line;
            self.record_error(saved_line, reason);
            // Continue as plain code from after the `<`, which becomes a Punct token.
            let start = self.byte_pos();
            let line = self.line;
            self.bump();
            self.emit(TokKind::Punct, start, line);
        }
    }

    /// A JSX region inside `{...}` of an enclosing region.
    fn lex_nested_jsx(&mut self) -> Result<(), &'static str> {
        if self.jsx_depth >= MAX_JSX_DEPTH {
            return Err("JSX nested too deeply");
        }
        self.jsx_depth = self.jsx_depth.saturating_add(1);
        let result = self.try_lex_jsx_region();
        self.jsx_depth = self.jsx_depth.saturating_sub(1);
        result
    }

    fn try_lex_jsx_region(&mut self) -> Result<(), &'static str> {
        let region_line = self.line;
        let mut depth = 0usize;
        loop {
            match self.try_parse_tag()? {
                TagKind::Opening => depth = depth.saturating_add(1),
                TagKind::SelfClosing => {
                    if depth == 0 {
                        return Ok(());
                    }
                }
                TagKind::Closing => {
                    if depth <= 1 {
                        return Ok(());
                    }
                    depth = depth.saturating_sub(1);
                }
            }
            // Text between tags is skipped up to the next tag or expression.
            loop {
                let c = match self.cur() {
                    Some(c) => c,
                    None => {
                        self.record_error(region_line, "unterminated JSX element");
                        return Ok(());
                    }
                };
                if c == '<' {
                    match self.peek() {
                        Some(n) if is_ident_start(n) || n == '/' || n == '>' => break,
                        _ => self.bump(),
                    }
                } else if c == '{' {
                    self.lex_jsx_expr()?;
                } else {
                    self.bump();
                }
            }
        }
    }

    fn try_parse_tag(&mut self) -> Result<TagKind, &'static str> {
        // The current character is `<`.
        self.emit_one_punct();
        let mut closing = false;
        if self.cur() == Some('/') {
            closing = true;
            self.emit_one_punct();
        }
        if self.cur() == Some('>') {
            self.emit_one_punct();
            return Ok(if closing {
                TagKind::Closing
            } else {
                TagKind::Opening
            });
        }
        if matches!(self.cur(), Some(c) if is_ident_start(c)) {
            self.lex_jsx_name();
        }
        loop {
            self.skip_space_and_comments();
            let c = match self.cur() {
                Some(c) => c,
                None => return Err(JSX_UNTERMINATED),
            };
            if c == '/' && self.peek() == Some('>') {
                self.emit_one_punct();
                self.emit_one_punct();
                return Ok(if closing {
                    TagKind::Closing
                } else {
                    TagKind::SelfClosing
                });
            }
            if c == '>' {
                self.emit_one_punct();
                return Ok(if closing {
                    TagKind::Closing
                } else {
                    TagKind::Opening
                });
            }
            if c == '{' {
                self.lex_jsx_expr()?;
                continue;
            }
            if is_ident_start(c) {
                self.lex_jsx_name();
                self.skip_space_and_comments();
                if self.cur() == Some('=') {
                    self.emit_one_punct();
                    self.skip_space_and_comments();
                    match self.cur() {
                        None => return Err(JSX_UNTERMINATED),
                        Some('\'') | Some('"') => self.lex_jsx_attr_string()?,
                        Some('{') => self.lex_jsx_expr()?,
                        Some('>') => {}
                        Some(_) => self.lex_one(),
                    }
                }
                continue;
            }
            self.emit_one_punct();
        }
    }

    /// A JSX attribute string: no escape sequences, and it may span lines.
    fn lex_jsx_attr_string(&mut self) -> Result<(), &'static str> {
        let start = self.byte_pos();
        let line = self.line;
        let quote = self.cur();
        self.bump();
        loop {
            match self.cur() {
                None => return Err(JSX_UNTERMINATED),
                Some(c) if Some(c) == quote => {
                    self.bump();
                    break;
                }
                Some(_) => self.bump(),
            }
        }
        self.emit(TokKind::Str, start, line);
        Ok(())
    }

    /// A tag or attribute name: identifier characters with `.`, `-` and `:` joined in.
    fn lex_jsx_name(&mut self) {
        let start = self.byte_pos();
        let line = self.line;
        self.bump();
        while let Some(c) = self.cur() {
            if is_ident_continue(c)
                || (matches!(c, '.' | '-' | ':')
                    && matches!(self.peek(), Some(n) if is_ident_continue(n)))
            {
                self.bump();
            } else {
                break;
            }
        }
        self.emit(TokKind::Ident, start, line);
    }

    /// Lexes `{ ... }` as code with brace depth tracking, including nested JSX.
    fn lex_jsx_expr(&mut self) -> Result<(), &'static str> {
        self.emit_one_punct();
        let mut depth = 1usize;
        loop {
            self.skip_space_and_comments();
            if self.cur().is_none() {
                return Err(JSX_UNTERMINATED);
            }
            if self.jsx && self.is_jsx_start() {
                self.lex_nested_jsx()?;
                continue;
            }
            self.lex_one();
            if let Some(token) = self.tokens.last().copied() {
                if token.is(self.src, TokKind::Punct, "{") {
                    depth = depth.saturating_add(1);
                } else if token.is(self.src, TokKind::Punct, "}") {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn is_based_digit(c: char, prefix: char) -> bool {
    match prefix {
        'x' | 'X' => c.is_ascii_hexdigit(),
        'b' | 'B' => matches!(c, '0' | '1'),
        'o' | 'O' => ('0'..='7').contains(&c),
        _ => c.is_ascii_digit(),
    }
}

#[cfg(test)]
mod tests {
    use super::lex;

    #[test]
    fn shift_assignments_are_one_token() {
        let src = "a >>= 1; b >>>= 2; c = d > e;";
        let lexed = lex(src, false);
        let texts: Vec<&str> = lexed.tokens.iter().map(|token| token.text(src)).collect();
        assert!(texts.contains(&">>="), "{texts:?}");
        assert!(texts.contains(&">>>="), "{texts:?}");
        assert!(lexed.errors.is_empty());
    }

    #[test]
    fn deep_jsx_nesting_does_not_overflow_the_stack() {
        // `<a>{<a>{...` recurses through JSX expressions; the depth limit stops the recursion,
        // so this returns with errors instead of overflowing the stack.
        let src = format!("const x = {};", "<a>{".repeat(10_000));
        let lexed = lex(&src, true);
        assert!(
            lexed
                .errors
                .iter()
                .any(|error| error.message == "JSX nested too deeply"),
            "{:?}",
            lexed.errors.first()
        );
    }

    #[test]
    fn multi_line_jsx_attribute_string() {
        let src = "const x = <a title=\"one\ntwo\">t</a>;";
        let lexed = lex(src, true);
        assert!(lexed.errors.is_empty(), "{:?}", lexed.errors.first());
    }
}
