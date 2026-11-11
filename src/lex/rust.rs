//! Rust lexer (`SPEC.md`, "Lexers", "Rust").

use super::{LexError, Lexed, TokKind, Token};

/// Tokenizes Rust source.
pub fn lex(src: &str) -> Lexed {
    let mut scanner = Scanner::new(src);
    let mut lexed = Lexed::default();

    // A shebang is only at the very start and is never the attribute `#! [...]`.
    if scanner.peek() == Some('#')
        && scanner.peek_at(1) == Some('!')
        && scanner.peek_at(2) != Some('[')
    {
        while let Some(c) = scanner.peek() {
            if c == '\n' {
                break;
            }
            scanner.bump();
        }
    }

    while let Some(c) = scanner.peek() {
        let start = scanner.byte_pos();
        let line = scanner.line;

        if c.is_whitespace() {
            scanner.bump();
            continue;
        }

        if c == '/' && scanner.peek_at(1) == Some('/') {
            while let Some(ch) = scanner.peek() {
                if ch == '\n' {
                    break;
                }
                scanner.bump();
            }
            continue;
        }
        if c == '/' && scanner.peek_at(1) == Some('*') {
            scan_block_comment(&mut scanner, &mut lexed.errors);
            continue;
        }

        if is_ident_start(c) {
            if raw_string_hashes(&scanner).is_some() {
                scan_raw_string(&mut scanner, &mut lexed.errors, line);
                lexed.tokens.push(Token {
                    kind: TokKind::Str,
                    start,
                    end: scanner.byte_pos(),
                    line,
                });
                continue;
            }
            if c == 'b' && scanner.peek_at(1) == Some('\'') {
                scan_char(&mut scanner, &mut lexed.errors, line);
                lexed.tokens.push(Token {
                    kind: TokKind::Char,
                    start,
                    end: scanner.byte_pos(),
                    line,
                });
                continue;
            }
            if (c == 'b' || c == 'c') && scanner.peek_at(1) == Some('"') {
                scan_plain_string(&mut scanner, &mut lexed.errors, line);
                lexed.tokens.push(Token {
                    kind: TokKind::Str,
                    start,
                    end: scanner.byte_pos(),
                    line,
                });
                continue;
            }
            if c == 'r'
                && scanner.peek_at(1) == Some('#')
                && scanner.peek_at(2).is_some_and(is_ident_start)
            {
                scanner.bump();
                scanner.bump();
                consume_ident(&mut scanner);
                lexed.tokens.push(Token {
                    kind: TokKind::Ident,
                    start,
                    end: scanner.byte_pos(),
                    line,
                });
                continue;
            }
            consume_ident(&mut scanner);
            lexed.tokens.push(Token {
                kind: TokKind::Ident,
                start,
                end: scanner.byte_pos(),
                line,
            });
            continue;
        }

        if c == '"' {
            scan_plain_string(&mut scanner, &mut lexed.errors, line);
            lexed.tokens.push(Token {
                kind: TokKind::Str,
                start,
                end: scanner.byte_pos(),
                line,
            });
            continue;
        }

        if c == '\'' {
            let next = scanner.peek_at(1);
            let after_next = scanner.peek_at(2);
            if next == Some('\\') || after_next == Some('\'') {
                scan_char(&mut scanner, &mut lexed.errors, line);
                lexed.tokens.push(Token {
                    kind: TokKind::Char,
                    start,
                    end: scanner.byte_pos(),
                    line,
                });
                continue;
            }
            if next.is_some_and(is_ident_start) {
                scanner.bump();
                consume_ident(&mut scanner);
                lexed.tokens.push(Token {
                    kind: TokKind::Lifetime,
                    start,
                    end: scanner.byte_pos(),
                    line,
                });
                continue;
            }
            scanner.bump();
            lexed.tokens.push(Token {
                kind: TokKind::Punct,
                start,
                end: scanner.byte_pos(),
                line,
            });
            lexed.errors.push(LexError {
                line,
                message: "stray single quote".to_string(),
            });
            continue;
        }

        if c.is_ascii_digit() {
            let prev_is_dot = match lexed.tokens.last() {
                Some(token) => token.is(src, TokKind::Punct, "."),
                None => false,
            };
            scan_number(&mut scanner, prev_is_dot);
            lexed.tokens.push(Token {
                kind: TokKind::Number,
                start,
                end: scanner.byte_pos(),
                line,
            });
            continue;
        }

        if !c.is_ascii_punctuation() {
            scanner.bump();
            lexed.tokens.push(Token {
                kind: TokKind::Punct,
                start,
                end: scanner.byte_pos(),
                line,
            });
            lexed.errors.push(LexError {
                line,
                message: "stray character".to_string(),
            });
            continue;
        }

        scan_punct(&mut scanner);
        lexed.tokens.push(Token {
            kind: TokKind::Punct,
            start,
            end: scanner.byte_pos(),
            line,
        });
    }

    lexed
}

