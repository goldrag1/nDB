//! JSON wire format for nDB records and values (§4 architecture overview).
#![allow(clippy::doc_markdown)]
//!
//! The on-disk format is custom binary (§11.2); the wire format is JSON.
//! This module converts between the engine's strongly-typed [`Value`] /
//! [`Record`] and a JSON shape suitable for HTTP request/response bodies
//! and JSONL streaming.
//!
//! v1 JSON shape decisions, locked in this commit:
//!
//! - **Tagged-union for `Value`.** Each value is `{"tag": "<name>", ... }`
//!   with `tag` being the snake-case variant name. The payload field is
//!   `value` for simple variants, named fields for `decimal` and the
//!   payload-bearing variants.
//! - **Bytes and Extension payloads are base64 (standard, padded).**
//!   JSON strings can't carry arbitrary bytes; base64 is the lowest-
//!   friction choice for cross-language clients.
//! - **`i128` decimal mantissa is a string.** JavaScript can't represent
//!   `i64` past 2^53 safely, let alone `i128`. Always a decimal string;
//!   parsing handles the sign.
//! - **`u64` tx ids stay as JSON numbers.** Clients that need
//!   JS-bigint-safe representation can hex-encode in v2; for v1 the wire
//!   talks to Rust clients first.
//! - **UUIDs are canonical lower-case hyphenated form** (the `uuid`
//!   crate's `Display`).
//! - **Tombstone records use `"kind": "tombstone"`** without the
//!   distinction of entity vs hyperedge target — the wire layer doesn't
//!   know (and shouldn't need to know) which kind of target the tombstone
//!   originally killed.
//!
//! Round-trip is byte-for-byte preserving — every wire-JSON produced
//! from a value parses back to an equal value.

use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TX_ACTIVE, TxId, TypeId};
use crate::record::{
    EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, RoleNameRecord, TombstoneRecord,
    TypeNameRecord,
};
use crate::value::Value;

