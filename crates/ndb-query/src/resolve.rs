//! Resolver — `NameQuery` (parser output) → `QueryRequest` (wire AST).
//!
//! The resolver maps NAMES to dictionary IDs by walking a
//! [`Dictionaries`] snapshot the caller provides. For v1 the caller
//! (the server's `/query` handler) builds it from
//! `Engine::snapshot_iter`; v2 will cache it on the engine.
//!
//! Resolution rules (lock'd by §5.7 of the working spec):
//!
//! - Pattern type name → `type_id`. Unknown → `UnknownType`.
//! - Per binding:
//!   - Name in role dictionary ONLY → role binding (RHS may be variable
//!     or literal).
//!   - Name in property dictionary ONLY → property filter (RHS
//!     variable = bind, literal = equality filter).
//!   - Name in BOTH dictionaries → `AmbiguousName` (Option A, spec
//!     §5.7).
//!   - Name in NEITHER → `UnknownRoleOrProperty`.
//! - Entity vs hyperedge classification: hyperedge if the pattern has
//!   any role binding, any recursion modifier, OR the type has been
//!   observed in `HyperEdge` records. Otherwise entity. v2 may add
//!   schema-driven classification.
//!
//! Variables used in `return` / `where` must be bound by some pattern;
//! the resolver collects the set of bound names and validates each
//! occurrence. Unbound → `UnboundVariable`.

use std::collections::{HashMap, HashSet};

use ndb_engine::record::Record;
use ndb_engine::{
    AsOf, CmpOp, Expr, Pattern, PropertyFilter, QueryRequest, Recursion, RoleBinding, Term,
};

use crate::ast::{
    NameAsOf, NameBinding, NameCmpOp, NameExpr, NamePattern, NameQuery, NameRecursion, NameTerm,
};
use crate::error::Span;

/// What we have observed about a type from records in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKindObserved {
    /// Only entity records of this type seen.
    Entity,
    /// Only hyperedge records of this type seen.
    Hyperedge,
    /// Both kinds seen — ambiguous at resolve time.
    Both,
}

impl TypeKindObserved {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Entity, Self::Entity) => Self::Entity,
            (Self::Hyperedge, Self::Hyperedge) => Self::Hyperedge,
            _ => Self::Both,
        }
    }
}

/// Name ↔ id maps for one snapshot of the database, plus observed
/// per-type kinds. The caller builds one of these from
/// `Engine::snapshot_iter` (or any iterator yielding the same records).
#[derive(Debug, Clone, Default)]
pub struct Dictionaries {
    /// `type_name` → `type_id`
    pub types: HashMap<String, u32>,
    /// `role_name` → `role_id`
    pub roles: HashMap<String, u32>,
    /// `property_name` → `property_id`
    pub properties: HashMap<String, u32>,
    /// `type_id` → observed kind (entity / hyperedge / both)
    pub type_kinds: HashMap<u32, TypeKindObserved>,
}

impl Dictionaries {
    /// Build a snapshot from an iterator of records.
    #[must_use]
    pub fn from_records<'a>(records: impl IntoIterator<Item = &'a Record>) -> Self {
        let mut out = Self::default();
        for r in records {
            match r {
                Record::TypeName(t) => {
                    out.types.insert(t.name.clone(), t.id.get());
                }
                Record::RoleName(r) => {
                    out.roles.insert(r.name.clone(), r.id.get());
                }
                Record::PropertyKey(p) => {
                    out.properties.insert(p.name.clone(), p.id.get());
                }
                Record::Entity(e) => {
                    out.observe(e.type_id.get(), TypeKindObserved::Entity);
                }
                Record::HyperEdge(h) => {
                    out.observe(h.type_id.get(), TypeKindObserved::Hyperedge);
                }
                Record::Tombstone(_)
                | Record::TxTimestamp(_)
                | Record::RetentionPolicy(_) => {}
            }
        }
        out
    }

    fn observe(&mut self, type_id: u32, kind: TypeKindObserved) {
        self.type_kinds
            .entry(type_id)
            .and_modify(|k| *k = k.merge(kind))
            .or_insert(kind);
    }

    /// Total entries across all three dictionaries.
    #[must_use]
    pub fn total(&self) -> usize {
        self.types.len() + self.roles.len() + self.properties.len()
    }
}

