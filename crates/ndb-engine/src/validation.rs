//! Validation engine — runtime constraint enforcement (§6.3, §17.1).
#![allow(clippy::doc_markdown)]
//!
//! v1 constraints, locked in this commit:
//!
//! - **Required property.** "An entity of type T must have a property
//!   with id P." Missing on commit ⇒ `MissingRequiredProperty`.
//! - **Value tag.** "Property P on type T must use value tag X." Mismatch
//!   on commit ⇒ `WrongValueTag`. Useful for catching schema drift —
//!   e.g. a property that should always be `i64` accidentally being set
//!   as a string.
//!
//! Constraints can be registered in two ways:
//!
//! 1. **At runtime** via [`ValidationEngine`] methods directly. Used by
//!    tests and small embedded apps. Constraints registered this way are
//!    in-memory only — they do NOT survive engine restarts.
//! 2. **Metadata-driven** via reserved-id constraint entities written to
//!    the database. The engine scans these at `open()` and calls the
//!    matching `ValidationEngine` method automatically. Constraints
//!    written this way are durable — they're part of the database.
//!
//! Metadata-constraint encoding (locked):
//!
//! A constraint is one entity with `type_id = TYPE_VALIDATION_CONSTRAINT`
//! (= [`TYPE_VALIDATION_CONSTRAINT`]) and the following properties:
//!
//! | Property id | Type | Meaning |
//! |---|---|---|
//! | [`PROP_CONSTRAINT_KIND`] | `Value::I64` | 1 = required property, 2 = value tag |
//! | [`PROP_TARGET_TYPE`] | `Value::I64` | `type_id` of the constraint's target |
//! | [`PROP_TARGET_PROPERTY`] | `Value::I64` | `property_id` of the constraint's target |
//! | [`PROP_EXPECTED_TAG`] | `Value::I64` | tag byte (only for kind=2 value_tag) |
//!
//! The 0xFFFF_FFFx reserved-ID space is chosen so client-side ids stay
//! well clear. Engine refuses commits that touch these reserved IDs from
//! anywhere other than the metadata path.
//!
//! Out-of-scope for v1:
//!
//! - Uniqueness (covered by the lookup-key index instead).
//! - Cross-entity referential integrity (a hyperedge role pointing at an
//!   entity that exists). This needs index lookups and bumps cost; defer.
//! - Format validators (regex, range, enum). Application-level concern
//!   until the engine has a query layer that can express them.

use std::collections::{HashMap, HashSet};

use thiserror::Error;

use crate::id::{PropertyId, TypeId};
use crate::record::Record;
use crate::value::Value;

// ---------------------------------------------------------------------------
// Reserved IDs for metadata-driven constraints
// ---------------------------------------------------------------------------

/// Reserved type id for constraint entities. Engine scans entities of
/// this type at `open()` and registers them with the validation engine.
pub const TYPE_VALIDATION_CONSTRAINT: TypeId = TypeId::new(0xFFFF_FFF0);

/// Property id: constraint kind discriminator.
/// Value: `I64(1)` = required property; `I64(2)` = value tag.
pub const PROP_CONSTRAINT_KIND: PropertyId = PropertyId::new(0xFFFF_FFF1);

/// Property id: `type_id` the constraint targets (stored as `Value::I64`).
pub const PROP_TARGET_TYPE: PropertyId = PropertyId::new(0xFFFF_FFF2);

/// Property id: `property_id` the constraint targets (stored as `Value::I64`).
pub const PROP_TARGET_PROPERTY: PropertyId = PropertyId::new(0xFFFF_FFF3);

/// Property id: expected `Value` tag byte (kind=2 only; stored as `Value::I64`).
pub const PROP_EXPECTED_TAG: PropertyId = PropertyId::new(0xFFFF_FFF4);

/// Constraint kind discriminator values.
pub const CONSTRAINT_KIND_REQUIRED: i64 = 1;
/// Constraint kind discriminator: value tag.
pub const CONSTRAINT_KIND_VALUE_TAG: i64 = 2;

/// Errors raised by the validation engine on commit.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Entity of `type_id` is missing a property declared required.
    #[error("entity type={type_id:?} missing required property={property_id:?}")]
    MissingRequiredProperty {
        /// Entity's declared type.
        type_id: TypeId,
        /// Property id that must be present.
        property_id: PropertyId,
    },

    /// Property carries the wrong `Value` tag (e.g. should be `String`
    /// but was set to `I64`).
    #[error(
        "entity type={type_id:?} property={property_id:?}: expected tag 0x{expected:02x}, got 0x{got:02x}"
    )]
    WrongValueTag {
        /// Entity's declared type.
        type_id: TypeId,
        /// Property whose value violates the expected tag.
        property_id: PropertyId,
        /// Tag byte declared via `expect_value_tag`.
        expected: u8,
        /// Tag byte actually present in the record.
        got: u8,
    },
}

