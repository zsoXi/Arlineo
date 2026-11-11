//! PostgreSQL lexer (`SPEC.md`, "Lexers", "SQL").

use super::{LexError, Lexed, TokKind, Token};

/// Tokenizes PostgreSQL source.
pub fn lex(src: &str) -> Lexed {
    let mut scanner = Scanner {
        src,
        chars: src.char_indices().collect(),
        pos: 0,
        line: 1,
        lexed: Lexed::default(),
    };
    scanner.run();
    scanner.lexed
}

/// Value of a `'...'` or `E'...'` string token text with `''` (and, for `E`, backslash)
/// escapes resolved. `None` for anything else.
pub fn unquote(text: &str) -> Option<String> {
    let (is_e_string, rest) =
        if let Some(rest) = text.strip_prefix("E'").or_else(|| text.strip_prefix("e'")) {
            (true, rest)
        } else {
            let rest = text.strip_prefix('\'')?;
            (false, rest)
        };
    let mut value = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                value.push('\'');
                continue;
            }
            // A single quote closes the token; nothing may follow it.
            if chars.next().is_none() {
                return Some(value);
            }
            return None;
        }
        if is_e_string && ch == '\\' {
            let escaped = chars.next()?;
            match escaped {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                'b' => value.push('\u{0008}'),
                'f' => value.push('\u{000c}'),
                '\\' => value.push('\\'),
                '\'' => value.push('\''),
                'x' => {
                    let high = chars.next()?.to_digit(16)?;
                    let low = chars.next()?.to_digit(16)?;
                    let code = high.checked_mul(16)?.checked_add(low)?;
                    value.push(char::from_u32(code)?);
                }
                'u' => {
                    let mut code: u32 = 0;
                    for _ in 0..4 {
                        let digit = chars.next()?.to_digit(16)?;
                        code = code.checked_mul(16)?.checked_add(digit)?;
                    }
                    value.push(char::from_u32(code)?);
                }
                other => value.push(other),
            }
            continue;
        }
        value.push(ch);
    }
    None
}

/// Body of a dollar-quoted string token text `$tag$body$tag$`, or `None`.
pub fn dollar_body(text: &str) -> Option<&str> {
    if !text.starts_with('$') {
        return None;
    }
    let tag_end = text.get(1..)?.find('$')?.checked_add(1)?;
    let tag = text.get(1..tag_end)?;
    if !valid_dollar_tag(tag) {
        return None;
    }
    let delimiter_end = tag_end.checked_add(1)?;
    let delimiter = text.get(..delimiter_end)?;
    let rest = text.get(delimiter_end..)?;
    let body_end = rest.find(delimiter)?;
    if body_end.checked_add(delimiter.len())? != rest.len() {
        return None;
    }
    rest.get(..body_end)
}

/// Identifier text as a SQL name: unquoted identifiers lowercased, `"Quoted"` identifiers
/// with quotes removed and `""` unescaped, case kept.
pub fn ident_name(text: &str) -> String {
    let Some(after_opening) = text.strip_prefix('"') else {
        return text.to_lowercase();
    };
    let mut name = String::new();
    let mut chars = after_opening.chars().peekable();
    let mut closed = false;
    while let Some(ch) = chars.next() {
        if ch == '"' {
            if chars.peek() == Some(&'"') {
                chars.next();
                name.push('"');
            } else {
                closed = true;
                break;
            }
        } else {
            name.push(ch);
        }
    }
    if closed {
        name
    } else {
        // Missing closing quote: keep the text after the opening quote as written.
        after_opening.to_owned()
    }
}

fn valid_dollar_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    match chars.next() {
        None => true,
        Some(first) => {
            (first.is_alphabetic() || first == '_')
                && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
        }
    }
}

struct Scanner<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
    line: u32,
    lexed: Lexed,
}