/// Resolver error. Each variant carries the source span that pointed at
/// the problem. Codes match §6.3 of the working spec.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    /// Type name not in the type dictionary.
    #[error("unknown_type: `{name}`")]
    UnknownType {
        /// The name that didn't resolve.
        name: String,
        /// Source location of the offending token.
        span: Span,
    },
    /// Binding name not registered as either a role or a property.
    #[error("unknown_role_or_property: `{name}`")]
    UnknownRoleOrProperty {
        /// The name that didn't resolve.
        name: String,
        /// Source location of the offending token.
        span: Span,
    },
    /// Binding name registered as BOTH a role and a property.
    #[error("ambiguous_name: `{name}` is registered as both a role and a property")]
    AmbiguousName {
        /// The conflicting name.
        name: String,
        /// Source location of the offending token.
        span: Span,
    },
    /// A return / where variable wasn't bound by any pattern.
    #[error("unbound_variable: `?{name}`")]
    UnboundVariable {
        /// The unbound variable name (without the `?`).
        name: String,
        /// Source location.
        span: Span,
    },
    /// Recursion modifier appeared on something the resolver can't
    /// treat as a hyperedge (currently: type observed only as entity).
    #[error("recursion_on_entity_type: type `{name}` is not a hyperedge")]
    RecursionOnEntityType {
        /// The type name.
        name: String,
        /// Source location.
        span: Span,
    },
    /// Anonymous `_` term inside `return` / `where` (only legal inside
    /// pattern bindings).
    #[error("anonymous_term_in_filter: `_` is only legal inside a pattern binding")]
    AnonymousTermInFilter {
        /// Source location.
        span: Span,
    },
}

impl ResolveError {
    /// Short error code identifying the failure class.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownType { .. } => "unknown_type",
            Self::UnknownRoleOrProperty { .. } => "unknown_role_or_property",
            Self::AmbiguousName { .. } => "ambiguous_name",
            Self::UnboundVariable { .. } => "unbound_variable",
            Self::RecursionOnEntityType { .. } => "recursion_on_entity_type",
            Self::AnonymousTermInFilter { .. } => "anonymous_term_in_filter",
        }
    }

    /// Source span associated with this error.
    #[must_use]
    pub const fn span(&self) -> Span {
        match *self {
            Self::UnknownType { span, .. }
            | Self::UnknownRoleOrProperty { span, .. }
            | Self::AmbiguousName { span, .. }
            | Self::UnboundVariable { span, .. }
            | Self::RecursionOnEntityType { span, .. }
            | Self::AnonymousTermInFilter { span } => span,
        }
    }
}