/// In-memory constraint set + checker.
#[derive(Debug, Default)]
pub struct ValidationEngine {
    /// `type_id → set of required property ids`.
    required: HashMap<TypeId, HashSet<PropertyId>>,
    /// `(type_id, property_id) → expected value tag byte`.
    expected_tag: HashMap<(TypeId, PropertyId), u8>,
}

impl ValidationEngine {
    /// New, empty engine — no constraints.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare that entities of `type_id` MUST carry the given property.
    /// Idempotent.
    pub fn require_property(&mut self, type_id: TypeId, property_id: PropertyId) {
        self.required
            .entry(type_id)
            .or_default()
            .insert(property_id);
    }

    /// Declare that property `property_id` on entities of `type_id` MUST
    /// use the given `Value` tag byte. Idempotent (later calls overwrite
    /// the expected tag for the same pair).
    pub fn expect_value_tag(&mut self, type_id: TypeId, property_id: PropertyId, tag: u8) {
        self.expected_tag.insert((type_id, property_id), tag);
    }

    /// Number of distinct types with at least one required property.
    #[must_use]
    pub fn type_count(&self) -> usize {
        self.required.len()
    }

    /// True iff at least one constraint is registered.
    #[must_use]
    pub fn has_constraints(&self) -> bool {
        !self.required.is_empty() || !self.expected_tag.is_empty()
    }

    /// Clear all constraints. Used by tests.
    pub fn clear(&mut self) {
        self.required.clear();
        self.expected_tag.clear();
    }

