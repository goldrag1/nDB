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
    /// `return` variable list.
    pub returns: Vec<NameReturn>,
    /// `order by` key list, in source order. Empty when absent. Sort
    /// is stable so multiple keys behave like SQL `ORDER BY a, b, c`.
    pub order_by: Vec<NameOrderKey>,
    /// `limit N`, or `None`.
    pub limit: Option<usize>,
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
/// - `?v` — `property` is `None`. Projects the variable's bound value
///   (UUID for self-bound entities / hyperedges, scalar for role
///   bindings).
/// - `?v.name` — `property` is `Some("name")`. Follows the bound UUID
///   to its record and projects the named property's value.
#[derive(Debug, Clone, PartialEq)]
pub struct NameReturn {
    /// Variable name (without the `?`).
    pub name: String,
    /// Optional property name following a `.` (e.g. `?p.season_from`).
    pub property: Option<String>,
    /// Source location spanning the whole `?v[.prop]` projection.
    pub span: Span,
}
