//! Span + error types for the query-language lexer and parser.
//!
//! Spans are byte offsets into the source text. We carry both the offset
//! and the length so error rendering can underline the offending token
//! precisely. Line/column are computed on demand from the source — keeping
//! the span minimal lets us produce spans cheaply at every token and only
//! pay the line-walk cost when actually rendering an error.

use thiserror::Error;

/// A contiguous byte range in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first byte of the token.
    pub start: usize,
    /// Length of the token in bytes.
    pub len: usize,
}

impl Span {
    /// Span covering a single byte at `pos`.
    #[must_use]
    pub const fn point(pos: usize) -> Self {
        Self { start: pos, len: 1 }
    }

    /// Span covering bytes `[start, end)`. `end` must be `≥ start`.
    #[must_use]
    pub const fn range(start: usize, end: usize) -> Self {
        Self {
            start,
            len: end.saturating_sub(start),
        }
    }

    /// One-past-last byte of the span.
    #[must_use]
    pub const fn end(self) -> usize {
        self.start + self.len
    }

    /// Convert this span to a 1-indexed (line, column, length) triple
    /// relative to `source`. The column is in Unicode scalar values from
    /// the line start, not bytes — that's the natural unit for editors.
    /// Returns `(line, column, char_len)`.
    #[must_use]
    pub fn locate(self, source: &str) -> (usize, usize, usize) {
        let mut line = 1usize;
        let mut col = 1usize;
        let mut byte_idx = 0;
        let mut chars = source.chars();
        // Walk to span start, counting lines/cols.
        while byte_idx < self.start {
            match chars.next() {
                Some('\n') => {
                    line += 1;
                    col = 1;
                    byte_idx += 1;
                }
                Some(c) => {
                    col += 1;
                    byte_idx += c.len_utf8();
                }
                None => break,
            }
        }
        // Count chars in the span.
        let mut span_chars = 0usize;
        let mut counted = 0usize;
        while counted < self.len {
            match chars.next() {
                Some(c) => {
                    span_chars += 1;
                    counted += c.len_utf8();
                }
                None => break,
            }
        }
        (line, col, span_chars)
    }
}

/// Errors raised by the lexer or parser. Every variant carries a span
/// into the source text so callers can render `(line, col, length)`
/// against the original input via [`Span::locate`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ParseError {
    /// Lexer: unexpected character.
    #[error("lex_error: unexpected character {ch:?}")]
    UnexpectedChar {
        /// Offending character.
        ch: char,
        /// Source location.
        span: Span,
    },
    /// Lexer: unterminated string literal.
    #[error("lex_error: unterminated string literal")]
    UnterminatedString {
        /// Source location of the opening quote.
        span: Span,
    },
    /// Lexer: invalid escape sequence inside a string.
    #[error("lex_error: invalid escape sequence")]
    InvalidEscape {
        /// Source location of the backslash.
        span: Span,
    },
    /// Lexer: malformed number literal.
    #[error("lex_error: malformed number {literal:?}")]
    BadNumber {
        /// The offending text.
        literal: String,
        /// Source location.
        span: Span,
    },
    /// Lexer: malformed `uuid:` literal.
    #[error("lex_error: malformed uuid literal {literal:?}")]
    BadUuid {
        /// The offending text.
        literal: String,
        /// Source location.
        span: Span,
    },
    /// Parser: unexpected token. `expected` lists the alternatives the
    /// parser would have accepted; `found` describes what it got.
    #[error("parse_error: expected {expected}, found {found}")]
    Unexpected {
        /// What the parser would have accepted.
        expected: String,
        /// What it got instead.
        found: String,
        /// Source location of the offending token.
        span: Span,
    },
    /// Parser: end of input where more was expected.
    #[error("parse_error: unexpected end of input — expected {expected}")]
    UnexpectedEof {
        /// What the parser would have accepted.
        expected: String,
        /// One-past-last byte of the source.
        span: Span,
    },
    /// Parser: `{n,m}` bounds with `n > m` or `m > MAX`.
    #[error("parse_error: recursion bounds invalid — min={min} max={max}")]
    RecursionBoundsInvalid {
        /// Lower bound supplied by the user.
        min: u32,
        /// Upper bound supplied by the user.
        max: u32,
        /// Source location of the bounded recursion suffix.
        span: Span,
    },
    /// Parser: comparison ops are non-associative; `a < b < c` is illegal.
    #[error("parse_error: chained comparison is not allowed; use `and`")]
    ChainedComparison {
        /// Source location of the second comparison operator.
        span: Span,
    },
}

impl ParseError {
    /// Return the source span associated with this error.
    #[must_use]
    pub const fn span(&self) -> Span {
        match *self {
            Self::UnexpectedChar { span, .. }
            | Self::UnterminatedString { span }
            | Self::InvalidEscape { span }
            | Self::BadNumber { span, .. }
            | Self::BadUuid { span, .. }
            | Self::Unexpected { span, .. }
            | Self::UnexpectedEof { span, .. }
            | Self::RecursionBoundsInvalid { span, .. }
            | Self::ChainedComparison { span } => span,
        }
    }

    /// Short error code identifying the failure class — matches the codes
    /// in §6 of the query-language working spec.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match *self {
            Self::UnexpectedChar { .. }
            | Self::UnterminatedString { .. }
            | Self::InvalidEscape { .. }
            | Self::BadNumber { .. }
            | Self::BadUuid { .. } => "lex_error",
            Self::Unexpected { .. }
            | Self::UnexpectedEof { .. }
            | Self::RecursionBoundsInvalid { .. }
            | Self::ChainedComparison { .. } => "parse_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_first_line_first_col() {
        let s = "match";
        let (line, col, len) = Span { start: 0, len: 5 }.locate(s);
        assert_eq!((line, col, len), (1, 1, 5));
    }

    #[test]
    fn locate_after_newline() {
        let s = "match\n  customer";
        // 'customer' starts at byte 8 (after "match\n  ").
        let (line, col, len) = Span { start: 8, len: 8 }.locate(s);
        assert_eq!((line, col, len), (2, 3, 8));
    }

    #[test]
    fn locate_multibyte_chars() {
        let s = "match\n  # ăn cơm\n  customer";
        let start = s.find("customer").unwrap();
        let (line, col, len) = Span { start, len: 8 }.locate(s);
        // line 3, col 3 (after two spaces).
        assert_eq!((line, col, len), (3, 3, 8));
    }
}