    /// Walk `records` and register any constraint entities found
    /// (`type_id == TYPE_VALIDATION_CONSTRAINT`). Records of other types
    /// are ignored. Returns the number of constraints loaded.
    ///
    /// Called by `Engine::open` after `rebuild_indexes` so durable
    /// constraints survive restarts. Callers that mutate constraint
    /// entities at runtime should call `Engine::reload_constraints` to
    /// pick the changes up (or directly mutate via `require_property` /
    /// `expect_value_tag` for ephemeral changes).
    pub fn load_from_metadata<'a>(&mut self, records: impl IntoIterator<Item = &'a Record>) -> usize {
        let mut loaded = 0;
        for r in records {
            let Record::Entity(e) = r else { continue };
            if e.type_id != TYPE_VALIDATION_CONSTRAINT {
                continue;
            }
            let mut kind: Option<i64> = None;
            let mut target_type: Option<i64> = None;
            let mut target_prop: Option<i64> = None;
            let mut expected_tag: Option<i64> = None;
            for (pid, v) in &e.properties {
                match (*pid, v) {
                    (p, Value::I64(n)) if p == PROP_CONSTRAINT_KIND => kind = Some(*n),
                    (p, Value::I64(n)) if p == PROP_TARGET_TYPE => target_type = Some(*n),
                    (p, Value::I64(n)) if p == PROP_TARGET_PROPERTY => target_prop = Some(*n),
                    (p, Value::I64(n)) if p == PROP_EXPECTED_TAG => expected_tag = Some(*n),
                    _ => {}
                }
            }
            let (Some(k), Some(t), Some(p)) = (kind, target_type, target_prop) else {
                continue;
            };
            let target_type_id = u32::try_from(t).map(TypeId::new).ok();
            let target_prop_id = u32::try_from(p).map(PropertyId::new).ok();
            let (Some(tt), Some(tp)) = (target_type_id, target_prop_id) else {
                continue;
            };
            match k {
                CONSTRAINT_KIND_REQUIRED => {
                    self.require_property(tt, tp);
                    loaded += 1;
                }
                CONSTRAINT_KIND_VALUE_TAG => {
                    if let Some(tag) = expected_tag.and_then(|n| u8::try_from(n).ok()) {
                        self.expect_value_tag(tt, tp, tag);
                        loaded += 1;
                    }
                }
                _ => {}
            }
        }
        loaded
    }

    /// Check a record. Tombstones and dictionary records pass through;
    /// only entity records currently have constraints.
    pub fn check(&self, record: &Record) -> Result<(), ValidationError> {
        match record {
            Record::Entity(e) => self.check_entity(e),
            // Hyperedge constraints would mirror entity ones (required
            // role / property / value-tag) but require a richer schema
            // model. Skipped in v1.
            Record::HyperEdge(_)
            | Record::Tombstone(_)
            | Record::TypeName(_)
            | Record::RoleName(_)
            | Record::PropertyKey(_)
            | Record::TxTimestamp(_)
            | Record::RetentionPolicy(_) => Ok(()),
        }
    }

    fn check_entity(&self, e: &crate::record::EntityRecord) -> Result<(), ValidationError> {
        if let Some(required) = self.required.get(&e.type_id) {
            let present: HashSet<PropertyId> = e.properties.iter().map(|(p, _)| *p).collect();
            for req in required {
                if !present.contains(req) {
                    return Err(ValidationError::MissingRequiredProperty {
                        type_id: e.type_id,
                        property_id: *req,
                    });
                }
            }
        }
        for (prop, value) in &e.properties {
            if let Some(expected) = self.expected_tag.get(&(e.type_id, *prop)) {
                let got = value.tag();
                if got != *expected {
                    return Err(ValidationError::WrongValueTag {
                        type_id: e.type_id,
                        property_id: *prop,
                        expected: *expected,
                        got,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Stop-gap shim for callers that want to validate an isolated entity
/// without instantiating an engine.
pub fn validate_with(engine: &ValidationEngine, record: &Record) -> Result<(), ValidationError> {
    engine.check(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntityId, HyperedgeId, RoleId, TxId};
    use crate::record::{EntityRecord, HyperEdgeRecord};
    use crate::value::{TAG_STRING, Value};

    fn entity(type_id: u32, props: Vec<(u32, Value)>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            properties: props
                .into_iter()
                .map(|(p, v)| (PropertyId::new(p), v))
                .collect(),
        })
    }

    #[test]
    fn passes_with_no_constraints() {
        let v = ValidationEngine::new();
        assert!(v.check(&entity(1, vec![])).is_ok());
        assert!(!v.has_constraints());
    }

    #[test]
    fn required_property_present() {
        let mut v = ValidationEngine::new();
        v.require_property(TypeId::new(1), PropertyId::new(7));
        let ok = entity(1, vec![(7, Value::String("alice".into()))]);
        assert!(v.check(&ok).is_ok());
    }

    #[test]
    fn missing_required_property_rejected() {
        let mut v = ValidationEngine::new();
        v.require_property(TypeId::new(1), PropertyId::new(7));
        let bad = entity(1, vec![(8, Value::String("nope".into()))]);
        match v.check(&bad) {
            Err(ValidationError::MissingRequiredProperty {
                type_id,
                property_id,
            }) => {
                assert_eq!(type_id, TypeId::new(1));
                assert_eq!(property_id, PropertyId::new(7));
            }
            other => panic!("expected MissingRequiredProperty, got {other:?}"),
        }
    }

    #[test]
    fn constraint_scoped_to_type() {
        let mut v = ValidationEngine::new();
        v.require_property(TypeId::new(1), PropertyId::new(7));
        // Type 2 has no required-property constraints — passes despite
        // missing prop 7.
        assert!(v.check(&entity(2, vec![])).is_ok());
    }

    #[test]
    fn wrong_value_tag_rejected() {
        let mut v = ValidationEngine::new();
        v.expect_value_tag(TypeId::new(1), PropertyId::new(7), TAG_STRING);
        let bad = entity(1, vec![(7, Value::I64(42))]);
        match v.check(&bad) {
            Err(ValidationError::WrongValueTag {
                expected,
                got,
                property_id,
                type_id,
            }) => {
                assert_eq!(expected, TAG_STRING);
                assert_ne!(got, TAG_STRING);
                assert_eq!(property_id, PropertyId::new(7));
                assert_eq!(type_id, TypeId::new(1));
            }
            other => panic!("expected WrongValueTag, got {other:?}"),
        }
    }

    #[test]
    fn correct_value_tag_accepted() {
        let mut v = ValidationEngine::new();
        v.expect_value_tag(TypeId::new(1), PropertyId::new(7), TAG_STRING);
        let ok = entity(1, vec![(7, Value::String("ok".into()))]);
        assert!(v.check(&ok).is_ok());
    }

    #[test]
    fn hyperedge_passes_through() {
        let mut v = ValidationEngine::new();
        v.require_property(TypeId::new(1), PropertyId::new(7));
        // Hyperedge constraints are not yet enforced — should not be
        // affected by entity-property requirements.
        let h = Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            properties: vec![],
        });
        assert!(v.check(&h).is_ok());
    }

    #[test]
    fn multiple_required_properties_all_checked() {
        let mut v = ValidationEngine::new();
        v.require_property(TypeId::new(1), PropertyId::new(7));
        v.require_property(TypeId::new(1), PropertyId::new(8));
        v.require_property(TypeId::new(1), PropertyId::new(9));
        let bad = entity(
            1,
            vec![
                (7, Value::String("a".into())),
                (8, Value::String("b".into())),
                // 9 is missing.
            ],
        );
        match v.check(&bad) {
            Err(ValidationError::MissingRequiredProperty { property_id, .. }) => {
                assert_eq!(property_id, PropertyId::new(9));
            }
            other => panic!("expected MissingRequiredProperty, got {other:?}"),
        }
    }

    #[test]
    fn clear_drops_constraints() {
        let mut v = ValidationEngine::new();
        v.require_property(TypeId::new(1), PropertyId::new(7));
        v.expect_value_tag(TypeId::new(1), PropertyId::new(7), TAG_STRING);
        assert!(v.has_constraints());
        v.clear();
        assert!(!v.has_constraints());
        assert!(v.check(&entity(1, vec![])).is_ok());
    }
}
