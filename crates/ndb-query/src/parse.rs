//! Recursive-descent parser — token stream → name-based AST.
//!
//! One-token lookahead. No backtracking. Each grammar rule from §3 of
//! the query-language working spec maps to a `parse_*` method.
//!
//! Grammar reminder (current locked form):
//!
//! ```text
//! query           = [ "as" "of" snapshot ] match_clause
//!                   [ where_clause ] return_clause [ limit_clause ]
//! match_clause    = "match" pattern { pattern }
//! pattern         = ident [ recursion ] "(" [ binding_list ] ")" [ "as" var ]
//! recursion       = "*" | "+" | "?" | "{" int "," int "}"
//! binding_list    = binding { "," binding }
//! binding         = ident ":" term
//! where_clause    = "where" or_expr
//! or_expr         = and_expr { "or" and_expr }
//! and_expr        = not_expr { "and" not_expr }
//! not_expr        = [ "not" ] primary
//! primary         = cmp | "(" or_expr ")"
//! cmp             = term cmp_op term       (* non-associative *)
//! return_clause   = "return" var { "," var } [ "," ]
//! limit_clause    = "limit" int
//! snapshot        = int | string
//! term            = var | underscore | literal
//! ```

use ndb_engine::JsonValue;

use crate::ast::{
    NameAsOf, NameBinding, NameCmpOp, NameExpr, NameOrderKey, NamePattern, NameQuery, NameRecursion, NameReturn,
    NameTerm,
};
use crate::error::{ParseError, Span};
use crate::lex::{Tok, TokKind, lex};

const MAX_RECURSION_BOUND: u32 = 64;