/// Errors raised while parsing wire JSON. Encoding never fails.
#[derive(Debug, Error)]
pub enum WireError {
    /// `serde_json` couldn't parse the input as JSON at all.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A field had the wrong shape (e.g. `kind` missing, `roles` not an array).
    #[error("malformed wire value: {0}")]
    Shape(&'static str),
    /// `i128` mantissa didn't parse.
    #[error("invalid decimal mantissa: {0}")]
    Decimal(#[from] std::num::ParseIntError),
    /// Base64-decoded payload was malformed.
    #[error("invalid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// UUID payload was malformed.
    #[error("invalid UUID: {0}")]
    Uuid(#[from] uuid::Error),
}

// ---------------------------------------------------------------------------
// JsonValue — wire shape mirroring `Value`
// ---------------------------------------------------------------------------

/// JSON-friendly mirror of [`Value`]. Use [`JsonValue::from`] / [`Value::try_from`]
/// to convert.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum JsonValue {
    /// `{"tag": "null"}`
    Null,
    /// `{"tag": "bool", "value": true}`
    Bool {
        /// Boolean payload.
        value: bool,
    },
    /// `{"tag": "i64", "value": 42}`
    I64 {
        /// Signed 64-bit payload.
        value: i64,
    },
    /// `{"tag": "f64", "value": 3.14}`. Special values (`NaN`, ±∞) are
    /// serialised as JSON `null`; clients should treat that as
    /// "non-finite" and handle accordingly.
    F64 {
        /// Double payload.
        value: f64,
    },
    /// `{"tag": "string", "value": "..."}`
    String {
        /// UTF-8 string payload.
        value: String,
    },
    /// `{"tag": "bytes", "value": "<base64>"}`
    Bytes {
        /// Base64-encoded payload.
        value: String,
    },
    /// `{"tag": "timestamp", "value": 1700000000000000}` (microseconds
    /// since Unix epoch).
    Timestamp {
        /// Microseconds payload.
        value: i64,
    },
    /// `{"tag": "uuid", "value": "<canonical>"}` (entity ref).
    Uuid {
        /// Canonical hyphenated lower-case UUID.
        value: String,
    },
    /// `{"tag": "decimal", "scale": 2, "mantissa": "1234567"}` (mantissa
    /// as string so JS clients don't lose precision).
    Decimal {
        /// Decimal places (0..=255).
        scale: u8,
        /// `i128` mantissa as a string.
        mantissa: String,
    },
    /// `{"tag": "vector", "value": [1.0, 2.0, ...]}`
    Vector {
        /// `f32` vector payload.
        value: Vec<f32>,
    },
    /// `{"tag": "extension", "value": "<base64>"}` — forward-compat
    /// passthrough for payloads the build doesn't understand.
    Extension {
        /// Base64-encoded payload.
        value: String,
    },
}

impl From<&Value> for JsonValue {
    fn from(v: &Value) -> Self {
        match v {
            Value::Null => JsonValue::Null,
            Value::Bool(b) => JsonValue::Bool { value: *b },
            Value::I64(n) => JsonValue::I64 { value: *n },
            Value::F64(f) => JsonValue::F64 { value: *f },
            Value::String(s) => JsonValue::String { value: s.clone() },
            Value::Bytes(b) => JsonValue::Bytes {
                value: BASE64.encode(b),
            },
            Value::Timestamp(t) => JsonValue::Timestamp { value: *t },
            Value::EntityRef(id) => JsonValue::Uuid {
                value: id.into_uuid().to_string(),
            },
            Value::Decimal { scale, mantissa } => JsonValue::Decimal {
                scale: *scale,
                mantissa: mantissa.to_string(),
            },
            Value::Vector(v) => JsonValue::Vector { value: v.clone() },
            Value::Extension(b) => JsonValue::Extension {
                value: BASE64.encode(b),
            },
        }
    }
}

impl TryFrom<JsonValue> for Value {
    type Error = WireError;

    fn try_from(j: JsonValue) -> Result<Self, Self::Error> {
        Ok(match j {
            JsonValue::Null => Value::Null,
            JsonValue::Bool { value } => Value::Bool(value),
            JsonValue::I64 { value } => Value::I64(value),
            JsonValue::F64 { value } => Value::F64(value),
            JsonValue::String { value } => Value::String(value),
            JsonValue::Bytes { value } => Value::Bytes(BASE64.decode(value)?),
            JsonValue::Timestamp { value } => Value::Timestamp(value),
            JsonValue::Uuid { value } => {
                Value::EntityRef(EntityId::from_uuid(Uuid::parse_str(&value)?))
            }
            JsonValue::Decimal { scale, mantissa } => Value::Decimal {
                scale,
                mantissa: i128::from_str(&mantissa)?,
            },
            JsonValue::Vector { value } => Value::Vector(value),
            JsonValue::Extension { value } => Value::Extension(BASE64.decode(value)?),
        })
    }
}

// ---------------------------------------------------------------------------
// JsonRecord — wire shape mirroring `Record`
// ---------------------------------------------------------------------------

/// Property pair as it appears on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonProperty {
    /// Dictionary id from the property-key dictionary.
    pub prop_id: u32,
    /// Tagged value.
    pub value: JsonValue,
}

/// Role binding as it appears on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRole {
    /// Dictionary id from the role-name dictionary.
    pub role_id: u32,
    /// Canonical UUID of the entity playing this role.
    pub entity_id: String,
}

/// JSON-friendly mirror of [`Record`]. `tx_id_supersede` is either the
/// literal numeric tx id OR the string `"active"` for the `TX_ACTIVE`
/// sentinel — this keeps the wire compact and obvious for human readers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JsonRecord {
    /// Entity assertion.
    Entity {
        /// Canonical UUID.
        entity_id: String,
        /// `TypeName` dictionary id (`0` for untyped).
        type_id: u32,
        /// Transaction that created this assertion.
        tx_id_assert: u64,
        /// Transaction that superseded it, or `"active"`.
        tx_id_supersede: TxIdOrActive,
        /// Property values.
        properties: Vec<JsonProperty>,
    },
    /// Hyperedge assertion.
    HyperEdge {
        /// Canonical UUID.
        hyperedge_id: String,
        /// `TypeName` dictionary id (must be non-zero).
        type_id: u32,
        /// Transaction that created this assertion.
        tx_id_assert: u64,
        /// Transaction that superseded it, or `"active"`.
        tx_id_supersede: TxIdOrActive,
        /// Role bindings (arity is implicit in length).
        roles: Vec<JsonRole>,
        /// Property values attached to the hyperedge itself.
        properties: Vec<JsonProperty>,
    },
    /// Deletion marker.
    Tombstone {
        /// Canonical UUID of the entity or hyperedge being deleted.
        target_id: String,
        /// Transaction that performed the delete.
        tx_id_supersede: u64,
    },
    /// Type-name dictionary entry.
    TypeName {
        /// Dictionary slot (must be non-zero).
        id: u32,
        /// Type name string.
        name: String,
    },
    /// Role-name dictionary entry.
    RoleName {
        /// Dictionary slot (must be non-zero).
        id: u32,
        /// Role name string.
        name: String,
    },
    /// Property-key dictionary entry.
    PropertyKey {
        /// Dictionary slot (must be non-zero).
        id: u32,
        /// Property key string.
        name: String,
    },
    /// Wall-clock commit timestamp for a transaction (v2.0+ internal metadata).
    TxTimestamp {
        /// Transaction id.
        tx_id: u64,
        /// Microseconds since Unix epoch at commit time.
        timestamp_us: i64,
    },
    /// Per-type retention policy (v2.0+ internal metadata).
    RetentionPolicy {
        /// Target type id.
        type_id: u32,
        /// Policy discriminator: 0 = LatestOnly, 1 = Versioned, 2 = Audited.
        policy_kind: u8,
        /// `keep_last_n` for Versioned; ignored otherwise.
        keep_last_n: u32,
    },
}