/// Resolve a name-based AST to the wire id-based AST.
pub fn resolve(query: NameQuery, dict: &Dictionaries) -> Result<QueryRequest, ResolveError> {
    let mut bound: HashSet<String> = HashSet::new();

    let patterns: Vec<Pattern> = query
        .patterns
        .into_iter()
        .map(|p| resolve_pattern(p, dict, &mut bound))
        .collect::<Result<_, _>>()?;

    let filter = match query.filter {
        Some(expr) => Some(resolve_expr(expr, &bound)?),
        None => None,
    };

    // deletes — variable must be bound by a match pattern. Resolve
    // BEFORE creates so a `match X delete ?x create Y(...) as ?x return ?x`
    // sequence treats them as ordered (delete the old, create the new
    // under the same self-bind).
    let deletes: Vec<ndb_engine::DeleteClause> = query
        .deletes
        .into_iter()
        .map(|d| {
            if !bound.contains(&d.name) {
                return Err(ResolveError::UnboundVariable { name: d.name, span: d.span });
            }
            Ok(ndb_engine::DeleteClause { variable: d.name })
        })
        .collect::<Result<_, _>>()?;

    // sets — `?v.prop = term`. Variable must be bound; property must resolve.
    let sets: Vec<ndb_engine::SetClause> = query
        .sets
        .into_iter()
        .map(|s| {
            if !bound.contains(&s.variable) {
                return Err(ResolveError::UnboundVariable {
                    name: s.variable, span: s.span,
                });
            }
            let property = *dict.properties.get(&s.property).ok_or_else(||
                ResolveError::UnknownRoleOrProperty {
                    name: s.property.clone(), span: s.span,
                }
            )?;
            let term = resolve_term(s.term, &mut bound);
            Ok(ndb_engine::SetClause {
                variable: s.variable,
                property,
                display: Some(s.property),
                term,
            })
        })
        .collect::<Result<_, _>>()?;

    // creates — must run BEFORE return resolution so that `create ... as ?new`
    // makes `?new` visible to the return list.
    let creates: Vec<ndb_engine::CreateClause> = query
        .creates
        .into_iter()
        .map(|c| resolve_create(c, dict, &mut bound))
        .collect::<Result<_, _>>()?;

    // merges — upsert. Type → id, bindings → role/property.
    let merges: Vec<ndb_engine::MergeClause> = query
        .merges
        .into_iter()
        .map(|m| resolve_merge(m, dict, &mut bound))
        .collect::<Result<_, _>>()?;

    let returns: Vec<ndb_engine::ReturnItem> = query
        .returns
        .into_iter()
        .map(|r| {
            // Aggregate path. `count()` has empty name + no property;
            // numeric aggregates require a bound variable + property.
            if let Some(agg) = r.aggregate {
                use crate::ast::AggregateFn::*;
                if matches!(agg, Count) && r.name.is_empty() {
                    return Ok(ndb_engine::ReturnItem::Aggregate {
                        func: agg.as_str().into(),
                        variable: None, property: None, display: None,
                    });
                }
                if r.name.is_empty() {
                    return Err(ResolveError::UnboundVariable {
                        name: format!("{}()", agg.as_str()),
                        span: r.span,
                    });
                }
                if !bound.contains(&r.name) {
                    return Err(ResolveError::UnboundVariable {
                        name: r.name, span: r.span,
                    });
                }
                let (property, display) = match r.property {
                    None => (None, None),
                    Some(p) => {
                        let pid = *dict.properties.get(&p).ok_or_else(||
                            ResolveError::UnknownRoleOrProperty {
                                name: p.clone(), span: r.span,
                            }
                        )?;
                        (Some(pid), Some(p))
                    }
                };
                return Ok(ndb_engine::ReturnItem::Aggregate {
                    func: agg.as_str().into(),
                    variable: Some(r.name),
                    property, display,
                });
            }
            // Regular projection.
            if !bound.contains(&r.name) {
                return Err(ResolveError::UnboundVariable {
                    name: r.name,
                    span: r.span,
                });
            }
            match r.property {
                None => Ok(ndb_engine::ReturnItem::Variable(r.name)),
                Some(prop_name) => {
                    let property = *dict.properties.get(&prop_name).ok_or_else(||
                        ResolveError::UnknownRoleOrProperty {
                            name: prop_name.clone(),
                            span: r.span,
                        }
                    )?;
                    Ok(ndb_engine::ReturnItem::Path {
                        variable: r.name,
                        property,
                        display: Some(prop_name),
                    })
                }
            }
        })
        .collect::<Result<_, _>>()?;

    // order_by — same name → id resolution as return-side property projection.
    let order_by: Vec<ndb_engine::OrderKey> = query
        .order_by
        .into_iter()
        .map(|k| {
            if !bound.contains(&k.name) {
                return Err(ResolveError::UnboundVariable { name: k.name, span: k.span });
            }
            let (property, display) = match k.property {
                None => (None, None),
                Some(prop_name) => {
                    let pid = *dict.properties.get(&prop_name).ok_or_else(||
                        ResolveError::UnknownRoleOrProperty {
                            name: prop_name.clone(),
                            span: k.span,
                        }
                    )?;
                    (Some(pid), Some(prop_name))
                }
            };
            Ok(ndb_engine::OrderKey {
                variable: k.name,
                property,
                display,
                descending: k.descending,
            })
        })
        .collect::<Result<_, _>>()?;

    let as_of = query.as_of.map(resolve_as_of);

    Ok(QueryRequest {
        as_of,
        patterns,
        filter,
        returns,
        order_by,
        limit: query.limit,
        creates,
        deletes,
        sets,
        merges,
    })
}