/// Parse `source` into a name-based AST. The resolver step (engine side)
/// maps names → ids and produces the wire AST.
pub fn parse_query(source: &str) -> Result<NameQuery, ParseError> {
    let tokens = lex(source)?;
    let mut p = Parser::new(tokens);
    p.parse_query()
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Self { toks, pos: 0 }
    }

    // -----------------------------------------------------------------
    // Cursor helpers
    // -----------------------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn peek_kind(&self) -> &TokKind {
        &self.toks[self.pos].kind
    }

    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if !matches!(t.kind, TokKind::Eof) {
            self.pos += 1;
        }
        t
    }

    fn check(&self, kind: &TokKind) -> bool {
        std::mem::discriminant(self.peek_kind()) == std::mem::discriminant(kind)
    }

    fn eat(&mut self, kind: &TokKind) -> Option<Tok> {
        if self.check(kind) {
            Some(self.advance())
        } else {
            None
        }
    }

    fn expect(&mut self, kind: &TokKind, expected: &str) -> Result<Tok, ParseError> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        let t = self.peek();
        if matches!(t.kind, TokKind::Eof) {
            ParseError::UnexpectedEof {
                expected: expected.into(),
                span: t.span,
            }
        } else {
            ParseError::Unexpected {
                expected: expected.into(),
                found: t.kind.describe(),
                span: t.span,
            }
        }
    }

    // -----------------------------------------------------------------
    // query = [ "as" "of" snapshot ] match_clause [ where ] return [ limit ]
    // -----------------------------------------------------------------

    fn parse_query(&mut self) -> Result<NameQuery, ParseError> {
        let start = self.peek().span.start;

        let as_of = if self.check(&TokKind::As) {
            // Disambiguate: `as ?var` is a self-bind, only legal inside a
            // pattern. At top level, `as` can ONLY start `as of <snapshot>`.
            self.advance();
            self.expect(&TokKind::Of, "`of`")?;
            Some(self.parse_snapshot()?)
        } else {
            None
        };

        self.expect(&TokKind::Match, "`match`")?;
        let patterns = self.parse_patterns()?;
        if patterns.is_empty() {
            return Err(self.unexpected("at least one pattern after `match`"));
        }

        let filter = if self.eat(&TokKind::Where).is_some() {
            Some(self.parse_or()?)
        } else {
            None
        };

        self.expect(&TokKind::Return, "`return`")?;
        let returns = self.parse_returns()?;

        // Optional `order by key [asc|desc], key [asc|desc], ...`
        let order_by = if self.eat(&TokKind::Order).is_some() {
            self.expect(&TokKind::By, "`by`")?;
            self.parse_order_keys()?
        } else {
            Vec::new()
        };

        let limit = if self.eat(&TokKind::Limit).is_some() {
            let t = self.advance();
            match t.kind {
                TokKind::IntLit(n) if n >= 0 => Some(usize::try_from(n).unwrap_or(usize::MAX)),
                other => {
                    return Err(ParseError::Unexpected {
                        expected: "non-negative integer".into(),
                        found: other.describe(),
                        span: t.span,
                    });
                }
            }
        } else {
            None
        };

        // After limit (or last clause), expect EOF.
        if !matches!(self.peek_kind(), TokKind::Eof) {
            return Err(self.unexpected("end of input"));
        }
        let end = self.peek().span.start;

        Ok(NameQuery {
            as_of,
            patterns,
            filter,
            returns,
            order_by,
            limit,
            span: Span::range(start, end),
        })
    }

    fn parse_snapshot(&mut self) -> Result<NameAsOf, ParseError> {
        let t = self.advance();
        match t.kind {
            TokKind::IntLit(n) if n >= 0 => {
                #[allow(clippy::cast_sign_loss)] // guarded by n >= 0
                Ok(NameAsOf::TxId(n as u64))
            }
            TokKind::StrLit(s) => Ok(NameAsOf::Timestamp(s)),
            other => Err(ParseError::Unexpected {
                expected: "non-negative integer (tx_id) or string (timestamp)".into(),
                found: other.describe(),
                span: t.span,
            }),
        }
    }

    // -----------------------------------------------------------------
    // patterns = pattern { pattern }
    // pattern  = ident [ recursion ] "(" binding_list? ")" [ "as" var ]
    // -----------------------------------------------------------------

    fn parse_patterns(&mut self) -> Result<Vec<NamePattern>, ParseError> {
        let mut out = Vec::new();
        while self.is_pattern_start() {
            out.push(self.parse_pattern()?);
        }
        Ok(out)
    }

    fn is_pattern_start(&self) -> bool {
        matches!(self.peek_kind(), TokKind::Ident(_))
    }

    fn parse_pattern(&mut self) -> Result<NamePattern, ParseError> {
        let type_tok = self.advance();
        let (type_name, type_span) = match type_tok.kind {
            TokKind::Ident(s) => (s, type_tok.span),
            _ => unreachable!("guarded by is_pattern_start"),
        };
        let start = type_span.start;

        // Recursion suffix BEFORE `(` per the locked grammar.
        let recursion = self.parse_recursion_suffix()?;

        self.expect(&TokKind::LParen, "`(`")?;
        let bindings = if matches!(self.peek_kind(), TokKind::RParen) {
            Vec::new()
        } else {
            self.parse_binding_list()?
        };
        let rparen = self.expect(&TokKind::RParen, "`)`")?;

        // Optional `as ?var` self-bind.
        let mut end = rparen.span.end();
        let self_var = if self.check(&TokKind::As) {
            // Lookahead: `as` followed by Var is a self-bind. Anything else
            // means we don't own the `as` here — roll back so the outer
            // parser can interpret it (likely a syntax error at this layer
            // since `as of` is only legal before `match`).
            let save = self.pos;
            self.advance();
            if let TokKind::Var(name) = self.peek_kind() {
                let name = name.clone();
                let vtok = self.advance();
                end = vtok.span.end();
                Some(name)
            } else {
                self.pos = save;
                None
            }
        } else {
            None
        };

        Ok(NamePattern {
            type_name,
            type_name_span: type_span,
            recursion,
            bindings,
            self_var,
            span: Span::range(start, end),
        })
    }

    fn parse_recursion_suffix(&mut self) -> Result<Option<NameRecursion>, ParseError> {
        match self.peek_kind() {
            TokKind::Star => {
                self.advance();
                Ok(Some(NameRecursion::Star))
            }
            TokKind::Plus => {
                self.advance();
                Ok(Some(NameRecursion::Plus))
            }
            TokKind::Question => {
                self.advance();
                Ok(Some(NameRecursion::Optional))
            }
            TokKind::LBrace => {
                let lbrace = self.advance();
                let min = self.expect_unsigned_int("recursion lower bound")?;
                self.expect(&TokKind::Comma, "`,`")?;
                let max = self.expect_unsigned_int("recursion upper bound")?;
                let rbrace = self.expect(&TokKind::RBrace, "`}`")?;
                if min > max || max > MAX_RECURSION_BOUND {
                    return Err(ParseError::RecursionBoundsInvalid {
                        min,
                        max,
                        span: Span::range(lbrace.span.start, rbrace.span.end()),
                    });
                }
                Ok(Some(NameRecursion::Bounded { min, max }))
            }
            _ => Ok(None),
        }
    }

    fn expect_unsigned_int(&mut self, expected: &str) -> Result<u32, ParseError> {
        let t = self.advance();
        match t.kind {
            TokKind::IntLit(n) if n >= 0 => {
                u32::try_from(n).map_err(|_| ParseError::Unexpected {
                    expected: expected.into(),
                    found: format!("{n}"),
                    span: t.span,
                })
            }
            other => Err(ParseError::Unexpected {
                expected: expected.into(),
                found: other.describe(),
                span: t.span,
            }),
        }
    }

    fn parse_binding_list(&mut self) -> Result<Vec<NameBinding>, ParseError> {
        let mut out = vec![self.parse_binding()?];
        while self.eat(&TokKind::Comma).is_some() {
            // Allow trailing comma: `,` followed by `)` ends the list.
            if matches!(self.peek_kind(), TokKind::RParen) {
                break;
            }
            out.push(self.parse_binding()?);
        }
        Ok(out)
    }

    fn parse_binding(&mut self) -> Result<NameBinding, ParseError> {
        let t = self.advance();
        let (name, span) = match t.kind {
            TokKind::Ident(s) => (s, t.span),
            other => {
                return Err(ParseError::Unexpected {
                    expected: "binding name (identifier)".into(),
                    found: other.describe(),
                    span: t.span,
                });
            }
        };
        self.expect(&TokKind::Colon, "`:`")?;
        let term = self.parse_term()?;
        Ok(NameBinding {
            name,
            name_span: span,
            term,
        })
    }

    fn parse_term(&mut self) -> Result<NameTerm, ParseError> {
        let t = self.advance();
        match t.kind {
            TokKind::Var(name) => Ok(NameTerm::Var {
                name,
                span: t.span,
            }),
            TokKind::Underscore => Ok(NameTerm::Anonymous { span: t.span }),
            TokKind::StrLit(s) => Ok(NameTerm::Literal {
                value: JsonValue::String { value: s },
                span: t.span,
            }),
            TokKind::IntLit(n) => Ok(NameTerm::Literal {
                value: JsonValue::I64 { value: n },
                span: t.span,
            }),
            TokKind::FloatLit(f) => Ok(NameTerm::Literal {
                value: JsonValue::F64 { value: f },
                span: t.span,
            }),
            TokKind::True => Ok(NameTerm::Literal {
                value: JsonValue::Bool { value: true },
                span: t.span,
            }),
            TokKind::False => Ok(NameTerm::Literal {
                value: JsonValue::Bool { value: false },
                span: t.span,
            }),
            TokKind::Null => Ok(NameTerm::Literal {
                value: JsonValue::Null,
                span: t.span,
            }),
            TokKind::UuidLit(s) => Ok(NameTerm::Literal {
                value: JsonValue::Uuid { value: s },
                span: t.span,
            }),
            other => Err(ParseError::Unexpected {
                expected: "variable, `_`, or literal".into(),
                found: other.describe(),
                span: t.span,
            }),
        }
    }

    // -----------------------------------------------------------------
    // Filter expression — or > and > not > comparison
    // -----------------------------------------------------------------

    fn parse_or(&mut self) -> Result<NameExpr, ParseError> {
        let mut left = self.parse_and()?;
        while self.eat(&TokKind::Or).is_some() {
            let right = self.parse_and()?;
            left = NameExpr::Or {
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<NameExpr, ParseError> {
        let mut left = self.parse_not()?;
        while self.eat(&TokKind::And).is_some() {
            let right = self.parse_not()?;
            left = NameExpr::And {
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<NameExpr, ParseError> {
        if let Some(not_tok) = self.eat(&TokKind::Not) {
            let inner = self.parse_not()?;
            return Ok(NameExpr::Not {
                inner: Box::new(inner),
                span: not_tok.span,
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<NameExpr, ParseError> {
        if self.eat(&TokKind::LParen).is_some() {
            let inner = self.parse_or()?;
            self.expect(&TokKind::RParen, "`)`")?;
            return Ok(inner);
        }
        let left = self.parse_term()?;
        let (op, op_span) = self.parse_cmp_op()?;
        let right = self.parse_term()?;

        // Non-associative: forbid a second comparison after this one
        // without an `and` / `or` between.
        if self.is_cmp_op() {
            return Err(ParseError::ChainedComparison {
                span: self.peek().span,
            });
        }

        Ok(NameExpr::Cmp {
            left,
            op,
            right,
            span: op_span,
        })
    }

    fn parse_cmp_op(&mut self) -> Result<(NameCmpOp, Span), ParseError> {
        let t = self.advance();
        let op = match t.kind {
            TokKind::Eq => NameCmpOp::Eq,
            TokKind::Ne => NameCmpOp::Ne,
            TokKind::Lt => NameCmpOp::Lt,
            TokKind::Le => NameCmpOp::Le,
            TokKind::Gt => NameCmpOp::Gt,
            TokKind::Ge => NameCmpOp::Ge,
            other => {
                return Err(ParseError::Unexpected {
                    expected: "comparison operator".into(),
                    found: other.describe(),
                    span: t.span,
                });
            }
        };
        Ok((op, t.span))
    }

    fn is_cmp_op(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokKind::Eq
                | TokKind::Ne
                | TokKind::Lt
                | TokKind::Le
                | TokKind::Gt
                | TokKind::Ge
        )
    }

    // -----------------------------------------------------------------
    // Return list
    // -----------------------------------------------------------------

    fn parse_returns(&mut self) -> Result<Vec<NameReturn>, ParseError> {
        let mut out = vec![self.parse_return_one()?];
        while self.eat(&TokKind::Comma).is_some() {
            // Allow trailing comma: stop if we're at order/limit/eof.
            if matches!(self.peek_kind(), TokKind::Order | TokKind::Limit | TokKind::Eof) {
                break;
            }
            out.push(self.parse_return_one()?);
        }
        Ok(out)
    }

    fn parse_order_keys(&mut self) -> Result<Vec<NameOrderKey>, ParseError> {
        let mut out = vec![self.parse_order_one()?];
        while self.eat(&TokKind::Comma).is_some() {
            if matches!(self.peek_kind(), TokKind::Limit | TokKind::Eof) { break; }
            out.push(self.parse_order_one()?);
        }
        Ok(out)
    }

    fn parse_order_one(&mut self) -> Result<NameOrderKey, ParseError> {
        let t = self.advance();
        let name = match t.kind {
            TokKind::Var(name) => name,
            other => return Err(ParseError::Unexpected {
                expected: "variable in order-by list".into(),
                found: other.describe(),
                span: t.span,
            }),
        };
        let (property, end_span) = if self.eat(&TokKind::Dot).is_some() {
            let prop_tok = self.advance();
            match prop_tok.kind {
                TokKind::Ident(s) => (Some(s), prop_tok.span),
                other => return Err(ParseError::Unexpected {
                    expected: "property name after `.` in order-by".into(),
                    found: other.describe(),
                    span: prop_tok.span,
                }),
            }
        } else {
            (None, t.span)
        };
        // Optional direction.
        let descending = if self.eat(&TokKind::Desc).is_some() {
            true
        } else {
            self.eat(&TokKind::Asc);  // ascending is the default; consume but ignore
            false
        };
        let combined_span = crate::error::Span::range(t.span.start, end_span.end());
        Ok(NameOrderKey { name, property, descending, span: combined_span })
    }

    fn parse_return_one(&mut self) -> Result<NameReturn, ParseError> {
        let t = self.advance();
        let name = match t.kind {
            TokKind::Var(name) => name,
            other => return Err(ParseError::Unexpected {
                expected: "variable in return list".into(),
                found: other.describe(),
                span: t.span,
            }),
        };
        // Optional `.identifier` property projection.
        let (property, end_span) = if self.eat(&TokKind::Dot).is_some() {
            let prop_tok = self.advance();
            match prop_tok.kind {
                TokKind::Ident(s) => (Some(s), prop_tok.span),
                other => return Err(ParseError::Unexpected {
                    expected: "property name after `.` in return projection".into(),
                    found: other.describe(),
                    span: prop_tok.span,
                }),
            }
        } else {
            (None, t.span)
        };
        // Span covers `?v` through the optional `.prop`.
        let combined_span = crate::error::Span::range(t.span.start, end_span.end());
        Ok(NameReturn { name, property, span: combined_span })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(s: &str) -> NameQuery {
        parse_query(s).unwrap_or_else(|e| panic!("expected ok, got {e:?}"))
    }

    fn parse_err(s: &str) -> ParseError {
        parse_query(s).unwrap_err()
    }

    #[test]
    fn minimal_query_one_entity_pattern() {
        let q = parse_ok("match customer(name: ?n) return ?n");
        assert_eq!(q.as_of, None);
        assert_eq!(q.patterns.len(), 1);
        assert_eq!(q.patterns[0].type_name, "customer");
        assert_eq!(q.patterns[0].bindings.len(), 1);
        assert_eq!(q.patterns[0].bindings[0].name, "name");
        assert_eq!(q.returns.len(), 1);
        assert_eq!(q.returns[0].name, "n");
        assert!(q.filter.is_none());
        assert!(q.limit.is_none());
    }

    #[test]
    fn empty_pattern_parens() {
        let q = parse_ok("match customer() return ?nothing");
        assert!(q.patterns[0].bindings.is_empty());
    }

    #[test]
    fn self_bind_via_as_var() {
        let q = parse_ok("match customer(name: ?n) as ?c return ?c");
        assert_eq!(q.patterns[0].self_var.as_deref(), Some("c"));
    }

    #[test]
    fn multi_pattern_join_via_shared_var() {
        let q = parse_ok(
            r"match
                 sales_order(customer: ?c, amount: ?a)
                 customer(name: ?n) as ?c
               return ?c, ?n, ?a",
        );
        assert_eq!(q.patterns.len(), 2);
        assert_eq!(q.patterns[0].type_name, "sales_order");
        assert_eq!(q.patterns[1].type_name, "customer");
        assert_eq!(q.patterns[1].self_var.as_deref(), Some("c"));
        assert_eq!(q.returns.len(), 3);
    }

    #[test]
    fn where_clause_with_and_or_not() {
        let q = parse_ok(
            r#"match customer(name: ?n) as ?c
               where ?n = "alice" or ?n = "bob" and not ?n = "system"
               return ?c"#,
        );
        let expr = q.filter.expect("filter");
        // Top should be Or because of precedence (and > or).
        assert!(matches!(expr, NameExpr::Or { .. }));
    }

    #[test]
    fn chained_comparison_forbidden() {
        let err = parse_err("match a(x: ?p) where ?p < ?q < ?r return ?p");
        assert!(matches!(err, ParseError::ChainedComparison { .. }));
    }

    #[test]
    fn recursion_star_prefix() {
        let q = parse_ok(
            "match contains*(parent: uuid:01923c00-0000-7000-8000-000000000001, child: ?leaf) return ?leaf",
        );
        assert_eq!(q.patterns[0].recursion, Some(NameRecursion::Star));
        assert_eq!(q.patterns[0].bindings.len(), 2);
        // Verify literal got into Term::Literal as a Uuid value.
        match &q.patterns[0].bindings[0].term {
            NameTerm::Literal {
                value: JsonValue::Uuid { value },
                ..
            } => {
                assert_eq!(value, "01923c00-0000-7000-8000-000000000001");
            }
            other => panic!("expected uuid literal, got {other:?}"),
        }
    }

    #[test]
    fn recursion_plus_question_bounded() {
        let q = parse_ok("match contains+(p: ?a, c: ?b) return ?b");
        assert_eq!(q.patterns[0].recursion, Some(NameRecursion::Plus));
        let q = parse_ok("match contains?(p: ?a, c: ?b) return ?b");
        assert_eq!(q.patterns[0].recursion, Some(NameRecursion::Optional));
        let q = parse_ok("match contains{2,5}(p: ?a, c: ?b) return ?b");
        assert_eq!(
            q.patterns[0].recursion,
            Some(NameRecursion::Bounded { min: 2, max: 5 })
        );
    }

    #[test]
    fn recursion_bounds_invalid_min_gt_max() {
        let err = parse_err("match contains{5,2}(p: ?a, c: ?b) return ?b");
        assert!(matches!(
            err,
            ParseError::RecursionBoundsInvalid { min: 5, max: 2, .. }
        ));
    }

    #[test]
    fn recursion_bounds_invalid_over_cap() {
        let err = parse_err("match contains{0,1000}(p: ?a, c: ?b) return ?b");
        assert!(matches!(
            err,
            ParseError::RecursionBoundsInvalid {
                min: 0,
                max: 1000,
                ..
            }
        ));
    }

    #[test]
    fn as_of_tx_id() {
        let q = parse_ok("as of 42 match customer(x: ?p) return ?p");
        assert_eq!(q.as_of, Some(NameAsOf::TxId(42)));
    }

    #[test]
    fn as_of_timestamp_string() {
        let q = parse_ok(r#"as of "2026-05-27T00:00:00Z" match a(x: ?p) return ?p"#);
        assert_eq!(
            q.as_of,
            Some(NameAsOf::Timestamp("2026-05-27T00:00:00Z".into()))
        );
    }

    #[test]
    fn limit_clause() {
        let q = parse_ok("match a(x: ?p) return ?p limit 100");
        assert_eq!(q.limit, Some(100));
    }

    #[test]
    fn anonymous_underscore_in_binding() {
        let q = parse_ok("match prescription(prescriber: _) as ?rx return ?rx");
        assert!(matches!(
            q.patterns[0].bindings[0].term,
            NameTerm::Anonymous { .. }
        ));
    }

    #[test]
    fn trailing_commas_in_bindings_and_returns() {
        let q = parse_ok("match a(x: ?p, y: ?q,) return ?p, ?q,");
        assert_eq!(q.patterns[0].bindings.len(), 2);
        assert_eq!(q.returns.len(), 2);
    }

    #[test]
    fn missing_match_keyword_errors() {
        let err = parse_err("customer(x: ?p) return ?p");
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn missing_return_errors() {
        let err = parse_err("match a(x: ?p)");
        assert!(matches!(
            err,
            ParseError::UnexpectedEof { .. } | ParseError::Unexpected { .. }
        ));
    }

    #[test]
    fn negative_limit_errors() {
        let err = parse_err("match a(x: ?p) return ?p limit -3");
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn full_medical_query_parses() {
        // The motivating §12.6 example — closes spec §5.7's role-vs-property
        // case at parse time (resolver decides per-name later).
        let q = parse_ok(
            r#"match
                 diagnosis(patient: ?p, symptom: "fever", pathogen: ?d)
                 diagnosis(patient: ?p, symptom: "rash",  pathogen: ?d)
                 treatment(disease: ?d, medication: ?med, contraindication: ?a)
                 patient_record(known_allergy: ?a) as ?p
               return ?p, ?med, ?a"#,
        );
        assert_eq!(q.patterns.len(), 4);
        assert_eq!(q.returns.len(), 3);
        // Patterns 0 and 1 are both "diagnosis" but with literal differences
        // ("fever" vs "rash"); planner will join on shared ?p and ?d.
        assert_eq!(q.patterns[0].type_name, "diagnosis");
        assert_eq!(q.patterns[1].type_name, "diagnosis");
        assert_eq!(q.patterns[2].type_name, "treatment");
        assert_eq!(q.patterns[3].type_name, "patient_record");
        assert_eq!(q.patterns[3].self_var.as_deref(), Some("p"));
    }

    #[test]
    fn parenthesised_filter_inverts_precedence() {
        // (?a or ?b) and ?c — without parens, `and` would bind tighter.
        let q = parse_ok(
            r#"match a(x: ?p) where (?p = "x" or ?p = "y") and ?p != "z" return ?p"#,
        );
        let expr = q.filter.expect("filter");
        // After parens, top should be And.
        assert!(matches!(expr, NameExpr::And { .. }));
    }

    #[test]
    fn pattern_with_comments_parses() {
        let q = parse_ok(
            r"# pick customers
               match customer(name: ?n)  # one pattern
               return ?n                 # one return
               # trailing comment",
        );
        assert_eq!(q.patterns.len(), 1);
    }

    #[test]
    fn span_locates_into_source() {
        let src = "match\n  customer(name: ?n)\nreturn ?n";
        let q = parse_ok(src);
        let span = q.patterns[0].type_name_span;
        let (line, col, _) = span.locate(src);
        assert_eq!((line, col), (2, 3));
    }
}