impl<'a> Scanner<'a> {
    fn run(&mut self) {
        while let Some(ch) = self.current() {
            let start = self.current_byte();
            let line = self.line;
            if ch.is_whitespace() {
                self.bump();
            } else if ch == '-' && self.peek(1) == Some('-') {
                self.skip_line_comment();
            } else if ch == '/' && self.peek(1) == Some('*') {
                self.skip_block_comment(line);
            } else if ch == '"' {
                self.lex_quoted_ident(start, line);
            } else if let Some((prefix_len, is_e_string)) = self.string_prefix() {
                self.lex_string(start, line, prefix_len, is_e_string);
            } else if ch == '\'' {
                self.lex_string(start, line, 0, false);
            } else if ch == '$' {
                self.lex_dollar(start, line);
            } else if ch.is_alphabetic() || ch == '_' {
                self.lex_ident(start, line);
            } else if ch.is_ascii_digit()
                || (ch == '.' && self.peek(1).is_some_and(|next| next.is_ascii_digit()))
            {
                self.lex_number(start, line);
            } else {
                self.lex_punct(start, line);
            }
        }
    }

    fn current(&self) -> Option<char> {
        self.chars.get(self.pos).map(|&(_, ch)| ch)
    }

    fn peek(&self, offset: usize) -> Option<char> {
        let index = self.pos.checked_add(offset)?;
        self.chars.get(index).map(|&(_, ch)| ch)
    }

    fn byte_at(&self, index: usize) -> usize {
        match self.chars.get(index) {
            Some(&(byte, _)) => byte,
            None => self.src.len(),
        }
    }

    fn current_byte(&self) -> usize {
        self.byte_at(self.pos)
    }

    fn bump(&mut self) -> Option<char> {
        let &(_, ch) = self.chars.get(self.pos)?;
        self.pos = self.pos.saturating_add(1);
        if ch == '\n' {
            self.line = self.line.saturating_add(1);
        }
        Some(ch)
    }

    fn push_token(&mut self, kind: TokKind, start: usize, line: u32) {
        let end = self.current_byte();
        self.lexed.tokens.push(Token {
            kind,
            start,
            end,
            line,
        });
    }

    fn push_error(&mut self, line: u32, message: &str) {
        self.lexed.errors.push(LexError {
            line,
            message: message.to_owned(),
        });
    }

    fn string_prefix(&self) -> Option<(usize, bool)> {
        match self.current() {
            Some('E') | Some('e') if self.peek(1) == Some('\'') => Some((1, true)),
            Some('B') | Some('b') | Some('X') | Some('x') | Some('N') | Some('n')
                if self.peek(1) == Some('\'') =>
            {
                Some((1, false))
            }
            Some('U') | Some('u') if self.peek(1) == Some('&') && self.peek(2) == Some('\'') => {
                Some((2, false))
            }
            _ => None,
        }
    }

    fn skip_line_comment(&mut self) {
        self.bump();
        self.bump();
        while let Some(ch) = self.current() {
            if ch == '\n' {
                break;
            }
            self.bump();
        }
    }

    fn skip_block_comment(&mut self, start_line: u32) {
        self.bump();
        self.bump();
        let mut depth: u32 = 1;
        while let Some(ch) = self.current() {
            if ch == '/' && self.peek(1) == Some('*') {
                self.bump();
                self.bump();
                depth = depth.saturating_add(1);
            } else if ch == '*' && self.peek(1) == Some('/') {
                self.bump();
                self.bump();
                if depth <= 1 {
                    return;
                }
                depth = depth.saturating_sub(1);
            } else {
                self.bump();
            }
        }
        self.push_error(start_line, "unterminated comment");
    }