fn resolve_merge(
    m: crate::ast::NameMerge,
    dict: &Dictionaries,
    bound: &mut HashSet<String>,
) -> Result<ndb_engine::MergeClause, ResolveError> {
    use crate::resolve::TypeKindObserved::*;
    let type_id = *dict.types.get(&m.type_name).ok_or_else(||
        ResolveError::UnknownType { name: m.type_name.clone(), span: m.type_span }
    )?;
    let observed = dict.type_kinds.get(&type_id).copied();
    let is_hyperedge = matches!(observed, Some(Hyperedge))
        || (matches!(observed, Some(Both) | None)
            && m.bindings.iter().any(|b| dict.roles.contains_key(&b.name)));

    let mut roles: Vec<ndb_engine::CreateRoleBinding> = Vec::new();
    let mut props: Vec<ndb_engine::CreateBinding>     = Vec::new();
    for b in m.bindings {
        let is_role = dict.roles.contains_key(&b.name);
        let is_prop = dict.properties.contains_key(&b.name);
        if is_role && is_prop {
            return Err(ResolveError::AmbiguousName { name: b.name, span: b.name_span });
        }
        if !is_role && !is_prop {
            return Err(ResolveError::UnknownRoleOrProperty {
                name: b.name.clone(), span: b.name_span,
            });
        }
        let term = resolve_term(b.term, &mut *bound);
        if is_role {
            roles.push(ndb_engine::CreateRoleBinding { role_id: dict.roles[&b.name], term });
        } else {
            props.push(ndb_engine::CreateBinding { property_id: dict.properties[&b.name], term });
        }
    }
    if let Some(ref v) = m.self_var {
        bound.insert(v.clone());
    }
    Ok(ndb_engine::MergeClause {
        type_id,
        is_hyperedge,
        properties: props,
        role_bindings: roles,
        self_var: m.self_var,
    })
}

fn resolve_create(
    c: crate::ast::NameCreate,
    dict: &Dictionaries,
    bound: &mut HashSet<String>,
) -> Result<ndb_engine::CreateClause, ResolveError> {
    use crate::resolve::TypeKindObserved::*;
    let type_id = *dict.types.get(&c.type_name).ok_or_else(||
        ResolveError::UnknownType { name: c.type_name.clone(), span: c.type_span }
    )?;
    let observed = dict.type_kinds.get(&type_id).copied();
    let is_hyperedge = match observed {
        Some(Hyperedge) => true,
        Some(Entity)    => false,
        // No observations yet — new type. Heuristic: if every binding
        // matches a registered ROLE name (and not a property), treat as
        // hyperedge; else entity. The user can pick a different type if
        // we guess wrong.
        Some(Both) | None => c.bindings.iter().any(|b| dict.roles.contains_key(&b.name)),
    };

    // Each binding inside parens is either a role binding or a property
    // binding. The same name can't be both (resolver enforces).
    let mut role_bindings: Vec<ndb_engine::CreateRoleBinding> = Vec::new();
    let mut prop_bindings: Vec<ndb_engine::CreateBinding>     = Vec::new();
    for b in c.bindings {
        let is_role = dict.roles.contains_key(&b.name);
        let is_prop = dict.properties.contains_key(&b.name);
        if is_role && is_prop {
            return Err(ResolveError::AmbiguousName { name: b.name, span: b.name_span });
        }
        if !is_role && !is_prop {
            return Err(ResolveError::UnknownRoleOrProperty {
                name: b.name.clone(),
                span: b.name_span,
            });
        }
        let term = resolve_term(b.term, bound);
        if is_role {
            role_bindings.push(ndb_engine::CreateRoleBinding {
                role_id: dict.roles[&b.name],
                term,
            });
        } else {
            prop_bindings.push(ndb_engine::CreateBinding {
                property_id: dict.properties[&b.name],
                term,
            });
        }
    }
    // Roles only make sense on hyperedges.
    if !is_hyperedge && !role_bindings.is_empty() {
        return Err(ResolveError::UnknownRoleOrProperty {
            name: format!("{} (role on entity type)", c.type_name),
            span: c.type_span,
        });
    }

    // Self-bind variable becomes available downstream.
    if let Some(ref v) = c.self_var {
        bound.insert(v.clone());
    }

    Ok(if is_hyperedge {
        ndb_engine::CreateClause::Hyperedge {
            type_id,
            role_bindings,
            properties: prop_bindings,
            self_var: c.self_var,
        }
    } else {
        ndb_engine::CreateClause::Entity {
            type_id,
            properties: prop_bindings,
            self_var: c.self_var,
        }
    })
}

