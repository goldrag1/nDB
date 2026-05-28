//! Query-language wire AST (`POST /query` body).
#![allow(clippy::doc_markdown)]
//!
//! See `docs/superpowers/specs/2026-05-27-query-language.md` for the
//! authoritative working spec; this module implements §4 of that spec.
//!
//! Locked design (2026-05-27):
//!
//! - **ID-based.** `type_id`, `role_id`, `property_id` are `u32` dictionary
//!   slots — same convention as every other wire shape in this crate.
//!   Clients with a dictionary cache can build this directly; clients
//!   without one post surface text and let the server's resolver fill in
//!   the ids.
//! - **Tagged unions follow the existing convention.** `Pattern`, `Term`,
//!   `Expr`, and `Recursion` use `#[serde(tag = "kind", rename_all =
//!   "snake_case")]`, matching the way `JsonRecord` is tagged in
//!   `crate::wire`. `AsOf` uses `untagged` because the discriminator IS
//!   the field name (`tx_id` vs `timestamp_us`) per the spec example.
//! - **No NaN-style flags.** `Option<T>` carries "absent" cleanly; serde
//!   skips serialising `None`.
//! - **Round-trip is exact** — every `QueryRequest` produced from typed
//!   fields parses back to the same struct (tested below per variant).
//!
//! What this module deliberately does NOT do:
//!
//! - Parse text → AST (lives in the `ndb-query` crate).
//! - Resolve names → ids (lives in the engine resolver, not yet built).
//! - Plan / execute (planner and executor, not yet built).
//!
//! It is purely the data interchange shape between client and server.

use serde::{Deserialize, Serialize};

use crate::wire::JsonValue;

/// Default cap on recursion depth — see §5.3 of the query-language spec.
/// Enforced at execution time, not serialisation time.
pub const DEFAULT_MAX_RECURSION_DEPTH: u32 = 64;

// ---------------------------------------------------------------------------
// Top-level request / response
// ---------------------------------------------------------------------------

/// `POST /query` request body — the wire AST of one query.
///
/// Round-trip example:
///
/// ```rust
/// use ndb_engine::wire_query::{QueryRequest, Pattern};
/// let q = QueryRequest {
///     as_of: None,
///     patterns: vec![],
///     filter: None,
///     returns: vec!["p".into()],
///     limit: Some(10),
/// };
/// let s = serde_json::to_string(&q).unwrap();
/// let parsed: QueryRequest = serde_json::from_str(&s).unwrap();
/// assert_eq!(parsed.returns, q.returns);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryRequest {
    /// Snapshot selector. `None` = engine's latest committed tx.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<AsOf>,

    /// Pattern atoms — at least one is required for a meaningful query
    /// (an empty list is technically legal but always returns zero rows).
    /// The planner picks the join order; authoring order is not significant.
    pub patterns: Vec<Pattern>,

    /// Optional `where` clause — boolean expression tree over bound
    /// variables. All variables referenced here must be bound by some
    /// pattern; the resolver enforces this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Expr>,

    /// Variables (and optional property projections) to return. Order
    /// is preserved in the response's `columns` array. Each entry is
    /// either a bare variable name (`?v`) or a path projection
    /// (`?v.property_name`). For backward compatibility, a JSON string
    /// `"v"` deserializes as the bare-variable case — existing callers
    /// don't need to change their wire payloads.
    pub returns: Vec<ReturnItem>,

    /// Optional cap on the number of result tuples returned. `None`
    /// means no cap. Servers may enforce an implementation-defined hard
    /// cap regardless — see §6.4 of the query-language spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// One entry in [`QueryRequest::returns`].