/// Wire form of `tx_id_supersede`: either a numeric `tx_id` OR the
/// literal string `"active"` for the `TX_ACTIVE` sentinel.
#[derive(Debug, Clone, Copy)]
pub enum TxIdOrActive {
    /// Specific transaction id (record was superseded).
    Tx(u64),
    /// `TX_ACTIVE` sentinel (record is still live).
    Active,
}

impl Serialize for TxIdOrActive {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tx(n) => s.serialize_u64(*n),
            Self::Active => s.serialize_str("active"),
        }
    }
}

impl<'de> Deserialize<'de> for TxIdOrActive {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Active(String),
            Tx(u64),
        }
        match Helper::deserialize(d)? {
            Helper::Tx(n) => Ok(Self::Tx(n)),
            Helper::Active(s) if s == "active" => Ok(Self::Active),
            Helper::Active(other) => Err(serde::de::Error::custom(format!(
                "expected 'active' or u64, got {other:?}"
            ))),
        }
    }
}

impl From<TxIdOrActive> for TxId {
    fn from(t: TxIdOrActive) -> TxId {
        match t {
            TxIdOrActive::Active => TxId::new(TX_ACTIVE),
            TxIdOrActive::Tx(n) => TxId::new(n),
        }
    }
}

impl From<TxId> for TxIdOrActive {
    fn from(t: TxId) -> TxIdOrActive {
        if t.is_active_sentinel() {
            TxIdOrActive::Active
        } else {
            TxIdOrActive::Tx(t.get())
        }
    }
}

impl From<&Record> for JsonRecord {
    fn from(r: &Record) -> Self {
        match r {
            Record::Entity(e) => JsonRecord::Entity {
                entity_id: e.entity_id.into_uuid().to_string(),
                type_id: e.type_id.get(),
                tx_id_assert: e.tx_id_assert.get(),
                tx_id_supersede: e.tx_id_supersede.into(),
                properties: e
                    .properties
                    .iter()
                    .map(|(p, v)| JsonProperty {
                        prop_id: p.get(),
                        value: v.into(),
                    })
                    .collect(),
            },
            Record::HyperEdge(h) => JsonRecord::HyperEdge {
                hyperedge_id: h.hyperedge_id.into_uuid().to_string(),
                type_id: h.type_id.get(),
                tx_id_assert: h.tx_id_assert.get(),
                tx_id_supersede: h.tx_id_supersede.into(),
                roles: h
                    .roles
                    .iter()
                    .map(|(r, e)| JsonRole {
                        role_id: r.get(),
                        entity_id: e.into_uuid().to_string(),
                    })
                    .collect(),
                properties: h
                    .properties
                    .iter()
                    .map(|(p, v)| JsonProperty {
                        prop_id: p.get(),
                        value: v.into(),
                    })
                    .collect(),
            },
            Record::Tombstone(t) => JsonRecord::Tombstone {
                target_id: t.target_id.to_string(),
                tx_id_supersede: t.tx_id_supersede.get(),
            },
            Record::TypeName(d) => JsonRecord::TypeName {
                id: d.id.get(),
                name: d.name.clone(),
            },
            Record::RoleName(d) => JsonRecord::RoleName {
                id: d.id.get(),
                name: d.name.clone(),
            },
            Record::PropertyKey(d) => JsonRecord::PropertyKey {
                id: d.id.get(),
                name: d.name.clone(),
            },
            Record::TxTimestamp(t) => JsonRecord::TxTimestamp {
                tx_id: t.tx_id.get(),
                timestamp_us: t.timestamp_us,
            },
            Record::RetentionPolicy(r) => JsonRecord::RetentionPolicy {
                type_id: r.type_id.get(),
                policy_kind: r.policy_kind,
                keep_last_n: r.keep_last_n,
            },
        }
    }
}

