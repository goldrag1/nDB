//! Name-based AST — what the parser produces, before the engine
//! resolver maps names to dictionary ids.
//!
//! Mirrors `ndb_engine::wire_query` shape but keeps type / role /
//! property fields as **strings**. Variables in `Term::Var` are the
//! name without the leading `?` (consistent with the wire AST).
//!
//! The resolver step (engine-side, not in this crate) walks a
//! `NameQuery` and produces a `ndb_engine::QueryRequest`. Names that
//! don't resolve become semantic errors at that stage. The parser
//! itself never touches a dictionary.

use ndb_engine::JsonValue;

use crate::error::Span;

/// Top-level parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct NameQuery {
    /// `as of <snapshot>` clause, or `None` if absent.
    pub as_of: Option<NameAsOf>,
    /// `match` patterns, in source order.
    pub patterns: Vec<NamePattern>,
    /// `where` clause, or `None`.
    pub filter: Option<NameExpr>,
    /// `return` variable list. Empty for write-only queries that don't
    /// project any results.
    pub returns: Vec<NameReturn>,
    /// `order by` key list, in source order. Empty when absent. Sort
    /// is stable so multiple keys behave like SQL `ORDER BY a, b, c`.
    pub order_by: Vec<NameOrderKey>,
    /// `limit N`, or `None`.
    pub limit: Option<usize>,
    /// `create` clauses, in source order. Executed AFTER `match` (so
    /// role-fillers can reference bound variables) and AFTER `delete`.
    pub creates: Vec<NameCreate>,
    /// `delete` clauses, in source order. Variables MUST be bound by
    /// a `match` pattern. Executed BEFORE `create` so a single query
    /// can replace data atomically.
    pub deletes: Vec<NameDelete>,
    /// `set` assignments, in source order. Variables MUST be bound by
    /// a `match` pattern. Each `set` reads the current record, writes
    /// a new assertion with the named property replaced.
    pub sets: Vec<NameSet>,
    /// `merge` clauses (upsert). In source order. Resolved AFTER
    /// `match` so merge bindings can reference earlier `?vars`.
    pub merges: Vec<NameMerge>,
    /// Overall span of the query (start of first token through end of last).
    pub span: Span,
}

/// Snapshot selector — parser-shape.
///
/// `as of 42` → `TxId(42)`.
/// `as of "2026-05-27T00:00:00Z"` → `Timestamp("...")` (RFC3339; resolver
///   parses into microseconds-since-epoch).
#[derive(Debug, Clone, PartialEq)]
pub enum NameAsOf {
    /// Integer transaction id.
    TxId(u64),
    /// RFC3339-style timestamp string (unparsed at this layer).
    Timestamp(String),
}

/// One pattern atom — `type[recursion](bindings) as ?self`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamePattern {
    /// Type name as written in the source.
    pub type_name: String,
    /// Span of the type-name token.
    pub type_name_span: Span,
    /// Recursion suffix, if any (`*`, `+`, `?`, `{n,m}`).
    pub recursion: Option<NameRecursion>,
    /// Role / property bindings inside `(...)`.
    pub bindings: Vec<NameBinding>,
    /// `as ?var` self-bind, if any.
    pub self_var: Option<String>,
    /// Span of the whole pattern (type name through `as ?var` if present).
    pub span: Span,
}

/// One binding inside a pattern's parens.
#[derive(Debug, Clone, PartialEq)]
pub struct NameBinding {
    /// Binding name (role OR property — the resolver decides).
    pub name: String,
    /// Span of the name token.
    pub name_span: Span,
    /// Term bound to this name.
    pub term: NameTerm,
}

/// Term inside a binding or comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum NameTerm {
    /// `?ident` — variable name (without the `?`).
    Var {
        /// Variable name.
        name: String,
        /// Source location.
        span: Span,
    },
    /// `_` — anonymous variable, fresh per occurrence.
    Anonymous {
        /// Source location.
        span: Span,
    },
    /// Literal value (UUID, string, number, bool, null).
    Literal {
        /// Tagged-union value, ready for the wire AST.
        value: JsonValue,
        /// Source location.
        span: Span,
    },
}

impl NameTerm {
    /// Span of this term in the source.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Var { span, .. } | Self::Anonymous { span } | Self::Literal { span, .. } => {
                *span
            }
        }
    }
}

/// Recursion modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameRecursion {
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `?`
    Optional,
    /// `{n,m}` — inclusive bounds.
    Bounded {
        /// Lower bound.
        min: u32,
        /// Upper bound.
        max: u32,
    },
}

/// Filter expression tree.
#[derive(Debug, Clone, PartialEq)]
pub enum NameExpr {
    /// `left and right`
    And {
        /// Left operand.
        left: Box<NameExpr>,
        /// Right operand.
        right: Box<NameExpr>,
    },
    /// `left or right`
    Or {
        /// Left operand.
        left: Box<NameExpr>,
        /// Right operand.
        right: Box<NameExpr>,
    },
    /// `not inner`
    Not {
        /// Negated expression.
        inner: Box<NameExpr>,
        /// Source location of the `not` keyword.
        span: Span,
    },
    /// `left op right`
    Cmp {
        /// Left operand (variable or literal).
        left: NameTerm,
        /// Comparison operator.
        op: NameCmpOp,
        /// Right operand (variable or literal).
        right: NameTerm,
        /// Source location of the operator.
        span: Span,
    },
}

