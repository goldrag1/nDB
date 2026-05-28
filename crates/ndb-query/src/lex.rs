//! Lexer — text → token stream with spans.
//!
//! Token shapes locked by §3 of the query-language working spec.
//!
//! Trickier lexer choices:
//!
//! - **`?` is context-sensitive.** `?ident` is a variable; bare `?`
//!   followed by anything else is the recursion-suffix token `Question`.
//!   The lexer peeks one char ahead to decide.
//! - **`-` is part of a number, not a unary operator.** The grammar has
//!   no arithmetic in v1, so `-3` is a single negative-number token.
//! - **Identifiers are case-insensitive at the keyword level.** The
//!   lexer matches reserved words case-insensitively but stores
//!   identifiers verbatim (the parser only checks reserved status).
//! - **Comments start with `#` and run to end-of-line.** Discarded by
//!   the lexer.
//! - **UUID literals are `uuid:HEX-...-...`** — the lexer recognises the
//!   `uuid:` prefix and consumes the canonical UUID form. We validate
//!   length and hyphen positions cheaply here; full UUID parsing is the
//!   resolver's job.

use crate::error::{ParseError, Span};

/// One lexed token plus its source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    /// Token kind + payload.
    pub kind: TokKind,
    /// Source location.
    pub span: Span,
}

/// All token kinds the parser understands.
#[derive(Debug, Clone, PartialEq)]
pub enum TokKind {
    /// `order` keyword.
    Order,
    /// `by` keyword.
    By,
    /// `asc` keyword.
    Asc,
    /// `desc` keyword.
    Desc,
    // Reserved words (case-insensitive on input)
    /// `match`
    Match,
    /// `where`
    Where,
    /// `return`
    Return,
    /// `limit`
    Limit,
    /// `as`
    As,
    /// `of`
    Of,
    /// `and`
    And,
    /// `or`
    Or,
    /// `not`
    Not,
    /// `true`
    True,
    /// `false`
    False,
    /// `null`
    Null,

    /// Identifier — type/role/property/keyword-ish name.
    Ident(String),
    /// `?ident` — variable name without the `?`.
    Var(String),
    /// `_` — anonymous variable placeholder.
    Underscore,

    /// String literal — payload is the unescaped contents.
    StrLit(String),
    /// Integer literal.
    IntLit(i64),
    /// Floating-point literal.
    FloatLit(f64),
    /// UUID literal — payload is the canonical UUID text (validated for shape).
    UuidLit(String),

    /// Punctuation `(`.
    LParen,
    /// Punctuation `)`.
    RParen,
    /// Punctuation `{`.
    LBrace,
    /// Punctuation `}`.
    RBrace,
    /// Punctuation `,`.
    Comma,
    /// Punctuation `:`.
    Colon,
    /// Punctuation `.` — used in property projection (`?var.prop`).
    Dot,

    // Operators
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,

    // Recursion suffixes
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `?` — when not followed by an identifier (variables consume the `?`).
    Question,

    /// End-of-input sentinel.
    Eof,
}

impl TokKind {
    /// Human-readable name for error messages.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Match => "`match`".into(),
            Self::Where => "`where`".into(),
            Self::Return => "`return`".into(),
            Self::Limit => "`limit`".into(),
            Self::Order => "`order`".into(),
            Self::By    => "`by`".into(),
            Self::Asc   => "`asc`".into(),
            Self::Desc  => "`desc`".into(),
            Self::As => "`as`".into(),
            Self::Of => "`of`".into(),
            Self::And => "`and`".into(),
            Self::Or => "`or`".into(),
            Self::Not => "`not`".into(),
            Self::True => "`true`".into(),
            Self::False => "`false`".into(),
            Self::Null => "`null`".into(),
            Self::Ident(s) => format!("identifier `{s}`"),
            Self::Var(s) => format!("variable `?{s}`"),
            Self::Underscore => "`_`".into(),
            Self::StrLit(_) => "string literal".into(),
            Self::IntLit(_) => "integer literal".into(),
            Self::FloatLit(_) => "float literal".into(),
            Self::UuidLit(_) => "uuid literal".into(),
            Self::LParen => "`(`".into(),
            Self::RParen => "`)`".into(),
            Self::LBrace => "`{`".into(),
            Self::RBrace => "`}`".into(),
            Self::Comma => "`,`".into(),
            Self::Colon => "`:`".into(),
            Self::Dot => "`.`".into(),
            Self::Eq => "`=`".into(),
            Self::Ne => "`!=`".into(),
            Self::Lt => "`<`".into(),
            Self::Le => "`<=`".into(),
            Self::Gt => "`>`".into(),
            Self::Ge => "`>=`".into(),
            Self::Star => "`*`".into(),
            Self::Plus => "`+`".into(),
            Self::Question => "`?`".into(),
            Self::Eof => "end of input".into(),
        }
    }
}