/// Value of a string literal token text: `"..."` with escapes resolved, `r"..."` and
/// `r#"..."#` raw strings as written inside the quotes. `None` for byte strings, C strings,
/// characters and anything that is not a complete string literal.
pub fn unquote(text: &str) -> Option<String> {
    if text.starts_with('"') {
        unquote_plain(text)
    } else if text.starts_with('r') {
        unquote_raw(text)
    } else {
        None
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

struct Scanner<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
    line: u32,
}

impl<'a> Scanner<'a> {
    fn new(src: &'a str) -> Scanner<'a> {
        Scanner {
            src,
            chars: src.char_indices().collect(),
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|item| item.1)
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.pos
            .checked_add(offset)
            .and_then(|index| self.chars.get(index))
            .map(|item| item.1)
    }

    fn byte_pos(&self) -> usize {
        self.chars
            .get(self.pos)
            .map(|item| item.0)
            .unwrap_or(self.src.len())
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        if c == '\n' {
            self.line = self.line.saturating_add(1);
        }
        self.pos = self.pos.saturating_add(1);
        Some(c)
    }
}

fn consume_ident(scanner: &mut Scanner<'_>) {
    while let Some(c) = scanner.peek() {
        if !is_ident_continue(c) {
            break;
        }
        scanner.bump();
    }
}

fn scan_block_comment(scanner: &mut Scanner<'_>, errors: &mut Vec<LexError>) {
    let start_line = scanner.line;
    scanner.bump();
    scanner.bump();
    let mut depth: u32 = 1;
    while depth > 0 {
        match scanner.peek() {
            None => {
                errors.push(LexError {
                    line: start_line,
                    message: "unterminated block comment".to_string(),
                });
                break;
            }
            Some('/') if scanner.peek_at(1) == Some('*') => {
                scanner.bump();
                scanner.bump();
                depth = depth.saturating_add(1);
            }
            Some('*') if scanner.peek_at(1) == Some('/') => {
                scanner.bump();
                scanner.bump();
                depth = depth.saturating_sub(1);
            }
            Some(_) => {
                scanner.bump();
            }
        }
    }
}

/// Hash count of a raw string starting at the cursor, when it really is one.
fn raw_string_hashes(scanner: &Scanner<'_>) -> Option<usize> {
    let mut offset = match scanner.peek() {
        Some('r') => 1,
        Some('b' | 'c') if scanner.peek_at(1) == Some('r') => 2,
        _ => return None,
    };
    let mut hashes: usize = 0;
    while scanner.peek_at(offset) == Some('#') {
        hashes = hashes.checked_add(1)?;
        offset = offset.checked_add(1)?;
    }
    if scanner.peek_at(offset) == Some('"') {
        Some(hashes)
    } else {
        None
    }
}

fn scan_plain_string(scanner: &mut Scanner<'_>, errors: &mut Vec<LexError>, start_line: u32) {
    // The cursor is on the opening quote or on a one-letter prefix (`b`, `c`).
    if scanner.peek() != Some('"') {
        scanner.bump();
    }
    scanner.bump();
    loop {
        match scanner.peek() {
            None => {
                errors.push(LexError {
                    line: start_line,
                    message: "unterminated string literal".to_string(),
                });
                break;
            }
            Some('"') => {
                scanner.bump();
                break;
            }
            Some('\\') => {
                scanner.bump();
                if scanner.peek().is_some() {
                    scanner.bump();
                }
            }
            Some(_) => {
                scanner.bump();
            }
        }
    }
}

fn scan_raw_string(scanner: &mut Scanner<'_>, errors: &mut Vec<LexError>, start_line: u32) {
    match scanner.peek() {
        Some('r') => {
            scanner.bump();
        }
        Some('b' | 'c') => {
            scanner.bump();
            scanner.bump();
        }
        _ => {}
    }
    let mut hashes: usize = 0;
    while scanner.peek() == Some('#') {
        hashes = hashes.saturating_add(1);
        scanner.bump();
    }
    scanner.bump();
    loop {
        match scanner.peek() {
            None => {
                errors.push(LexError {
                    line: start_line,
                    message: "unterminated raw string literal".to_string(),
                });
                break;
            }
            Some('"') => {
                let closes = (1..=hashes).all(|offset| scanner.peek_at(offset) == Some('#'));
                if closes {
                    scanner.bump();
                    for _ in 0..hashes {
                        scanner.bump();
                    }
                    break;
                }
                scanner.bump();
            }
            Some(_) => {
                scanner.bump();
            }
        }
    }
}

fn scan_char(scanner: &mut Scanner<'_>, errors: &mut Vec<LexError>, start_line: u32) {
    // The cursor is on the opening quote or on the `b` prefix.
    if scanner.peek() != Some('\'') {
        scanner.bump();
    }
    scanner.bump();
    loop {
        match scanner.peek() {
            None => {
                errors.push(LexError {
                    line: start_line,
                    message: "unterminated character literal".to_string(),
                });
                break;
            }
            Some('\'') => {
                scanner.bump();
                break;
            }
            Some('\\') => {
                scanner.bump();
                if scanner.peek().is_some() {
                    scanner.bump();
                }
            }
            Some(_) => {
                scanner.bump();
            }
        }
    }
}

fn scan_number(scanner: &mut Scanner<'_>, prev_is_dot: bool) {
    if scanner.peek() == Some('0') {
        let base = scanner.peek_at(1);
        if base == Some('x') || base == Some('o') || base == Some('b') {
            scanner.bump();
            scanner.bump();
            while let Some(c) = scanner.peek() {
                let digit = match base {
                    Some('x') => c.is_ascii_hexdigit(),
                    Some('o') => ('0'..='7').contains(&c),
                    _ => c == '0' || c == '1',
                };
                if !digit && c != '_' {
                    break;
                }
                scanner.bump();
            }
            scan_suffix(scanner);
            return;
        }
    }

    while let Some(c) = scanner.peek() {
        if !c.is_ascii_digit() && c != '_' {
            break;
        }
        scanner.bump();
    }

    // A dot starts a fraction only before a digit, and never right after a dot token.
    if !prev_is_dot
        && scanner.peek() == Some('.')
        && scanner.peek_at(1).is_some_and(|c| c.is_ascii_digit())
    {
        scanner.bump();
        while let Some(c) = scanner.peek() {
            if !c.is_ascii_digit() && c != '_' {
                break;
            }
            scanner.bump();
        }
    }

    scan_exponent(scanner);
    scan_suffix(scanner);
}

fn scan_exponent(scanner: &mut Scanner<'_>) {
    if scanner.peek() != Some('e') && scanner.peek() != Some('E') {
        return;
    }
    let first = scanner.peek_at(1);
    let valid = match first {
        Some(c) if c.is_ascii_digit() => true,
        Some('+' | '-') => scanner.peek_at(2).is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    };
    if !valid {
        return;
    }
    scanner.bump();
    if scanner.peek() == Some('+') || scanner.peek() == Some('-') {
        scanner.bump();
    }
    while let Some(c) = scanner.peek() {
        if !c.is_ascii_digit() && c != '_' {
            break;
        }
        scanner.bump();
    }
}

fn scan_suffix(scanner: &mut Scanner<'_>) {
    if scanner.peek().is_some_and(is_ident_start) {
        consume_ident(scanner);
    }
}

fn scan_punct(scanner: &mut Scanner<'_>) {
    let triple = (scanner.peek(), scanner.peek_at(1), scanner.peek_at(2));
    let count = if triple == (Some('<'), Some('<'), Some('='))
        || triple == (Some('>'), Some('>'), Some('='))
        || triple == (Some('.'), Some('.'), Some('='))
        || triple == (Some('.'), Some('.'), Some('.'))
    {
        3
    } else if matches!(
        (scanner.peek(), scanner.peek_at(1)),
        (Some(':'), Some(':'))
            | (Some('-'), Some('>'))
            | (Some('='), Some('>'))
            | (Some('='), Some('='))
            | (Some('!'), Some('='))
            | (Some('<'), Some('='))
            | (Some('>'), Some('='))
            | (Some('&'), Some('&'))
            | (Some('|'), Some('|'))
            | (Some('+'), Some('='))
            | (Some('-'), Some('='))
            | (Some('*'), Some('='))
            | (Some('/'), Some('='))
            | (Some('%'), Some('='))
            | (Some('^'), Some('='))
            | (Some('&'), Some('='))
            | (Some('|'), Some('='))
            | (Some('.'), Some('.'))
    ) {
        2
    } else {
        1
    };
    for _ in 0..count {
        scanner.bump();
    }
}

fn unquote_plain(text: &str) -> Option<String> {
    let mut chars = text.char_indices().peekable();
    match chars.next() {
        Some((_, '"')) => {}
        _ => return None,
    }
    let mut out = String::new();
    loop {
        let (_, c) = chars.next()?;
        match c {
            '"' => {
                if chars.next().is_some() {
                    return None;
                }
                return Some(out);
            }
            '\\' => {
                let (_, esc) = chars.next()?;
                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '0' => out.push('\0'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    'x' => {
                        let (_, high) = chars.next()?;
                        let (_, low) = chars.next()?;
                        let value = high
                            .to_digit(16)?
                            .checked_mul(16)?
                            .checked_add(low.to_digit(16)?)?;
                        if value > 0x7F {
                            return None;
                        }
                        out.push(char::from_u32(value)?);
                    }
                    'u' => {
                        match chars.next() {
                            Some((_, '{')) => {}
                            _ => return None,
                        }
                        let mut value: u32 = 0;
                        let mut digits: usize = 0;
                        loop {
                            let (_, d) = chars.next()?;
                            if d == '}' {
                                break;
                            }
                            let digit = d.to_digit(16)?;
                            digits = digits.checked_add(1)?;
                            if digits > 6 {
                                return None;
                            }
                            value = value.checked_mul(16)?.checked_add(digit)?;
                        }
                        if digits == 0 {
                            return None;
                        }
                        out.push(char::from_u32(value)?);
                    }
                    '\n' => {
                        skip_following_whitespace(&mut chars);
                    }
                    '\r' => {
                        if let Some(&(_, '\n')) = chars.peek() {
                            chars.next();
                        }
                        skip_following_whitespace(&mut chars);
                    }
                    _ => return None,
                }
            }
            _ => out.push(c),
        }
    }
}

fn skip_following_whitespace(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
    while let Some(&(_, c)) = chars.peek() {
        if !c.is_whitespace() {
            break;
        }
        chars.next();
    }
}

fn unquote_raw(text: &str) -> Option<String> {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, 'r')) => {}
        _ => return None,
    }
    let mut hashes: usize = 0;
    let content_start = loop {
        let (index, c) = chars.next()?;
        match c {
            '#' => {
                hashes = hashes.checked_add(1)?;
            }
            '"' => break index.checked_add(1)?,
            _ => return None,
        }
    };
    let mut closing = String::new();
    closing.push('"');
    for _ in 0..hashes {
        closing.push('#');
    }
    let rest = text.get(content_start..)?;
    if !rest.ends_with(closing.as_str()) {
        return None;
    }
    let content_len = rest.len().checked_sub(closing.len())?;
    let content = rest.get(..content_len)?;
    if content.contains(closing.as_str()) {
        return None;
    }
    Some(content.to_owned())
}
