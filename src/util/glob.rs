//! Glob patterns over repository paths and fact keys (`SPEC.md`, "Globs").
//!
//! Syntax: `*` matches any run of characters except `/`; `**` as a whole segment matches any
//! number of segments (including none); `?` matches one character except `/`; every other
//! character matches itself. There are no character classes and no escapes.

use crate::error::{Error, Result};

/// One unit of a compiled path segment.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SegmentToken {
    /// A character that matches itself.
    Literal(char),
    /// `*`: any run of characters except `/`.
    Star,
    /// `?`: one character except `/`.
    Question,
}

/// One compiled path segment of a glob.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PatternSegment {
    /// `**` as a whole segment: zero or more whole text segments.
    DoubleStar,
    /// Any other segment, matched character by character.
    Segment(Vec<SegmentToken>),
}

/// A compiled glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
    pattern: String,
    segments: Vec<PatternSegment>,
}

impl Glob {
    /// Compiles a glob. `Error::Config` when the pattern is empty, contains `\`, or uses `**`
    /// inside a segment (`a**b`).
    pub fn new(pattern: &str) -> Result<Glob> {
        if pattern.is_empty() {
            return Err(Error::Config {
                message: format!("invalid glob {pattern:?}: glob is empty"),
            });
        }
        if pattern.contains('\\') {
            return Err(Error::Config {
                message: format!("invalid glob {pattern:?}: glob contains a backslash"),
            });
        }
        for segment in pattern.split('/') {
            if segment != "**" && segment.contains("**") {
                return Err(Error::Config {
                    message: format!(
                        "invalid glob {pattern:?}: ** is only allowed as a whole path segment"
                    ),
                });
            }
        }
        let mut segments = Vec::new();
        for segment in pattern.split('/') {
            if segment == "**" {
                segments.push(PatternSegment::DoubleStar);
            } else {
                let mut tokens = Vec::new();
                for ch in segment.chars() {
                    match ch {
                        '*' => tokens.push(SegmentToken::Star),
                        '?' => tokens.push(SegmentToken::Question),
                        other => tokens.push(SegmentToken::Literal(other)),
                    }
                }
                segments.push(PatternSegment::Segment(tokens));
            }
        }
        Ok(Glob {
            pattern: pattern.to_owned(),
            segments,
        })
    }

    /// Whether `text` matches the whole pattern.
    pub fn is_match(&self, text: &str) -> bool {
        let text_segments: Vec<Vec<char>> = text
            .split('/')
            .map(|segment| segment.chars().collect())
            .collect();
        let width = text_segments.len().saturating_add(1);
        // Dynamic programming over pattern segments and text segments: `prev`
        // says which text prefixes the pattern segments seen so far can match.
        // Cost is bounded by pattern segments times text segments times the
        // cost of one segment comparison, never exponential.
        let mut prev = vec![false; width];
        if let Some(first) = prev.get_mut(0) {
            *first = true;
        }
        for segment in &self.segments {
            let mut next = vec![false; width];
            for used in 0..=text_segments.len() {
                let matched = match segment {
                    PatternSegment::DoubleStar => {
                        // Match no further segment, or consume one more segment.
                        let zero = prev.get(used).copied().unwrap_or(false);
                        let consumed = match used.checked_sub(1) {
                            Some(previous) => next.get(previous).copied().unwrap_or(false),
                            None => false,
                        };
                        zero || consumed
                    }
                    PatternSegment::Segment(tokens) => match used.checked_sub(1) {
                        Some(previous) => {
                            let prefix = prev.get(previous).copied().unwrap_or(false);
                            let segment_matches = match text_segments.get(previous) {
                                Some(text_segment) => segment_is_match(tokens, text_segment),
                                None => false,
                            };
                            prefix && segment_matches
                        }
                        None => false,
                    },
                };
                if let Some(slot) = next.get_mut(used) {
                    *slot = matched;
                }
            }
            prev = next;
        }
        prev.get(text_segments.len()).copied().unwrap_or(false)
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.pattern
    }
}

/// Whether one pattern segment matches one text segment.
///
/// Two-row dynamic programming over characters, so the cost is bounded by
/// pattern length times text length. The text segment never contains `/`
/// because the caller split on `/`, which is what keeps `*` and `?` from
/// matching a separator.
fn segment_is_match(tokens: &[SegmentToken], text: &[char]) -> bool {
    let width = text.len().saturating_add(1);
    let mut prev = vec![false; width];
    if let Some(first) = prev.get_mut(0) {
        *first = true;
    }
    for token in tokens {
        let mut next = vec![false; width];
        for used in 0..=text.len() {
            let matched = match token {
                SegmentToken::Star => {
                    // Match no character, or consume one more character.
                    let zero = prev.get(used).copied().unwrap_or(false);
                    let consumed = match used.checked_sub(1) {
                        Some(previous) => next.get(previous).copied().unwrap_or(false),
                        None => false,
                    };
                    zero || consumed
                }
                SegmentToken::Question => match used.checked_sub(1) {
                    Some(previous) => {
                        let prefix = prev.get(previous).copied().unwrap_or(false);
                        let one_char = match text.get(previous) {
                            Some(ch) => *ch != '/',
                            None => false,
                        };
                        prefix && one_char
                    }
                    None => false,
                },
                SegmentToken::Literal(expected) => match used.checked_sub(1) {
                    Some(previous) => {
                        let prefix = prev.get(previous).copied().unwrap_or(false);
                        let same = match text.get(previous) {
                            Some(ch) => ch == expected,
                            None => false,
                        };
                        prefix && same
                    }
                    None => false,
                },
            };
            if let Some(slot) = next.get_mut(used) {
                *slot = matched;
            }
        }
        prev = next;
    }
    prev.get(text.len()).copied().unwrap_or(false)
}

/// Several globs; matches when any of them matches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobSet {
    globs: Vec<Glob>,
}

impl GlobSet {
    /// Compiles every pattern; fails on the first invalid one.
    pub fn new(patterns: &[String]) -> Result<GlobSet> {
        let mut globs = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            globs.push(Glob::new(pattern)?);
        }
        Ok(GlobSet { globs })
    }

    /// Whether any glob matches `text`.
    pub fn is_match(&self, text: &str) -> bool {
        self.globs.iter().any(|glob| glob.is_match(text))
    }

    /// Whether the set has no globs.
    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Glob;

    fn matches(pattern: &str, text: &str) -> bool {
        Glob::new(pattern).expect("valid glob").is_match(text)
    }

    #[test]
    fn star_stays_within_a_segment() {
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(!matches("src/*.rs", "src/a/main.rs"));
        assert!(matches("*orders*", "list_orders"));
        assert!(!matches("*orders*", "web/src/api/orders.ts::load"));
    }

    #[test]
    fn double_star_crosses_segments() {
        assert!(matches("src/**/*.rs", "src/main.rs"));
        assert!(matches("src/**/*.rs", "src/a/b/main.rs"));
        assert!(matches("**", "a/b/c"));
        assert!(matches("**/*orders*", "web/src/api/orders.ts::load"));
        assert!(
            Glob::new("**orders**").is_err(),
            "** must be a whole segment"
        );
    }

    #[test]
    fn question_mark_is_one_character() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "a/c"));
        assert!(!matches("a?c", "ac"));
    }
}