///
/// Wire shape is **untagged** for backward compatibility:
///
/// - `"v"` (JSON string) deserializes as [`ReturnItem::Variable`] —
///   project the variable's bound value as-is. For self-bound entity
///   / hyperedge variables, that's the UUID; for role-bound terms
///   it's whatever scalar landed there.
/// - `{"variable": "v", "property": 30, "display": "name"}` (JSON
///   object) deserializes as [`ReturnItem::Path`] — follow the bound
///   UUID to its record, look up `property` by id, project that
///   value. `display` is the human-readable property name, used as
///   the column header in the response. `display` is optional on the
///   wire; the resolver always populates it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ReturnItem {
    /// `?var` — project the binding's value directly.
    Variable(String),
    /// `?var.property_name` — UUID-bound variable + property name.
    Path {
        /// Bound variable name.
        variable: String,
        /// Resolved property id.
        property: u32,
        /// Optional human-readable property name, used as the column header.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
}

impl ReturnItem {
    /// Column-header label for this projection.
    #[must_use]
    pub fn column_name(&self) -> String {
        match self {
            Self::Variable(name) => name.clone(),
            Self::Path { variable, property, display } => match display {
                Some(d) => format!("{variable}.{d}"),
                None    => format!("{variable}.{property}"),
            },
        }
    }

    /// Variable this item refers to.
    #[must_use]
    pub fn variable_name(&self) -> &str {
        match self {
            Self::Variable(name) => name,
            Self::Path { variable, .. } => variable,
        }
    }
}

impl From<&str> for ReturnItem {
    fn from(s: &str) -> Self { Self::Variable(s.to_string()) }
}
impl From<String> for ReturnItem {
    fn from(s: String) -> Self { Self::Variable(s) }
}

/// `POST /query` response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResponse {
    /// Column names mirroring `QueryRequest::returns`.
    pub columns: Vec<String>,

    /// One row per result tuple. Each row has `columns.len()` entries
    /// in the same order.
    pub rows: Vec<Vec<JsonValue>>,

    /// `true` if the result was capped by `limit` or by the
    /// implementation-defined hard cap.
    #[serde(default)]
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// Snapshot selector (as of T)
// ---------------------------------------------------------------------------

/// Snapshot selector for a query — corresponds to the `as of <expr>` clause.
///
/// Serde shape — untagged, distinguished by the present field name:
///
/// - `{"tx_id": 42}` — pin to a specific transaction id.
/// - `{"timestamp_us": 1700000000000000}` — latest tx with
///   `commit_timestamp_us ≤ T`. (Timestamp-form support depends on the
///   engine tracking commit timestamps; in v1 only the `tx_id` form is
///   wired end to end.)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum AsOf {
    /// Pin to a specific transaction id.
    TxId {
        /// Transaction id snapshot point.
        tx_id: u64,
    },
    /// Pin to the latest transaction at or before a given timestamp
    /// (microseconds since Unix epoch).
    TimestampUs {
        /// Microseconds since Unix epoch.
        timestamp_us: i64,
    },
}

// ---------------------------------------------------------------------------
// Pattern atoms
// ---------------------------------------------------------------------------

/// One pattern atom in a `match` clause.
///
/// - **Entity** patterns match assertion records by type and property
///   filters. They can self-bind the entity's UUID to a variable via
///   `self_var`.
/// - **Hyperedge** patterns match assertion records by type, role
///   bindings, property filters, and an optional recursive walk. They
///   can self-bind the hyperedge's UUID to a variable via `self_var`.
///
/// The resolver decides which kind a surface-syntax pattern resolves to,
/// based on the type-name dictionary entry. Ambiguous types
/// (name registered as both entity and hyperedge) raise a resolver error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pattern {
    /// `customer(name: ?n, region: "Vietnam") as ?cust`
    Entity {
        /// Type dictionary id (must be `≠ 0`).
        type_id: u32,
        /// Optional variable to capture the entity's own UUID.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        self_var: Option<String>,
        /// Property filters scoped to this entity.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        property_filters: Vec<PropertyFilter>,
    },
    /// `approval(document: ?d, approver: ?a) as ?app`
    Hyperedge {
        /// Type dictionary id (must be `≠ 0`).
        type_id: u32,
        /// Optional variable to capture the hyperedge's own UUID.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        self_var: Option<String>,
        /// Role bindings — one per role of the hyperedge.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        role_bindings: Vec<RoleBinding>,
        /// Property filters on the hyperedge's own properties.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        property_filters: Vec<PropertyFilter>,
        /// Optional recursion modifier — `*`, `+`, `?`, `{n,m}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recursion: Option<Recursion>,
    },
}