/// Comparison operators (parser-side; convert to `ndb_engine::CmpOp` at
/// resolve time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameCmpOp {
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

/// A `create` clause in a write query.
///
/// `create species(common_name: "Foo") as ?new` creates one new entity
/// with type `species`, properties bound via the role-binding-style
/// inside the parens, and optionally binds the new UUID to `?new`.
///
/// `create predation(predator: ?wolf, prey: ?elk, season_from: 1)` creates
/// a new hyperedge whose role-fillers reference variables bound by a
/// preceding `match` (or literal UUIDs). The resolver decides whether
/// the type is entity-kind or hyperedge-kind via the dictionary.
#[derive(Debug, Clone, PartialEq)]
pub struct NameCreate {
    /// Type name from source text (resolver maps → `type_id`).
    pub type_name: String,
    /// Source span of the type name token.
    pub type_span: Span,
    /// Inside-parens bindings. For entities, every binding is a property.
    /// For hyperedges, names matching a role → role binding, names matching
    /// a property → property binding. Resolver disambiguates.
    pub bindings: Vec<NameBinding>,
    /// Optional `as ?v` capture of the new record's UUID.
    pub self_var: Option<String>,
    /// Span of the whole clause (`create type(...) [as ?v]`).
    pub span: Span,
}

/// A `set ?v.property = term` assignment within a `set` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct NameSet {
    /// Variable name (without the `?`) — must be bound by a preceding `match`.
    pub variable: String,
    /// Property name (the part after `.`).
    pub property: String,
    /// Term to assign.
    pub term: NameTerm,
    /// Span of the entire assignment.
    pub span: Span,
}

/// A `merge type(prop: val, ...) [as ?v]` clause — upsert.
///
/// Looks up an entity (or hyperedge) where every binding's value
/// matches the record's existing property value. If found, the
/// variable binds to the first match. If not found, creates a new
/// record with the bindings as properties and binds to the new record.
#[derive(Debug, Clone, PartialEq)]
pub struct NameMerge {
    /// Type name (resolver maps → type_id, decides entity vs hyperedge).
    pub type_name: String,
    /// Source span of the type name token.
    pub type_span: Span,
    /// Bindings — used for BOTH match-criteria and create-properties.
    pub bindings: Vec<NameBinding>,
    /// Optional `as ?v` capture.
    pub self_var: Option<String>,
    /// Whole-clause span.
    pub span: Span,
}

/// A `delete ?v` clause.
///
/// The variable MUST be bound by a preceding `match`. The executor
/// writes a tombstone for whichever record the UUID points at.
#[derive(Debug, Clone, PartialEq)]
pub struct NameDelete {
    /// Variable name (without the `?`).
    pub name: String,
    /// Source location.
    pub span: Span,
}

/// One sort key in the `order by` list.
#[derive(Debug, Clone, PartialEq)]
pub struct NameOrderKey {
    /// Variable name (without the `?`).
    pub name: String,
    /// Optional property name (`?v.season_from`). `None` sorts by the
    /// bound value directly (typically a UUID — useful for stable
    /// pagination but not very meaningful as a display order).
    pub property: Option<String>,
    /// `true` for descending, `false` for ascending (the default).
    pub descending: bool,
    /// Source location.
    pub span: Span,
}

/// One entry in the `return` list.
///
/// - `?v` — `property` is `None`, `aggregate` is `None`. Projects the variable's bound value.
/// - `?v.name` — `property = Some("name")`. UUID → record → property value.
/// - `count()` — `aggregate = Some(Count)`, `name = ""`, no `property`. Counts rows per group.
/// - `sum(?v.prop)` — `aggregate = Some(Sum)`, `name = "v"`, `property = Some("prop")`.
///
/// When ANY entry in the return list has an aggregate, the executor
/// implicitly groups by every non-aggregate entry — Cypher semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct NameReturn {
    /// Variable name (without the `?`). May be empty for `count()`.
    pub name: String,
    /// Optional property name following a `.` (e.g. `?p.season_from`).
    pub property: Option<String>,
    /// Optional aggregate function wrapping this projection.
    pub aggregate: Option<AggregateFn>,
    /// Source location spanning the whole entry.
    pub span: Span,
}

/// Aggregate functions supported in the `return` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateFn {
    /// `count()` (no argument) — counts rows per group.
    Count,
    /// `sum(?v.prop)` — numeric sum across the group.
    Sum,
    /// `avg(?v.prop)` — arithmetic mean across the group.
    Avg,
    /// `min(?v.prop)` — minimum across the group (string-aware).
    Min,
    /// `max(?v.prop)` — maximum across the group (string-aware).
    Max,
}

impl AggregateFn {
    /// Parse from an identifier; returns `None` if not a recognised name.
    #[must_use]
    pub fn from_ident(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "count" => Some(Self::Count),
            "sum"   => Some(Self::Sum),
            "avg"   => Some(Self::Avg),
            "min"   => Some(Self::Min),
            "max"   => Some(Self::Max),
            _ => None,
        }
    }

    /// Lower-case function name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum   => "sum",
            Self::Avg   => "avg",
            Self::Min   => "min",
            Self::Max   => "max",
        }
    }
}