// Note: the actual term-resolution logic lives lower in this file as
// `fn resolve_term(t: NameTerm, bound: &mut HashSet<String>) -> Term`.

fn resolve_as_of(a: NameAsOf) -> AsOf {
    match a {
        NameAsOf::TxId(n) => AsOf::TxId { tx_id: n },
        NameAsOf::Timestamp(s) => {
            // Naive RFC3339 → microseconds parsing. Caller's
            // responsibility for v1; the resolver just passes the string
            // through as best-effort microseconds=0 — actual parsing is
            // left to the executor in v2 once commit timestamps land.
            // For now: store the parsed integer if the string was
            // already integer-like; else 0 (caller will hit
            // snapshot_unavailable at execute time).
            AsOf::TimestampUs {
                timestamp_us: s.parse::<i64>().unwrap_or(0),
            }
        }
    }
}

fn resolve_pattern(
    pat: NamePattern,
    dict: &Dictionaries,
    bound: &mut HashSet<String>,
) -> Result<Pattern, ResolveError> {
    let type_id = dict
        .types
        .get(&pat.type_name)
        .copied()
        .ok_or(ResolveError::UnknownType {
            name: pat.type_name.clone(),
            span: pat.type_name_span,
        })?;

    let mut role_bindings = Vec::new();
    let mut property_filters = Vec::new();
    for b in pat.bindings {
        match classify_binding(&b, dict)? {
            BindingClass::Role(role_id) => {
                let term = resolve_term(b.term, bound);
                role_bindings.push(RoleBinding { role_id, term });
            }
            BindingClass::Property(property_id) => {
                let term = resolve_term(b.term, bound);
                property_filters.push(PropertyFilter {
                    property_id,
                    op: CmpOp::Eq,
                    term,
                });
            }
        }
    }

    if let Some(name) = &pat.self_var {
        bound.insert(name.clone());
    }

    let has_recursion = pat.recursion.is_some();
    let observed = dict.type_kinds.get(&type_id);
    let is_hyperedge = has_recursion
        || !role_bindings.is_empty()
        || matches!(
            observed,
            Some(TypeKindObserved::Hyperedge | TypeKindObserved::Both)
        );

    if has_recursion && observed == Some(&TypeKindObserved::Entity) {
        return Err(ResolveError::RecursionOnEntityType {
            name: pat.type_name,
            span: pat.type_name_span,
        });
    }

    Ok(if is_hyperedge {
        Pattern::Hyperedge {
            type_id,
            self_var: pat.self_var,
            role_bindings,
            property_filters,
            recursion: pat.recursion.map(resolve_recursion),
        }
    } else {
        Pattern::Entity {
            type_id,
            self_var: pat.self_var,
            property_filters,
        }
    })
}

enum BindingClass {
    Role(u32),
    Property(u32),
}

fn classify_binding(b: &NameBinding, dict: &Dictionaries) -> Result<BindingClass, ResolveError> {
    let role = dict.roles.get(&b.name).copied();
    let prop = dict.properties.get(&b.name).copied();
    match (role, prop) {
        (Some(_), Some(_)) => Err(ResolveError::AmbiguousName {
            name: b.name.clone(),
            span: b.name_span,
        }),
        (Some(role_id), None) => Ok(BindingClass::Role(role_id)),
        (None, Some(prop_id)) => Ok(BindingClass::Property(prop_id)),
        (None, None) => Err(ResolveError::UnknownRoleOrProperty {
            name: b.name.clone(),
            span: b.name_span,
        }),
    }
}