/// One role binding inside a hyperedge pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleBinding {
    /// Role dictionary id (must be `≠ 0`).
    pub role_id: u32,
    /// Term bound to this role — variable or literal.
    pub term: Term,
}

/// One property match inside a pattern. The RHS is a `Term`:
///
/// - `term = Var { name }` + `op = Eq`: BIND — the variable is bound to
///   the property value at match time. Used for `customer(name: ?n)`.
/// - `term = Literal { value }` + `op = Eq`: equality FILTER. Used for
///   `customer(region: "Vietnam")`.
/// - `term = Literal { value }` + other `op`: ordered FILTER. The parser
///   only emits `Eq` for v1; clients building the wire AST manually may
///   emit other ops (e.g. for planner stress tests).
/// - `term = Var { name }` + other `op`: ambiguous semantically; the
///   executor treats it as `bind + comparison constraint`, but the
///   parser doesn't produce this — variable-vs-variable / inequality
///   filters go through the where-clause path per spec §5.7.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PropertyFilter {
    /// Property dictionary id (must be `≠ 0`).
    pub property_id: u32,
    /// Comparison operator.
    pub op: CmpOp,
    /// Right-hand side — variable (bind) or literal (filter).
    pub term: Term,
}

/// Recursion modifier on a hyperedge pattern.
///
/// - `Star` — zero-or-more steps (transitive closure including self).
/// - `Plus` — one-or-more steps (transitive closure excluding self).
/// - `Optional` — zero-or-one steps.
/// - `Bounded { min, max }` — inclusive bounds, `min ≤ max`, `max ≤ 64`.
///
/// `max_depth` carries the cycle-protection cap. The executor enforces
/// it as an absolute ceiling regardless of `Bounded { min, max }`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recursion {
    /// `type*` — zero-or-more.
    Star {
        /// Hard cap on traversal depth; default `DEFAULT_MAX_RECURSION_DEPTH`.
        #[serde(default = "default_max_depth")]
        max_depth: u32,
    },
    /// `type+` — one-or-more.
    Plus {
        /// Hard cap on traversal depth; default `DEFAULT_MAX_RECURSION_DEPTH`.
        #[serde(default = "default_max_depth")]
        max_depth: u32,
    },
    /// `type?` — zero-or-one.
    Optional,
    /// `type{n,m}` — bounded inclusive range.
    Bounded {
        /// Minimum number of steps.
        min: u32,
        /// Maximum number of steps (must be `≤ DEFAULT_MAX_RECURSION_DEPTH`).
        max: u32,
    },
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_RECURSION_DEPTH
}

// ---------------------------------------------------------------------------
// Terms (var | literal)
// ---------------------------------------------------------------------------

/// A term inside a pattern role binding or a filter comparison.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Term {
    /// `?name` — a variable. Binding rules are in §5.1 of the spec.
    Var {
        /// Variable name (without the leading `?`).
        name: String,
    },
    /// A literal value — re-uses the engine's tagged-union `JsonValue`.
    Literal {
        /// The literal value.
        value: JsonValue,
    },
}

// ---------------------------------------------------------------------------
// Filter expression tree (where clause)
// ---------------------------------------------------------------------------