impl TryFrom<JsonRecord> for Record {
    type Error = WireError;

    fn try_from(j: JsonRecord) -> Result<Self, Self::Error> {
        Ok(match j {
            JsonRecord::Entity {
                entity_id,
                type_id,
                tx_id_assert,
                tx_id_supersede,
                properties,
            } => Record::Entity(EntityRecord {
                entity_id: EntityId::from_uuid(Uuid::parse_str(&entity_id)?),
                type_id: TypeId::new(type_id),
                tx_id_assert: TxId::new(tx_id_assert),
                tx_id_supersede: tx_id_supersede.into(),
                properties: properties
                    .into_iter()
                    .map(|p| Ok::<_, WireError>((PropertyId::new(p.prop_id), p.value.try_into()?)))
                    .collect::<Result<_, _>>()?,
            }),
            JsonRecord::HyperEdge {
                hyperedge_id,
                type_id,
                tx_id_assert,
                tx_id_supersede,
                roles,
                properties,
            } => Record::HyperEdge(HyperEdgeRecord {
                hyperedge_id: HyperedgeId::from_uuid(Uuid::parse_str(&hyperedge_id)?),
                type_id: TypeId::new(type_id),
                tx_id_assert: TxId::new(tx_id_assert),
                tx_id_supersede: tx_id_supersede.into(),
                roles: roles
                    .into_iter()
                    .map(|r| {
                        Ok::<_, WireError>((
                            RoleId::new(r.role_id),
                            EntityId::from_uuid(Uuid::parse_str(&r.entity_id)?),
                        ))
                    })
                    .collect::<Result<_, _>>()?,
                properties: properties
                    .into_iter()
                    .map(|p| Ok::<_, WireError>((PropertyId::new(p.prop_id), p.value.try_into()?)))
                    .collect::<Result<_, _>>()?,
            }),
            JsonRecord::Tombstone {
                target_id,
                tx_id_supersede,
            } => Record::Tombstone(TombstoneRecord {
                target_id: Uuid::parse_str(&target_id)?,
                tx_id_supersede: TxId::new(tx_id_supersede),
            }),
            JsonRecord::TypeName { id, name } => Record::TypeName(TypeNameRecord {
                id: TypeId::new(id),
                name,
            }),
            JsonRecord::RoleName { id, name } => Record::RoleName(RoleNameRecord {
                id: RoleId::new(id),
                name,
            }),
            JsonRecord::PropertyKey { id, name } => Record::PropertyKey(PropertyKeyRecord {
                id: PropertyId::new(id),
                name,
            }),
            JsonRecord::TxTimestamp {
                tx_id,
                timestamp_us,
            } => Record::TxTimestamp(crate::record::TxTimestampRecord {
                tx_id: TxId::new(tx_id),
                timestamp_us,
            }),
            JsonRecord::RetentionPolicy {
                type_id,
                policy_kind,
                keep_last_n,
            } => Record::RetentionPolicy(crate::record::RetentionPolicyRecord {
                type_id: TypeId::new(type_id),
                policy_kind,
                keep_last_n,
            }),
        })
    }
}

// ---------------------------------------------------------------------------
// API request/response shapes
// ---------------------------------------------------------------------------

/// `POST /commit` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRequest {
    /// Records to commit. `tx_id_assert` / `tx_id_supersede` in the
    /// records are STAMPED by the server (overwritten with the new
    /// transaction's id), so callers can leave them as 0 / "active".
    pub records: Vec<JsonRecord>,
}

/// `POST /commit` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResponse {
    /// Transaction id assigned to this commit.
    pub tx_id: u64,
}

/// `GET /read/:uuid?snapshot=N` response body. `snapshot` is optional;
/// the server defaults to `manifest().last_tx_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReadResponse {
    /// No version visible at the snapshot.
    Missing,
    /// Tombstone is the latest visible event.
    Deleted {
        /// Tx that committed the delete.
        deleted_at: u64,
    },
    /// Live record visible at the snapshot.
    Live {
        /// The visible record in JSON wire form.
        record: JsonRecord,
    },
}