/// Tokenise `source` into a flat vector. EOF token is always present at
/// the end. Errors abort lexing at the offending character.
pub fn lex(source: &str) -> Result<Vec<Tok>, ParseError> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if skip_trivia(bytes, &mut i) {
            continue;
        }
        if let Some((kind, len)) = lex_punct_or_op(bytes, i)? {
            out.push(Tok {
                kind,
                span: Span { start: i, len },
            });
            i += len;
            continue;
        }
        let (tok, next) = lex_value_token(source, bytes, i, b)?;
        out.push(tok);
        i = next;
    }

    out.push(Tok {
        kind: TokKind::Eof,
        span: Span {
            start: bytes.len(),
            len: 0,
        },
    });
    Ok(out)
}

/// Skip whitespace and `#`-to-EOL comments. Returns `true` if anything was
/// consumed.
fn skip_trivia(bytes: &[u8], i: &mut usize) -> bool {
    let start = *i;
    let b = bytes[*i];
    if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
        return true;
    }
    if b == b'#' {
        while *i < bytes.len() && bytes[*i] != b'\n' {
            *i += 1;
        }
        return true;
    }
    debug_assert_eq!(start, *i);
    false
}

/// Try to lex punctuation or comparison operators. Returns `Ok(None)` if
/// the byte at `i` doesn't start one.
fn lex_punct_or_op(bytes: &[u8], i: usize) -> Result<Option<(TokKind, usize)>, ParseError> {
    let b = bytes[i];

    // Single-char punctuation that's never the prefix of a longer token.
    let single = match b {
        b'(' => Some(TokKind::LParen),
        b')' => Some(TokKind::RParen),
        b'{' => Some(TokKind::LBrace),
        b'}' => Some(TokKind::RBrace),
        b',' => Some(TokKind::Comma),
        b':' => Some(TokKind::Colon),
        // `.` is property-projection punctuation. Numeric literals
        // (`1.5`) are handled by lex_number before we get here, so
        // a `.` here is genuinely the path operator.
        b'.' => Some(TokKind::Dot),
        b'=' => Some(TokKind::Eq),
        b'*' => Some(TokKind::Star),
        b'+' => Some(TokKind::Plus),
        _ => None,
    };
    if let Some(k) = single {
        return Ok(Some((k, 1)));
    }

    // Comparison ops with optional `=` suffix.
    if b == b'<' {
        let len = if bytes.get(i + 1) == Some(&b'=') { 2 } else { 1 };
        return Ok(Some((
            if len == 2 { TokKind::Le } else { TokKind::Lt },
            len,
        )));
    }
    if b == b'>' {
        let len = if bytes.get(i + 1) == Some(&b'=') { 2 } else { 1 };
        return Ok(Some((
            if len == 2 { TokKind::Ge } else { TokKind::Gt },
            len,
        )));
    }
    if b == b'!' {
        if bytes.get(i + 1) == Some(&b'=') {
            return Ok(Some((TokKind::Ne, 2)));
        }
        return Err(ParseError::UnexpectedChar {
            ch: '!',
            span: Span::point(i),
        });
    }
    Ok(None)
}