    fn lex_quoted_ident(&mut self, start: usize, line: u32) {
        self.bump();
        loop {
            match self.current() {
                None => {
                    self.push_token(TokKind::QuotedIdent, start, line);
                    self.push_error(line, "unterminated quoted identifier");
                    return;
                }
                Some('"') if self.peek(1) == Some('"') => {
                    self.bump();
                    self.bump();
                }
                Some('"') => {
                    self.bump();
                    self.push_token(TokKind::QuotedIdent, start, line);
                    return;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn lex_string(&mut self, start: usize, line: u32, prefix_len: usize, is_e_string: bool) {
        for _ in 0..prefix_len {
            self.bump();
        }
        self.bump();
        loop {
            match self.current() {
                None => {
                    self.push_token(TokKind::Str, start, line);
                    self.push_error(line, "unterminated string");
                    return;
                }
                Some('\\') if is_e_string => {
                    self.bump();
                    if self.current().is_some() {
                        self.bump();
                    }
                }
                Some('\'') if self.peek(1) == Some('\'') => {
                    self.bump();
                    self.bump();
                }
                Some('\'') => {
                    self.bump();
                    self.push_token(TokKind::Str, start, line);
                    return;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn lex_dollar(&mut self, start: usize, line: u32) {
        if self.peek(1).is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
            self.bump_digits();
            self.push_token(TokKind::Punct, start, line);
            return;
        }
        let Some((body_start, delimiter)) = self.dollar_opening(start) else {
            self.bump();
            self.push_token(TokKind::Punct, start, line);
            return;
        };
        let rest = self.src.get(body_start..).unwrap_or_default();
        match rest.find(delimiter) {
            Some(found) => {
                let end = body_start
                    .saturating_add(found)
                    .saturating_add(delimiter.len());
                while self.current_byte() < end {
                    if self.bump().is_none() {
                        break;
                    }
                }
                self.push_token(TokKind::DollarStr, start, line);
            }
            None => {
                while self.current().is_some() {
                    self.bump();
                }
                self.push_token(TokKind::DollarStr, start, line);
                self.push_error(line, "unterminated dollar-quoted string");
            }
        }
    }

    fn dollar_opening(&self, start: usize) -> Option<(usize, &'a str)> {
        let first = self.peek(1)?;
        if first == '$' {
            let end = self.byte_at(self.pos.saturating_add(2));
            let delimiter = self.src.get(start..end)?;
            return Some((end, delimiter));
        }
        if !(first.is_alphabetic() || first == '_') {
            return None;
        }
        let mut offset: usize = 1;
        loop {
            let ch = self.peek(offset)?;
            if ch == '$' {
                let end = self.byte_at(self.pos.saturating_add(offset).saturating_add(1));
                let delimiter = self.src.get(start..end)?;
                return Some((end, delimiter));
            }
            if !(ch.is_alphanumeric() || ch == '_') {
                return None;
            }
            offset = offset.saturating_add(1);
        }
    }

    fn lex_ident(&mut self, start: usize, line: u32) {
        self.bump();
        while let Some(ch) = self.current() {
            if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                self.bump();
            } else {
                break;
            }
        }
        self.push_token(TokKind::Ident, start, line);
    }

    fn lex_number(&mut self, start: usize, line: u32) {
        if self.current() == Some('.') {
            self.bump();
            self.bump_digits();
        } else {
            self.bump_digits();
            if self.current() == Some('.') && self.peek(1).is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
                self.bump_digits();
            }
        }
        if matches!(self.current(), Some('e') | Some('E')) {
            let first = self.peek(1);
            let has_digits = first.is_some_and(|ch| ch.is_ascii_digit())
                || (matches!(first, Some('+') | Some('-'))
                    && self.peek(2).is_some_and(|ch| ch.is_ascii_digit()));
            if has_digits {
                self.bump();
                if matches!(self.current(), Some('+') | Some('-')) {
                    self.bump();
                }
                self.bump_digits();
            }
        }
        self.push_token(TokKind::Number, start, line);
    }

    fn bump_digits(&mut self) {
        while self.current().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
    }

    fn lex_punct(&mut self, start: usize, line: u32) {
        let Some(ch) = self.current() else {
            return;
        };
        let three = (ch == '#' && self.peek(1) == Some('>') && self.peek(2) == Some('>'))
            || (ch == '-' && self.peek(1) == Some('>') && self.peek(2) == Some('>'));
        let two = matches!(
            (ch, self.peek(1)),
            (':', Some(':'))
                | (':', Some('='))
                | ('<', Some('='))
                | ('<', Some('>'))
                | ('>', Some('='))
                | ('!', Some('='))
                | ('|', Some('|'))
                | ('-', Some('>'))
                | ('#', Some('>'))
                | ('=', Some('>'))
        );
        let count = if three {
            3
        } else if two {
            2
        } else {
            1
        };
        for _ in 0..count {
            self.bump();
        }
        self.push_token(TokKind::Punct, start, line);
    }
}