/// Boolean expression tree for `where`-clause filtering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expr {
    /// `left and right`.
    And {
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `left or right`.
    Or {
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `not inner`.
    Not {
        /// Wrapped expression.
        inner: Box<Expr>,
    },
    /// `left op right` — `=`, `!=`, `<`, `<=`, `>`, `>=`.
    Cmp {
        /// Left operand (variable or literal).
        left: Term,
        /// Comparison operator.
        op: CmpOp,
        /// Right operand (variable or literal).
        right: Term,
    },
}

/// Comparison operators used in property filters and filter expressions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
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
}

impl CmpOp {
    /// Symbolic form used in surface syntax + error messages.
    #[must_use]
    pub const fn as_symbol(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — round-trip every variant + spec example shape
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lit_str(s: &str) -> JsonValue {
        JsonValue::String {
            value: s.to_string(),
        }
    }
    fn lit_i64(n: i64) -> JsonValue {
        JsonValue::I64 { value: n }
    }
    fn var(name: &str) -> Term {
        Term::Var {
            name: name.to_string(),
        }
    }
    fn lit_term(v: JsonValue) -> Term {
        Term::Literal { value: v }
    }

    fn round_trip<T>(value: T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let s = serde_json::to_string(&value).expect("serialise");
        serde_json::from_str(&s).expect("parse")
    }

    #[test]
    fn cmp_op_serializes_snake_case() {
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            let s = serde_json::to_string(&op).unwrap();
            assert!(s.starts_with('"') && s.ends_with('"'), "got {s}");
            let back: CmpOp = serde_json::from_str(&s).unwrap();
            assert_eq!(op, back);
        }
        assert_eq!(serde_json::to_string(&CmpOp::Eq).unwrap(), "\"eq\"");
        assert_eq!(serde_json::to_string(&CmpOp::Ne).unwrap(), "\"ne\"");
        assert_eq!(serde_json::to_string(&CmpOp::Lt).unwrap(), "\"lt\"");
        assert_eq!(serde_json::to_string(&CmpOp::Le).unwrap(), "\"le\"");
        assert_eq!(serde_json::to_string(&CmpOp::Gt).unwrap(), "\"gt\"");
        assert_eq!(serde_json::to_string(&CmpOp::Ge).unwrap(), "\"ge\"");
        assert_eq!(CmpOp::Eq.as_symbol(), "=");
        assert_eq!(CmpOp::Le.as_symbol(), "<=");
    }

    #[test]
    fn as_of_round_trips() {
        let a = AsOf::TxId { tx_id: 42 };
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(s, r#"{"tx_id":42}"#);
        assert_eq!(round_trip(a), a);

        let b = AsOf::TimestampUs {
            timestamp_us: 1_700_000_000_000_000,
        };
        let s = serde_json::to_string(&b).unwrap();
        assert_eq!(s, r#"{"timestamp_us":1700000000000000}"#);
        assert_eq!(round_trip(b), b);
    }

    #[test]
    fn term_round_trips_both_kinds() {
        let v = var("p");
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, r#"{"kind":"var","name":"p"}"#);
        assert_eq!(round_trip(v.clone()), v);

        let l = lit_term(lit_str("fever"));
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(s, r#"{"kind":"literal","value":{"tag":"string","value":"fever"}}"#);
        assert_eq!(round_trip(l.clone()), l);
    }

    #[test]
    fn property_filter_round_trips_literal_term() {
        let pf = PropertyFilter {
            property_id: 30,
            op: CmpOp::Eq,
            term: lit_term(lit_str("fever")),
        };
        let s = serde_json::to_string(&pf).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["property_id"], 30);
        assert_eq!(v["op"], "eq");
        assert_eq!(v["term"]["kind"], "literal");
        assert_eq!(v["term"]["value"]["tag"], "string");
        assert_eq!(v["term"]["value"]["value"], "fever");
        assert_eq!(round_trip(pf.clone()), pf);
    }

    #[test]
    fn property_filter_round_trips_var_term() {
        // `customer(name: ?n)` shape — bind variable to property value.
        let pf = PropertyFilter {
            property_id: 31,
            op: CmpOp::Eq,
            term: var("n"),
        };
        let s = serde_json::to_string(&pf).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["term"]["kind"], "var");
        assert_eq!(v["term"]["name"], "n");
        assert_eq!(round_trip(pf.clone()), pf);
    }

    #[test]
    fn role_binding_round_trips() {
        let rb = RoleBinding {
            role_id: 10,
            term: var("p"),
        };
        let s = serde_json::to_string(&rb).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["role_id"], 10);
        assert_eq!(v["term"]["kind"], "var");
        assert_eq!(v["term"]["name"], "p");
        assert_eq!(round_trip(rb.clone()), rb);
    }