/// Lex one value-bearing token (string, number, variable, anonymous,
/// identifier-or-keyword-or-uuid) starting at byte `i`. The caller
/// guarantees `i` doesn't point at trivia, punctuation, or an operator.
fn lex_value_token(
    source: &str,
    bytes: &[u8],
    i: usize,
    b: u8,
) -> Result<(Tok, usize), ParseError> {
    if b == b'"' {
        return lex_string(source, i);
    }
    if b == b'-' || b.is_ascii_digit() {
        if b == b'-' && !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            return Err(ParseError::UnexpectedChar {
                ch: '-',
                span: Span::point(i),
            });
        }
        return lex_number(source, i);
    }
    if b == b'?' {
        if bytes.get(i + 1).is_some_and(|&c| is_ident_start(c)) {
            return Ok(lex_var(source, i));
        }
        return Ok((
            Tok {
                kind: TokKind::Question,
                span: Span::point(i),
            },
            i + 1,
        ));
    }
    if b == b'_' && !bytes.get(i + 1).is_some_and(|&c| is_ident_cont(c)) {
        return Ok((
            Tok {
                kind: TokKind::Underscore,
                span: Span::point(i),
            },
            i + 1,
        ));
    }
    if is_ident_start(b) {
        return lex_ident_or_uuid(source, i);
    }
    // Unrecognised character — report as UTF-8 char to keep the span clean.
    let ch = source[i..].chars().next().unwrap_or('?');
    Err(ParseError::UnexpectedChar {
        ch,
        span: Span {
            start: i,
            len: ch.len_utf8(),
        },
    })
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn lex_string(source: &str, start: usize) -> Result<(Tok, usize), ParseError> {
    let bytes = source.as_bytes();
    debug_assert_eq!(bytes[start], b'"');
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Ok((
                Tok {
                    kind: TokKind::StrLit(out),
                    span: Span::range(start, i + 1),
                },
                i + 1,
            ));
        }
        if b == b'\\' {
            if i + 1 >= bytes.len() {
                return Err(ParseError::InvalidEscape {
                    span: Span::point(i),
                });
            }
            let next = bytes[i + 1];
            let ch = match next {
                b'"' => '"',
                b'\\' => '\\',
                b'n' => '\n',
                b't' => '\t',
                b'r' => '\r',
                b'0' => '\0',
                _ => {
                    return Err(ParseError::InvalidEscape {
                        span: Span { start: i, len: 2 },
                    });
                }
            };
            out.push(ch);
            i += 2;
            continue;
        }
        // Take one Unicode scalar value.
        let ch = source[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Err(ParseError::UnterminatedString {
        span: Span::point(start),
    })
}

fn lex_number(source: &str, start: usize) -> Result<(Tok, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut i = start;
    if bytes[i] == b'-' {
        i += 1;
    }
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' {
        // Look ahead: must be followed by digits to be a float.
        if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            is_float = true;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let literal = &source[start..i];
    let span = Span::range(start, i);
    if i == int_start {
        // Nothing after the sign — bare `-` shouldn't reach here, but
        // defend.
        return Err(ParseError::BadNumber {
            literal: literal.into(),
            span,
        });
    }
    if is_float {
        match literal.parse::<f64>() {
            Ok(f) => Ok((
                Tok {
                    kind: TokKind::FloatLit(f),
                    span,
                },
                i,
            )),
            Err(_) => Err(ParseError::BadNumber {
                literal: literal.into(),
                span,
            }),
        }
    } else {
        match literal.parse::<i64>() {
            Ok(n) => Ok((
                Tok {
                    kind: TokKind::IntLit(n),
                    span,
                },
                i,
            )),
            Err(_) => Err(ParseError::BadNumber {
                literal: literal.into(),
                span,
            }),
        }
    }
}

fn lex_var(source: &str, start: usize) -> (Tok, usize) {
    let bytes = source.as_bytes();
    debug_assert_eq!(bytes[start], b'?');
    let mut i = start + 1;
    while i < bytes.len() && is_ident_cont(bytes[i]) {
        i += 1;
    }
    let name = source[start + 1..i].to_string();
    (
        Tok {
            kind: TokKind::Var(name),
            span: Span::range(start, i),
        },
        i,
    )
}