fn resolve_term(t: NameTerm, bound: &mut HashSet<String>) -> Term {
    match t {
        NameTerm::Var { name, .. } => {
            bound.insert(name.clone());
            Term::Var { name }
        }
        NameTerm::Anonymous { .. } => {
            // Fresh anonymous variable — generate a stable-but-unique
            // name. Each `_` in the source gets its own.
            let name = format!("__anon_{}", anon_counter());
            Term::Var { name }
        }
        NameTerm::Literal { value, .. } => Term::Literal { value },
    }
}

// Process-wide counter for anonymous-variable names. Resolver runs in a
// single thread per query; we use a thread-local so concurrent queries
// across threads don't collide.
thread_local! {
    static ANON_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
fn anon_counter() -> u64 {
    ANON_COUNTER.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v
    })
}

const fn resolve_recursion(r: NameRecursion) -> Recursion {
    match r {
        NameRecursion::Star => Recursion::Star { max_depth: 64 },
        NameRecursion::Plus => Recursion::Plus { max_depth: 64 },
        NameRecursion::Optional => Recursion::Optional,
        NameRecursion::Bounded { min, max } => Recursion::Bounded { min, max },
    }
}

fn resolve_expr(e: NameExpr, bound: &HashSet<String>) -> Result<Expr, ResolveError> {
    match e {
        NameExpr::And { left, right } => Ok(Expr::And {
            left: Box::new(resolve_expr(*left, bound)?),
            right: Box::new(resolve_expr(*right, bound)?),
        }),
        NameExpr::Or { left, right } => Ok(Expr::Or {
            left: Box::new(resolve_expr(*left, bound)?),
            right: Box::new(resolve_expr(*right, bound)?),
        }),
        NameExpr::Not { inner, .. } => Ok(Expr::Not {
            inner: Box::new(resolve_expr(*inner, bound)?),
        }),
        NameExpr::Cmp {
            left, op, right, ..
        } => {
            let left = resolve_filter_term(left, bound)?;
            let right = resolve_filter_term(right, bound)?;
            Ok(Expr::Cmp {
                left,
                op: resolve_cmp_op(op),
                right,
            })
        }
    }
}

const fn resolve_cmp_op(op: NameCmpOp) -> CmpOp {
    match op {
        NameCmpOp::Eq => CmpOp::Eq,
        NameCmpOp::Ne => CmpOp::Ne,
        NameCmpOp::Lt => CmpOp::Lt,
        NameCmpOp::Le => CmpOp::Le,
        NameCmpOp::Gt => CmpOp::Gt,
        NameCmpOp::Ge => CmpOp::Ge,
    }
}