    #[test]
    fn entity_pattern_round_trips() {
        let p = Pattern::Entity {
            type_id: 100,
            self_var: Some("cust".into()),
            property_filters: vec![PropertyFilter {
                property_id: 41,
                op: CmpOp::Eq,
                term: lit_term(lit_str("Vietnam")),
            }],
        };
        let s = serde_json::to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "entity");
        assert_eq!(v["type_id"], 100);
        assert_eq!(v["self_var"], "cust");
        assert_eq!(v["property_filters"][0]["property_id"], 41);
        assert_eq!(round_trip(p.clone()), p);
    }

    #[test]
    fn hyperedge_pattern_round_trips_with_recursion() {
        let p = Pattern::Hyperedge {
            type_id: 200,
            self_var: None,
            role_bindings: vec![
                RoleBinding {
                    role_id: 10,
                    term: lit_term(JsonValue::Uuid {
                        value: "01923c00-0000-7000-8000-000000000001".into(),
                    }),
                },
                RoleBinding {
                    role_id: 11,
                    term: var("leaf"),
                },
            ],
            property_filters: vec![],
            recursion: Some(Recursion::Star { max_depth: 64 }),
        };
        let s = serde_json::to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "hyperedge");
        assert_eq!(v["type_id"], 200);
        assert!(v["self_var"].is_null());
        assert_eq!(v["role_bindings"].as_array().unwrap().len(), 2);
        assert_eq!(v["recursion"]["kind"], "star");
        assert_eq!(v["recursion"]["max_depth"], 64);
        assert_eq!(round_trip(p.clone()), p);
    }

    #[test]
    fn recursion_all_kinds_round_trip() {
        for r in [
            Recursion::Star { max_depth: 64 },
            Recursion::Plus { max_depth: 16 },
            Recursion::Optional,
            Recursion::Bounded { min: 1, max: 3 },
        ] {
            assert_eq!(round_trip(r), r);
        }
    }

    #[test]
    fn recursion_optional_serializes_without_extra_fields() {
        let s = serde_json::to_string(&Recursion::Optional).unwrap();
        assert_eq!(s, r#"{"kind":"optional"}"#);
    }

    #[test]
    fn recursion_star_default_max_depth() {
        // Wire JSON omitting max_depth must default to DEFAULT_MAX_RECURSION_DEPTH.
        let r: Recursion = serde_json::from_str(r#"{"kind":"star"}"#).unwrap();
        assert_eq!(
            r,
            Recursion::Star {
                max_depth: DEFAULT_MAX_RECURSION_DEPTH
            }
        );
    }

    #[test]
    fn expr_compound_round_trip() {
        // (?amt > 1000) AND NOT (?amt = 9999)
        let e = Expr::And {
            left: Box::new(Expr::Cmp {
                left: var("amt"),
                op: CmpOp::Gt,
                right: lit_term(lit_i64(1000)),
            }),
            right: Box::new(Expr::Not {
                inner: Box::new(Expr::Cmp {
                    left: var("amt"),
                    op: CmpOp::Eq,
                    right: lit_term(lit_i64(9999)),
                }),
            }),
        };
        let s = serde_json::to_string(&e).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "and");
        assert_eq!(v["left"]["kind"], "cmp");
        assert_eq!(v["right"]["kind"], "not");
        assert_eq!(v["right"]["inner"]["kind"], "cmp");
        assert_eq!(round_trip(e.clone()), e);
    }

    #[test]
    fn expr_or_round_trip() {
        let e = Expr::Or {
            left: Box::new(Expr::Cmp {
                left: var("p"),
                op: CmpOp::Eq,
                right: lit_term(lit_str("alice")),
            }),
            right: Box::new(Expr::Cmp {
                left: var("p"),
                op: CmpOp::Eq,
                right: lit_term(lit_str("bob")),
            }),
        };
        assert_eq!(round_trip(e.clone()), e);
    }

    #[test]
    fn full_query_request_round_trips_spec_example_shape() {
        // Mirrors §4 of the query-language spec.
        let q = QueryRequest {
            as_of: Some(AsOf::TxId { tx_id: 42 }),
            patterns: vec![
                Pattern::Hyperedge {
                    type_id: 200,
                    self_var: None,
                    role_bindings: vec![
                        RoleBinding {
                            role_id: 10,
                            term: var("p"),
                        },
                        RoleBinding {
                            role_id: 11,
                            term: lit_term(lit_str("fever")),
                        },
                        RoleBinding {
                            role_id: 12,
                            term: var("disease"),
                        },
                    ],
                    property_filters: vec![],
                    recursion: None,
                },
                Pattern::Hyperedge {
                    type_id: 200,
                    self_var: None,
                    role_bindings: vec![
                        RoleBinding {
                            role_id: 10,
                            term: var("p"),
                        },
                        RoleBinding {
                            role_id: 11,
                            term: lit_term(lit_str("rash")),
                        },
                        RoleBinding {
                            role_id: 12,
                            term: var("disease"),
                        },
                    ],
                    property_filters: vec![],
                    recursion: None,
                },
            ],
            filter: Some(Expr::Cmp {
                left: var("amt"),
                op: CmpOp::Gt,
                right: lit_term(lit_i64(1000)),
            }),
            returns: vec!["p".into(), "disease".into()],
            limit: Some(100),
        };
        assert_eq!(round_trip(q.clone()), q);
    }

    #[test]
    fn empty_optional_fields_omitted_on_serialize() {
        let q = QueryRequest {
            as_of: None,
            patterns: vec![],
            filter: None,
            returns: vec!["x".into()],
            limit: None,
        };
        let s = serde_json::to_string(&q).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("as_of").is_none(), "as_of must be omitted");
        assert!(v.get("filter").is_none(), "filter must be omitted");
        assert!(v.get("limit").is_none(), "limit must be omitted");
        assert!(v.get("patterns").is_some(), "patterns must be present");
        assert_eq!(v["returns"], json!(["x"]));
    }

    #[test]
    fn query_response_round_trips() {
        let r = QueryResponse {
            columns: vec!["p".into(), "name".into()],
            rows: vec![
                vec![
                    JsonValue::Uuid {
                        value: "01923c00-0000-7000-8000-000000000001".into(),
                    },
                    JsonValue::String {
                        value: "Alice".into(),
                    },
                ],
                vec![
                    JsonValue::Uuid {
                        value: "01923c00-0000-7000-8000-000000000002".into(),
                    },
                    JsonValue::String {
                        value: "Bob".into(),
                    },
                ],
            ],
            truncated: true,
        };
        assert_eq!(round_trip(r.clone()), r);
    }

    #[test]
    fn pattern_with_empty_vecs_skipped_on_serialize() {
        let p = Pattern::Entity {
            type_id: 100,
            self_var: None,
            property_filters: vec![],
        };
        let s = serde_json::to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(
            v.get("self_var").is_none(),
            "absent optional must not serialize"
        );
        assert!(
            v.get("property_filters").is_none(),
            "empty vec must not serialize"
        );
    }
}