/// Generic error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Short error code identifying the failure class.
    pub error: String,
    /// Human-readable detail.
    pub detail: String,
}

/// `POST /lookup` request body — find an entity by an external lookup-key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LookupRequest {
    /// Lookup-key property id (must have been `register_lookup_key`'d
    /// on the server).
    pub property_id: u32,
    /// Tagged-union value to match.
    pub value: JsonValue,
}

/// `POST /lookup` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LookupResponse {
    /// Matched entity uuid, or `None` if no entity carries that key.
    pub entity_id: Option<String>,
}

/// Distance metric tag on the wire. Matches the engine `Distance` enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorMetric {
    /// `Distance::L2Squared`.
    L2,
    /// `Distance::Cosine`.
    Cosine,
}

/// `POST /vector_search` request body — k-NN over a vector-indexed property.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchRequest {
    /// Vector property id (must have been `register_vector_property`'d).
    pub property_id: u32,
    /// Query vector. Must match the property's locked dimension.
    pub query: Vec<f32>,
    /// Top-k cap. Server enforces an upper bound to prevent unbounded scans.
    pub k: usize,
    /// Distance metric.
    pub metric: VectorMetric,
}

/// One hit in a vector-search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHit {
    /// Entity uuid as a string.
    pub entity_id: String,
    /// Distance per the requested metric. Smaller = closer.
    pub distance: f32,
}

/// `POST /vector_search` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResponse {
    /// Top-k hits, sorted ascending by distance.
    pub hits: Vec<VectorHit>,
}

/// `POST /property_lookup` request body — exact match over the property B-tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyLookupRequest {
    /// Entity type id.
    pub type_id: u32,
    /// Property id (must be `register_property_btree`'d on the server).
    pub property_id: u32,
    /// Tagged-union value to match exactly.
    pub value: JsonValue,
}

/// `POST /property_lookup` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyLookupResponse {
    /// Matched entity uuids.
    pub entity_ids: Vec<String>,
}

/// `POST /property_range` request body — range query over the property B-tree.
/// Inclusive bounds on both ends; `None` = unbounded on that side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyRangeRequest {
    /// Entity type id.
    pub type_id: u32,
    /// Property id (must be `register_property_btree`'d on the server).
    pub property_id: u32,
    /// Lower bound (inclusive). `None` for unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<JsonValue>,
    /// Upper bound (inclusive). `None` for unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<JsonValue>,
}

/// `POST /property_range` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyRangeResponse {
    /// Matched entity uuids.
    pub entity_ids: Vec<String>,
}

/// One hop in a [`TraverseRequest`]. Specifies which hyperedge type to
/// walk across at this step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseHop {
    /// Only walk hyperedges of this type at this hop. `None` = any type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperedge_type_id: Option<u32>,
}

/// `POST /traverse` request body. Server-side breadth-first traversal
/// starting at one entity, walking through the configured sequence of
/// hyperedge types, returning every entity reachable at the end.
///
/// At each hop, for every entity in the current frontier, the server
/// looks up every hyperedge incident on that entity (via the adjacency
/// index), filters by the hop's hyperedge type, and adds the other
/// role-bound entities to the next frontier. Cycles are deduplicated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseRequest {
    /// UUID of the entity to start the walk from.
    pub start: String,
    /// Ordered sequence of hops. Length determines walk depth.
    pub hops: Vec<TraverseHop>,
}

/// `POST /traverse` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseResponse {
    /// Entity uuids reachable after walking every hop.
    pub entity_ids: Vec<String>,
}

/// `POST /subscribe` request body — long-poll for records committed
/// after `since_tx_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeRequest {
    /// Return records with `tx_id_assert > since_tx_id`.
    pub since_tx_id: u64,
    /// Maximum time to wait for new commits, in milliseconds. Server
    /// caps this server-side to prevent indefinite holds. Default 30000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)] // test helpers
mod tests {
    use super::*;
    use crate::id::{PropertyId, RoleId};
    use crate::record::{EntityRecord, HyperEdgeRecord, TombstoneRecord, TypeNameRecord};
    use crate::value::Value;

    fn round_trip_value(v: Value) {
        let j: JsonValue = (&v).into();
        let s = serde_json::to_string(&j).unwrap();
        let j2: JsonValue = serde_json::from_str(&s).unwrap();
        let restored: Value = j2.try_into().unwrap();
        assert_eq!(restored, v, "json roundtrip preserves value");
    }