fn lex_ident_or_uuid(source: &str, start: usize) -> Result<(Tok, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut i = start;
    while i < bytes.len() && is_ident_cont(bytes[i]) {
        i += 1;
    }
    let ident = &source[start..i];

    // Detect the `uuid:` prefix — followed by canonical UUID form.
    if ident.eq_ignore_ascii_case("uuid") && i < bytes.len() && bytes[i] == b':' {
        // Consume the canonical UUID form: 8-4-4-4-12 hex digits.
        let uuid_start = i + 1;
        let mut j = uuid_start;
        while j < bytes.len() {
            let c = bytes[j];
            if c.is_ascii_hexdigit() || c == b'-' {
                j += 1;
            } else {
                break;
            }
        }
        let body = &source[uuid_start..j];
        let span = Span::range(start, j);
        if is_canonical_uuid(body) {
            return Ok((
                Tok {
                    kind: TokKind::UuidLit(body.to_string()),
                    span,
                },
                j,
            ));
        }
        return Err(ParseError::BadUuid {
            literal: body.into(),
            span,
        });
    }

    let span = Span::range(start, i);
    let kind = match ident.to_ascii_lowercase().as_str() {
        "match" => TokKind::Match,
        "where" => TokKind::Where,
        "return" => TokKind::Return,
        "limit" => TokKind::Limit,
        "order" => TokKind::Order,
        "by"    => TokKind::By,
        "asc"   => TokKind::Asc,
        "desc"  => TokKind::Desc,
        "as" => TokKind::As,
        "of" => TokKind::Of,
        "and" => TokKind::And,
        "or" => TokKind::Or,
        "not" => TokKind::Not,
        "true" => TokKind::True,
        "false" => TokKind::False,
        "null" => TokKind::Null,
        _ => TokKind::Ident(ident.to_string()),
    };
    Ok((Tok { kind, span }, i))
}