fn resolve_filter_term(t: NameTerm, bound: &HashSet<String>) -> Result<Term, ResolveError> {
    match t {
        NameTerm::Var { name, span } => {
            if !bound.contains(&name) {
                return Err(ResolveError::UnboundVariable { name, span });
            }
            Ok(Term::Var { name })
        }
        NameTerm::Anonymous { span } => Err(ResolveError::AnonymousTermInFilter { span }),
        NameTerm::Literal { value, .. } => Ok(Term::Literal { value }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_query;
    use ndb_engine::JsonValue;
    use ndb_engine::record::{
        EntityRecord, HyperEdgeRecord, PropertyKeyRecord, RoleNameRecord, TypeNameRecord,
    };
    use ndb_engine::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};

    fn type_rec(id: u32, name: &str) -> Record {
        Record::TypeName(TypeNameRecord {
            id: TypeId::new(id),
            name: name.into(),
        })
    }
    fn role_rec(id: u32, name: &str) -> Record {
        Record::RoleName(RoleNameRecord {
            id: RoleId::new(id),
            name: name.into(),
        })
    }
    fn prop_rec(id: u32, name: &str) -> Record {
        Record::PropertyKey(PropertyKeyRecord {
            id: PropertyId::new(id),
            name: name.into(),
        })
    }
    fn entity_rec(type_id: u32) -> Record {
        Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![],
        })
    }
    fn hyperedge_rec(type_id: u32) -> Record {
        Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        })
    }

    fn basic_dict() -> Dictionaries {
        Dictionaries::from_records(&[
            type_rec(100, "customer"),
            type_rec(200, "sales_order"),
            type_rec(201, "contains"),
            role_rec(10, "customer"),
            role_rec(11, "parent"),
            role_rec(12, "child"),
            prop_rec(30, "name"),
            prop_rec(31, "region"),
            prop_rec(32, "amount"),
            entity_rec(100),
            hyperedge_rec(200),
            hyperedge_rec(201),
        ])
    }

    fn resolve_str(s: &str) -> Result<QueryRequest, ResolveError> {
        let q = parse_query(s).unwrap();
        resolve(q, &basic_dict())
    }

    #[test]
    fn unknown_type_errors() {
        let err = resolve_str("match nosuch(name: ?n) return ?n").unwrap_err();
        assert!(matches!(err, ResolveError::UnknownType { ref name, .. } if name == "nosuch"));
    }

    #[test]
    fn unknown_role_or_property_errors() {
        let err = resolve_str("match customer(zzz: ?v) return ?v").unwrap_err();
        assert!(matches!(
            err,
            ResolveError::UnknownRoleOrProperty { ref name, .. } if name == "zzz"
        ));
    }

    #[test]
    fn ambiguous_name_errors() {
        // role "customer" and property "customer" both exist? Let's build a
        // dict that has that collision.
        let dict = Dictionaries::from_records(&[
            type_rec(100, "sales_order"),
            role_rec(10, "x"),
            prop_rec(30, "x"),
            hyperedge_rec(100),
        ]);
        let q = parse_query("match sales_order(x: ?v) return ?v").unwrap();
        let err = resolve(q, &dict).unwrap_err();
        assert!(matches!(
            err,
            ResolveError::AmbiguousName { ref name, .. } if name == "x"
        ));
    }

    #[test]
    fn entity_pattern_property_filter_literal() {
        let q = resolve_str(r#"match customer(region: "Vietnam") return ?nothing"#).unwrap_err();
        // ?nothing is unbound — but the pattern itself resolves; verify by
        // re-parsing with a bound return.
        assert!(matches!(q, ResolveError::UnboundVariable { ref name, .. } if name == "nothing"));

        let q = resolve_str(r#"match customer(region: "Vietnam") as ?c return ?c"#).unwrap();
        match &q.patterns[0] {
            Pattern::Entity {
                type_id,
                property_filters,
                ..
            } => {
                assert_eq!(*type_id, 100);
                assert_eq!(property_filters.len(), 1);
                assert_eq!(property_filters[0].property_id, 31);
                assert!(matches!(
                    property_filters[0].term,
                    Term::Literal {
                        value: JsonValue::String { .. }
                    }
                ));
            }
            Pattern::Hyperedge { .. } => panic!("expected entity pattern"),
        }
    }

    #[test]
    fn entity_pattern_property_bind_variable() {
        // customer(name: ?n) — ?n binds to the property value.
        let q = resolve_str("match customer(name: ?n) as ?c return ?c, ?n").unwrap();
        match &q.patterns[0] {
            Pattern::Entity {
                property_filters, ..
            } => {
                assert_eq!(property_filters.len(), 1);
                assert!(matches!(
                    &property_filters[0].term,
                    Term::Var { name } if name == "n"
                ));
            }
            Pattern::Hyperedge { .. } => panic!("expected entity pattern"),
        }
    }

    #[test]
    fn hyperedge_pattern_with_role_binding() {
        let q = resolve_str(
            "match sales_order(customer: ?c, amount: ?a) return ?c, ?a",
        )
        .unwrap();
        match &q.patterns[0] {
            Pattern::Hyperedge {
                role_bindings,
                property_filters,
                ..
            } => {
                assert_eq!(role_bindings.len(), 1);
                assert_eq!(role_bindings[0].role_id, 10);
                assert_eq!(property_filters.len(), 1);
                assert_eq!(property_filters[0].property_id, 32);
            }
            Pattern::Entity { .. } => panic!("expected hyperedge pattern"),
        }
    }

    #[test]
    fn recursive_hyperedge_pattern() {
        let q = resolve_str(
            "match contains*(parent: uuid:01923c00-0000-7000-8000-000000000001, child: ?leaf) as ?h return ?leaf, ?h",
        ).unwrap();
        match &q.patterns[0] {
            Pattern::Hyperedge {
                recursion: Some(Recursion::Star { max_depth }),
                role_bindings,
                ..
            } => {
                assert_eq!(*max_depth, 64);
                assert_eq!(role_bindings.len(), 2);
            }
            Pattern::Entity { .. } | Pattern::Hyperedge { .. } => {
                panic!("expected recursive hyperedge")
            }
        }
    }

    #[test]
    fn unbound_variable_in_return() {
        let err = resolve_str("match customer(name: ?n) as ?c return ?other").unwrap_err();
        assert!(matches!(
            err,
            ResolveError::UnboundVariable { ref name, .. } if name == "other"
        ));
    }

    #[test]
    fn unbound_variable_in_where() {
        let err = resolve_str("match customer(name: ?n) as ?c where ?ghost = ?n return ?c")
            .unwrap_err();
        assert!(matches!(
            err,
            ResolveError::UnboundVariable { ref name, .. } if name == "ghost"
        ));
    }

    #[test]
    fn anonymous_in_filter_errors() {
        // Resolver-side rule: `_` in `where` makes no sense (each occurrence
        // is a fresh variable, so `_ = ?p` is trivially satisfied — disallow
        // to surface user confusion).
        let q = parse_query("match customer(name: ?n) as ?c where _ = ?n return ?c").unwrap();
        let err = resolve(q, &basic_dict()).unwrap_err();
        assert!(matches!(err, ResolveError::AnonymousTermInFilter { .. }));
    }

    #[test]
    fn anonymous_in_pattern_is_fine() {
        let q = resolve_str("match sales_order(customer: _, amount: ?a) return ?a").unwrap();
        // The _ should have become a fresh variable that's not in `returns`.
        match &q.patterns[0] {
            Pattern::Hyperedge { role_bindings, .. } => {
                assert!(matches!(&role_bindings[0].term, Term::Var { name } if name.starts_with("__anon_")));
            }
            Pattern::Entity { .. } => panic!("expected hyperedge"),
        }
    }

    #[test]
    fn dictionaries_observes_kinds() {
        let dict = Dictionaries::from_records(&[
            type_rec(100, "customer"),
            type_rec(200, "sales_order"),
            entity_rec(100),
            entity_rec(100),
            hyperedge_rec(200),
        ]);
        assert_eq!(dict.type_kinds.get(&100), Some(&TypeKindObserved::Entity));
        assert_eq!(
            dict.type_kinds.get(&200),
            Some(&TypeKindObserved::Hyperedge)
        );
    }

    #[test]
    fn dictionaries_total_count() {
        let dict = Dictionaries::from_records(&[
            type_rec(1, "a"),
            role_rec(2, "b"),
            prop_rec(3, "c"),
        ]);
        assert_eq!(dict.total(), 3);
    }

    #[test]
    fn as_of_tx_id_resolves() {
        let q = resolve_str("as of 42 match customer(name: ?n) as ?c return ?c").unwrap();
        assert_eq!(q.as_of, Some(AsOf::TxId { tx_id: 42 }));
    }

    #[test]
    fn full_resolved_query_matches_wire_shape() {
        let q = resolve_str(
            r"match
                 sales_order(customer: ?c, amount: ?a)
                 customer(name: ?n) as ?c
               where ?a > 1000
               return ?c, ?n, ?a
               limit 100",
        )
        .unwrap();
        assert_eq!(q.patterns.len(), 2);
        assert!(matches!(q.patterns[0], Pattern::Hyperedge { .. }));
        assert!(matches!(q.patterns[1], Pattern::Entity { .. }));
        assert!(q.filter.is_some());
        let names: Vec<String> = q.returns.iter().map(|r| match r {
            ndb_engine::ReturnItem::Variable(n) => n.clone(),
            ndb_engine::ReturnItem::Path { variable, .. } => variable.clone(),
            ndb_engine::ReturnItem::Aggregate { .. } => "<agg>".into(),
        }).collect();
        assert_eq!(names, vec!["c", "n", "a"]);
        assert_eq!(q.limit, Some(100));
    }
}