    #[test]
    fn value_round_trips_every_tag() {
        round_trip_value(Value::Null);
        round_trip_value(Value::Bool(true));
        round_trip_value(Value::Bool(false));
        round_trip_value(Value::I64(-42));
        round_trip_value(Value::I64(i64::MIN));
        round_trip_value(Value::I64(i64::MAX));
        round_trip_value(Value::F64(0.0));
        round_trip_value(Value::F64(std::f64::consts::PI));
        round_trip_value(Value::String("hello 🦀".into()));
        round_trip_value(Value::String(String::new()));
        round_trip_value(Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        round_trip_value(Value::Bytes(vec![]));
        round_trip_value(Value::Timestamp(1_700_000_000_000_000));
        round_trip_value(Value::EntityRef(EntityId::now_v7()));
        round_trip_value(Value::Decimal {
            scale: 2,
            mantissa: 12_345,
        });
        round_trip_value(Value::Decimal {
            scale: 0,
            mantissa: i128::MIN,
        });
        round_trip_value(Value::Decimal {
            scale: 255,
            mantissa: i128::MAX,
        });
        round_trip_value(Value::Vector(vec![1.0, -2.0, 3.5]));
        round_trip_value(Value::Vector(vec![]));
        round_trip_value(Value::Extension(b"\x01\x02".to_vec()));
    }

    #[test]
    fn entity_record_round_trip() {
        let r = Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(42),
            tx_id_assert: TxId::new(100),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(1), Value::String("alice".into())),
                (PropertyId::new(2), Value::I64(30)),
            ],
        });
        let j: JsonRecord = (&r).into();
        let s = serde_json::to_string_pretty(&j).unwrap();
        let j2: JsonRecord = serde_json::from_str(&s).unwrap();
        let restored: Record = j2.try_into().unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn hyperedge_record_round_trip() {
        let r = Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(7),
            tx_id_assert: TxId::new(50),
            tx_id_supersede: TxId::new(99), // exercise non-active supersede on hyperedge
            roles: vec![
                (RoleId::new(1), EntityId::now_v7()),
                (RoleId::new(2), EntityId::now_v7()),
            ],
            properties: vec![],
        });
        let j: JsonRecord = (&r).into();
        let s = serde_json::to_string(&j).unwrap();
        let j2: JsonRecord = serde_json::from_str(&s).unwrap();
        let restored: Record = j2.try_into().unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn tombstone_round_trip() {
        let r = Record::Tombstone(TombstoneRecord {
            target_id: uuid::Uuid::now_v7(),
            tx_id_supersede: TxId::new(123),
        });
        let j: JsonRecord = (&r).into();
        let s = serde_json::to_string(&j).unwrap();
        let j2: JsonRecord = serde_json::from_str(&s).unwrap();
        let restored: Record = j2.try_into().unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn dict_record_round_trip() {
        let r = Record::TypeName(TypeNameRecord {
            id: TypeId::new(5),
            name: "Customer".into(),
        });
        let j: JsonRecord = (&r).into();
        let s = serde_json::to_string(&j).unwrap();
        let j2: JsonRecord = serde_json::from_str(&s).unwrap();
        let restored: Record = j2.try_into().unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn active_sentinel_serializes_as_string() {
        let r = Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![],
        });
        let j: JsonRecord = (&r).into();
        let s = serde_json::to_string(&j).unwrap();
        assert!(
            s.contains("\"tx_id_supersede\":\"active\""),
            "expected 'active' string sentinel, got: {s}"
        );
    }

    #[test]
    fn malformed_uuid_rejected() {
        let bad = serde_json::json!({
            "kind": "entity",
            "entity_id": "not-a-uuid",
            "type_id": 1,
            "tx_id_assert": 0,
            "tx_id_supersede": "active",
            "properties": []
        });
        let j: JsonRecord = serde_json::from_value(bad).unwrap();
        let r: Result<Record, _> = j.try_into();
        assert!(matches!(r, Err(WireError::Uuid(_))));
    }

    #[test]
    fn decimal_mantissa_string_round_trip() {
        let big = Value::Decimal {
            scale: 18,
            mantissa: i128::MAX,
        };
        let j: JsonValue = (&big).into();
        let s = serde_json::to_string(&j).unwrap();
        // Confirm the mantissa is a STRING in the JSON, not a number.
        assert!(
            s.contains("\"mantissa\":\""),
            "mantissa must be a quoted string: {s}"
        );
        let restored: Value = serde_json::from_str::<JsonValue>(&s)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(restored, big);
    }
}