fn is_canonical_uuid(s: &str) -> bool {
    // 8-4-4-4-12 with hyphens at positions 8, 13, 18, 23.
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        match idx {
            8 | 13 | 18 | 23 => {
                if b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<TokKind> {
        lex(s).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn empty_source_produces_only_eof() {
        assert_eq!(kinds(""), vec![TokKind::Eof]);
    }

    #[test]
    fn keywords_are_case_insensitive() {
        for s in ["MATCH", "Match", "match", "MaTcH"] {
            assert_eq!(kinds(s), vec![TokKind::Match, TokKind::Eof]);
        }
    }

    #[test]
    fn identifier_preserves_case() {
        assert_eq!(
            kinds("Customer"),
            vec![TokKind::Ident("Customer".into()), TokKind::Eof]
        );
    }

    #[test]
    fn variable_and_lone_question() {
        let ks = kinds("?foo ? ?bar123");
        assert_eq!(
            ks,
            vec![
                TokKind::Var("foo".into()),
                TokKind::Question,
                TokKind::Var("bar123".into()),
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn underscore_is_anonymous_var() {
        let ks = kinds("(_)");
        assert_eq!(
            ks,
            vec![
                TokKind::LParen,
                TokKind::Underscore,
                TokKind::RParen,
                TokKind::Eof
            ]
        );
    }

    #[test]
    fn underscore_inside_ident_stays_ident() {
        let ks = kinds("a_name _name name_");
        assert_eq!(
            ks,
            vec![
                TokKind::Ident("a_name".into()),
                TokKind::Ident("_name".into()),
                TokKind::Ident("name_".into()),
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn string_literal_with_escapes() {
        let ks = kinds(r#""hello\n\"world\"""#);
        assert_eq!(
            ks,
            vec![TokKind::StrLit("hello\n\"world\"".into()), TokKind::Eof]
        );
    }

    #[test]
    fn string_unicode_passthrough() {
        let ks = kinds("\"ăn cơm\"");
        assert_eq!(ks, vec![TokKind::StrLit("ăn cơm".into()), TokKind::Eof]);
    }

    #[test]
    fn integers_and_floats_and_negatives() {
        let ks = kinds("0 42 -7 3.25 -0.5");
        assert_eq!(
            ks,
            vec![
                TokKind::IntLit(0),
                TokKind::IntLit(42),
                TokKind::IntLit(-7),
                TokKind::FloatLit(3.25),
                TokKind::FloatLit(-0.5),
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn unbalanced_dot_after_number_lexes_int_then_dot() {
        // `42.` (no following digit) — number is the leading 42, the `.`
        // is then the Dot token used for property projection. The parser
        // rejects this combination; the lexer accepts both tokens.
        let toks = lex("42.").unwrap();
        let kinds: Vec<_> = toks.iter().map(|t| &t.kind).collect();
        assert!(matches!(kinds[0], TokKind::IntLit(42)));
        assert!(matches!(kinds[1], TokKind::Dot));
        assert!(matches!(kinds[2], TokKind::Eof));
    }

    #[test]
    fn comparison_operators() {
        let ks = kinds("= != < <= > >=");
        assert_eq!(
            ks,
            vec![
                TokKind::Eq,
                TokKind::Ne,
                TokKind::Lt,
                TokKind::Le,
                TokKind::Gt,
                TokKind::Ge,
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn bang_without_equals_is_error() {
        let err = lex("!").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedChar { ch: '!', .. }));
    }

    #[test]
    fn lone_dash_with_nothing_after_is_error() {
        let err = lex("-").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedChar { ch: '-', .. }));
    }

    #[test]
    fn punctuation() {
        let ks = kinds("( ) { } , : * +");
        assert_eq!(
            ks,
            vec![
                TokKind::LParen,
                TokKind::RParen,
                TokKind::LBrace,
                TokKind::RBrace,
                TokKind::Comma,
                TokKind::Colon,
                TokKind::Star,
                TokKind::Plus,
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn comments_are_stripped() {
        let ks = kinds("match # find customers\n  customer");
        assert_eq!(
            ks,
            vec![
                TokKind::Match,
                TokKind::Ident("customer".into()),
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn uuid_literal_recognised() {
        let s = "uuid:01923c00-0000-7000-8000-000000000001";
        let ks = kinds(s);
        assert_eq!(
            ks,
            vec![
                TokKind::UuidLit("01923c00-0000-7000-8000-000000000001".into()),
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn uuid_malformed_errors() {
        let err = lex("uuid:not-a-uuid").unwrap_err();
        assert!(matches!(err, ParseError::BadUuid { .. }));
    }

    #[test]
    fn unterminated_string_errors_with_span_at_open_quote() {
        let err = lex(r#""never closes"#).unwrap_err();
        match err {
            ParseError::UnterminatedString { span } => {
                assert_eq!(span.start, 0);
            }
            _ => panic!("expected UnterminatedString, got {err:?}"),
        }
    }

    #[test]
    fn invalid_escape_errors() {
        let err = lex(r#""bad \q escape""#).unwrap_err();
        assert!(matches!(err, ParseError::InvalidEscape { .. }));
    }

    #[test]
    fn full_query_lexes_clean() {
        let s = r#"match
  diagnosis(patient: ?p, symptom: "fever", pathogen: ?d)
  customer(region: "Vietnam") as ?cust
where ?amt > 1000
return ?p, ?d, ?cust
limit 100
"#;
        let toks = lex(s).unwrap();
        // 7 variable tokens: ?p ?d in diagnosis, ?cust in `as`, ?amt in where, ?p ?d ?cust in return.
        let var_count = toks
            .iter()
            .filter(|t| matches!(t.kind, TokKind::Var(_)))
            .count();
        assert_eq!(var_count, 7);
        // 2 string literals: "fever" and "Vietnam".
        let str_count = toks
            .iter()
            .filter(|t| matches!(t.kind, TokKind::StrLit(_)))
            .count();
        assert_eq!(str_count, 2);
        // 1 integer: 1000, 100 → wait, limit 100 is also a number.
        let int_count = toks
            .iter()
            .filter(|t| matches!(t.kind, TokKind::IntLit(_)))
            .count();
        assert_eq!(int_count, 2);
    }

    #[test]
    fn case_insensitive_keywords_inside_query() {
        let toks = lex("MATCH customer(x: ?p) RETURN ?p").unwrap();
        let kinds: Vec<&TokKind> = toks.iter().map(|t| &t.kind).collect();
        assert!(kinds.contains(&&TokKind::Match));
        assert!(kinds.contains(&&TokKind::Return));
    }
}
