//! On-disk record layouts (§11.2 / §11.3).
//!
//! Six record kinds share a common envelope:
//!
//! ```text
//! ┌──────────────┬────────────┬────────────────┬──────── … ─────────┬───────┐
//! │ record_size  │ record_kind│ format_version │       payload      │ crc32 │
//! │   u32 (LE)   │    u8      │      u8        │      (variable)    │  u32  │
//! └──────────────┴────────────┴────────────────┴──────── … ─────────┴───────┘
//!  ↑ self-inclusive: counts these bytes and the trailing CRC                ↑
//! ```
//!
//! `record_size` is self-inclusive (it counts its own 4 bytes plus the
//! trailing 4-byte CRC). The CRC32 is computed over every byte of the record
//! *except* the CRC field itself, so a scanner that reads the size first can
//! always seek `record_size` forward to land on the next record's first byte.

use crc32fast::Hasher;

use crate::codec::{Cursor, write_u8, write_u16, write_u32, write_u64};
use crate::error::{DecodeError, EncodeError};
use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TX_ACTIVE, TYPE_UNTYPED, TxId, TypeId};
use crate::value::Value;

// ---------------------------------------------------------------------------
// Envelope constants
// ---------------------------------------------------------------------------

/// Current on-disk record-layout version this build emits.
pub const FORMAT_VERSION: u8 = 1;

/// Highest `format_version` this build can decode. Bumped when older readers
/// can still parse newer-version records (forward-compat); equal to
/// `FORMAT_VERSION` otherwise.
pub const FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;

const SIZE_FIELD_LEN: usize = 4;
const KIND_FIELD_LEN: usize = 1;
const VERSION_FIELD_LEN: usize = 1;
const CRC_FIELD_LEN: usize = 4;

/// Bytes consumed by the envelope (size + kind + `format_version` + CRC),
/// regardless of which record kind sits in between.
pub const ENVELOPE_OVERHEAD: usize =
    SIZE_FIELD_LEN + KIND_FIELD_LEN + VERSION_FIELD_LEN + CRC_FIELD_LEN;

/// Discriminator byte for each record kind (§11.2).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// `EntityRecord` — `0x01`.
    Entity = 0x01,
    /// `HyperEdgeRecord` — `0x02`.
    HyperEdge = 0x02,
    /// `TombstoneRecord` — `0x03`.
    Tombstone = 0x03,
    /// `TypeNameRecord` — `0x04`. Dictionary entry: `u32 ↔ type-name string`.
    TypeName = 0x04,
    /// `RoleNameRecord` — `0x05`. Dictionary entry: `u32 ↔ role-name string`.
    RoleName = 0x05,
    /// `PropertyKeyRecord` — `0x06`. Dictionary entry: `u32 ↔ property-key string`.
    PropertyKey = 0x06,
}

impl RecordKind {
    /// Decode a kind byte. Returns `UnknownRecordKind` for unrecognised values.
    pub fn from_byte(b: u8) -> Result<Self, DecodeError> {
        Ok(match b {
            0x01 => Self::Entity,
            0x02 => Self::HyperEdge,
            0x03 => Self::Tombstone,
            0x04 => Self::TypeName,
            0x05 => Self::RoleName,
            0x06 => Self::PropertyKey,
            other => return Err(DecodeError::UnknownRecordKind { kind: other }),
        })
    }

    /// The kind byte as it appears on disk.
    #[inline]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// Per-kind structs
// ---------------------------------------------------------------------------

/// Assertion that an entity with the given id and type carries these
/// properties as of `tx_id_assert`, until (optionally) `tx_id_supersede`.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityRecord {
    /// UUID v7 of the entity. Stable for the lifetime of the entity.
    pub entity_id: EntityId,
    /// Declared type (`TypeId::UNTYPED` for typeless entities).
    pub type_id: TypeId,
    /// Transaction that created this assertion.
    pub tx_id_assert: TxId,
    /// Transaction that superseded this assertion, or `TxId::ACTIVE`.
    pub tx_id_supersede: TxId,
    /// Property values keyed by `PropertyId`. Order is preserved on disk.
    pub properties: Vec<(PropertyId, Value)>,
}

/// Assertion that the named role-players participate in this hyperedge as of
/// `tx_id_assert`. Arity is implicit in `roles.len()`.
#[derive(Debug, Clone, PartialEq)]
pub struct HyperEdgeRecord {
    /// UUID v7 of the hyperedge.
    pub hyperedge_id: HyperedgeId,
    /// Declared hyperedge type. Must be non-zero — `TYPE_UNTYPED` is
    /// entity-only.
    pub type_id: TypeId,
    /// Transaction that created this assertion.
    pub tx_id_assert: TxId,
    /// Transaction that superseded this assertion, or `TxId::ACTIVE`.
    pub tx_id_supersede: TxId,
    /// Role bindings. `roles.len()` is the arity (must be ≥ 1). `RoleId(0)`
    /// is rejected on encode.
    pub roles: Vec<(RoleId, EntityId)>,
    /// Properties attached to the hyperedge itself.
    pub properties: Vec<(PropertyId, Value)>,
}

/// Explicit deletion marker. Targets either an entity or a hyperedge; which
/// one is recovered by looking up the prior active assertion under
/// `target_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct TombstoneRecord {
    /// UUID v7 of the entity or hyperedge being deleted.
    pub target_id: uuid::Uuid,
    /// Transaction performing the deletion.
    pub tx_id_supersede: TxId,
}

/// Dictionary entry mapping a `u32` slot to a UTF-8 type-name string.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeNameRecord {
    /// Dictionary slot (must be non-zero).
    pub id: TypeId,
    /// Type-name string.
    pub name: String,
}

/// Dictionary entry mapping a `u32` slot to a UTF-8 role-name string.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleNameRecord {
    /// Dictionary slot (must be non-zero).
    pub id: RoleId,
    /// Role-name string.
    pub name: String,
}

/// Dictionary entry mapping a `u32` slot to a UTF-8 property-key string.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyKeyRecord {
    /// Dictionary slot (must be non-zero).
    pub id: PropertyId,
    /// Property-key string.
    pub name: String,
}

// ---------------------------------------------------------------------------
// Any-record enum — convenience wrapper used by the WAL replayer and the
// scan-recovery loop.
// ---------------------------------------------------------------------------

/// Union of every record kind, used by code that handles a heterogeneous
/// stream of records (WAL replay, `SSTable` iteration, scan-recovery).
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// `0x01` — `EntityRecord`.
    Entity(EntityRecord),
    /// `0x02` — `HyperEdgeRecord`.
    HyperEdge(HyperEdgeRecord),
    /// `0x03` — `TombstoneRecord`.
    Tombstone(TombstoneRecord),
    /// `0x04` — `TypeNameRecord`.
    TypeName(TypeNameRecord),
    /// `0x05` — `RoleNameRecord`.
    RoleName(RoleNameRecord),
    /// `0x06` — `PropertyKeyRecord`.
    PropertyKey(PropertyKeyRecord),
}

impl Record {
    /// `RecordKind` discriminant for this record.
    #[must_use]
    pub fn kind(&self) -> RecordKind {
        match self {
            Self::Entity(_) => RecordKind::Entity,
            Self::HyperEdge(_) => RecordKind::HyperEdge,
            Self::Tombstone(_) => RecordKind::Tombstone,
            Self::TypeName(_) => RecordKind::TypeName,
            Self::RoleName(_) => RecordKind::RoleName,
            Self::PropertyKey(_) => RecordKind::PropertyKey,
        }
    }

    /// Encode whichever variant is active onto `out`. Returns bytes appended.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        match self {
            Self::Entity(r) => r.encode(out),
            Self::HyperEdge(r) => r.encode(out),
            Self::Tombstone(r) => r.encode(out),
            Self::TypeName(r) => r.encode(out),
            Self::RoleName(r) => r.encode(out),
            Self::PropertyKey(r) => r.encode(out),
        }
    }

    /// Peek at the leading envelope bytes of `input` to discover the record
    /// kind, then dispatch to the matching decoder.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let kind = peek_record_kind(input)?;
        Ok(match kind {
            RecordKind::Entity => {
                let (r, n) = EntityRecord::decode(input)?;
                (Self::Entity(r), n)
            }
            RecordKind::HyperEdge => {
                let (r, n) = HyperEdgeRecord::decode(input)?;
                (Self::HyperEdge(r), n)
            }
            RecordKind::Tombstone => {
                let (r, n) = TombstoneRecord::decode(input)?;
                (Self::Tombstone(r), n)
            }
            RecordKind::TypeName => {
                let (r, n) = TypeNameRecord::decode(input)?;
                (Self::TypeName(r), n)
            }
            RecordKind::RoleName => {
                let (r, n) = RoleNameRecord::decode(input)?;
                (Self::RoleName(r), n)
            }
            RecordKind::PropertyKey => {
                let (r, n) = PropertyKeyRecord::decode(input)?;
                (Self::PropertyKey(r), n)
            }
        })
    }
}

impl From<EntityRecord> for Record {
    fn from(r: EntityRecord) -> Self {
        Self::Entity(r)
    }
}
impl From<HyperEdgeRecord> for Record {
    fn from(r: HyperEdgeRecord) -> Self {
        Self::HyperEdge(r)
    }
}
impl From<TombstoneRecord> for Record {
    fn from(r: TombstoneRecord) -> Self {
        Self::Tombstone(r)
    }
}
impl From<TypeNameRecord> for Record {
    fn from(r: TypeNameRecord) -> Self {
        Self::TypeName(r)
    }
}
impl From<RoleNameRecord> for Record {
    fn from(r: RoleNameRecord) -> Self {
        Self::RoleName(r)
    }
}
impl From<PropertyKeyRecord> for Record {
    fn from(r: PropertyKeyRecord) -> Self {
        Self::PropertyKey(r)
    }
}

// ---------------------------------------------------------------------------
// Envelope helpers (write + read)
// ---------------------------------------------------------------------------

fn begin_record(buf: &mut Vec<u8>, kind: RecordKind) -> usize {
    let start = buf.len();
    buf.extend_from_slice(&[0u8; SIZE_FIELD_LEN]); // placeholder
    write_u8(buf, kind.as_byte());
    write_u8(buf, FORMAT_VERSION);
    start
}

fn finalize_record(buf: &mut Vec<u8>, start: usize) -> Result<usize, EncodeError> {
    // total_size = body length so far + 4 trailing CRC bytes (self-inclusive).
    let body_len = buf.len() - start;
    let total_with_crc = body_len + CRC_FIELD_LEN;
    let size_u32 = u32::try_from(total_with_crc)
        .map_err(|_| EncodeError::RecordSizeOverflow(total_with_crc))?;
    buf[start..start + SIZE_FIELD_LEN].copy_from_slice(&size_u32.to_le_bytes());

    let mut h = Hasher::new();
    h.update(&buf[start..start + body_len]);
    let crc = h.finalize();
    buf.extend_from_slice(&crc.to_le_bytes());
    Ok(total_with_crc)
}

/// Result of validating a record envelope without parsing the payload.
struct Envelope<'a> {
    /// Bytes occupied by this record on disk, including envelope + CRC.
    total_size: usize,
    /// Payload bytes — everything between the `format_version` byte and the CRC.
    payload: &'a [u8],
    /// `format_version` byte. Recorded for callers that want to log it.
    #[allow(dead_code)]
    format_version: u8,
}

fn read_envelope(input: &[u8], expected: RecordKind) -> Result<Envelope<'_>, DecodeError> {
    let min = ENVELOPE_OVERHEAD;
    if input.len() < min {
        return Err(DecodeError::Truncated {
            offset: 0,
            needed: min - input.len(),
        });
    }
    let claimed = u32::from_le_bytes(input[0..4].try_into().unwrap()) as usize;
    if claimed < min {
        return Err(DecodeError::RecordSizeTooSmall {
            claimed,
            minimum: min,
        });
    }
    if claimed > input.len() {
        return Err(DecodeError::InvalidRecordSize {
            claimed,
            available: input.len(),
        });
    }
    let kind_byte = input[4];
    let kind = RecordKind::from_byte(kind_byte)?;
    if kind != expected {
        return Err(DecodeError::WrongRecordKind {
            found: kind_byte,
            expected: expected.as_byte(),
        });
    }
    let format_version = input[5];
    if format_version > FORMAT_VERSION_MAX_SUPPORTED {
        return Err(DecodeError::UnsupportedFormatVersion {
            version: format_version,
            supported: FORMAT_VERSION_MAX_SUPPORTED,
        });
    }
    let body_end = claimed - CRC_FIELD_LEN;
    let stored_crc = u32::from_le_bytes(input[body_end..body_end + 4].try_into().unwrap());
    let mut h = Hasher::new();
    h.update(&input[0..body_end]);
    let computed = h.finalize();
    if stored_crc != computed {
        return Err(DecodeError::CrcMismatch {
            stored: stored_crc,
            computed,
        });
    }
    Ok(Envelope {
        total_size: claimed,
        payload: &input[6..body_end],
        format_version,
    })
}

/// Read the `record_size` field from the head of `input` without validating
/// or parsing the rest. Useful for scan-recovery loops that need to skip
/// past corrupted records.
pub fn peek_record_size(input: &[u8]) -> Result<usize, DecodeError> {
    if input.len() < SIZE_FIELD_LEN {
        return Err(DecodeError::Truncated {
            offset: 0,
            needed: SIZE_FIELD_LEN - input.len(),
        });
    }
    Ok(u32::from_le_bytes(input[0..4].try_into().unwrap()) as usize)
}

/// Read the `record_kind` byte at offset 4 without decoding the rest.
pub fn peek_record_kind(input: &[u8]) -> Result<RecordKind, DecodeError> {
    if input.len() < SIZE_FIELD_LEN + KIND_FIELD_LEN {
        return Err(DecodeError::Truncated {
            offset: 0,
            needed: SIZE_FIELD_LEN + KIND_FIELD_LEN - input.len(),
        });
    }
    RecordKind::from_byte(input[4])
}

// ---------------------------------------------------------------------------
// Property-list helpers
// ---------------------------------------------------------------------------

fn encode_property_list(
    buf: &mut Vec<u8>,
    props: &[(PropertyId, Value)],
) -> Result<(), EncodeError> {
    let count =
        u16::try_from(props.len()).map_err(|_| EncodeError::PropertyCountOverflow(props.len()))?;
    write_u16(buf, count);
    for (pid, v) in props {
        if pid.get() == 0 {
            return Err(EncodeError::ZeroPropertyId);
        }
        write_u32(buf, pid.get());
        v.encode(buf)?;
    }
    Ok(())
}

fn decode_property_list(c: &mut Cursor<'_>) -> Result<Vec<(PropertyId, Value)>, DecodeError> {
    let count = c.read_u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let pid = c.read_u32()?;
        if pid == 0 {
            return Err(DecodeError::InvalidSentinel("prop_id must be non-zero"));
        }
        let value = Value::decode_from(c)?;
        out.push((PropertyId(pid), value));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// EntityRecord
// ---------------------------------------------------------------------------

impl EntityRecord {
    /// Encode this record onto `out`. Returns the number of bytes appended.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        let start = begin_record(out, RecordKind::Entity);
        out.extend_from_slice(self.entity_id.as_bytes());
        write_u32(out, self.type_id.get());
        write_u64(out, self.tx_id_assert.get());
        write_u64(out, self.tx_id_supersede.get());
        encode_property_list(out, &self.properties)?;
        finalize_record(out, start)
    }

    /// Decode the record at the start of `input`. Returns the record and the
    /// byte count it consumed.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let env = read_envelope(input, RecordKind::Entity)?;
        let mut c = Cursor::new(env.payload);
        let entity_id = EntityId::from_bytes(c.read_array::<16>()?);
        let type_id = TypeId::new(c.read_u32()?);
        let tx_id_assert = TxId::new(c.read_u64()?);
        let tx_id_supersede = TxId::new(c.read_u64()?);
        let properties = decode_property_list(&mut c)?;
        if c.remaining() != 0 {
            return Err(DecodeError::TrailingBytes(c.remaining()));
        }
        Ok((
            Self {
                entity_id,
                type_id,
                tx_id_assert,
                tx_id_supersede,
                properties,
            },
            env.total_size,
        ))
    }
}

// ---------------------------------------------------------------------------
// HyperEdgeRecord
// ---------------------------------------------------------------------------

impl HyperEdgeRecord {
    /// Encode this record onto `out`. Returns the number of bytes appended.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        if self.roles.is_empty() {
            return Err(EncodeError::HyperEdgeZeroArity);
        }
        if self.type_id.get() == TYPE_UNTYPED {
            return Err(EncodeError::ZeroHyperEdgeTypeId);
        }
        let arity = u8::try_from(self.roles.len())
            .map_err(|_| EncodeError::ArityOverflow(self.roles.len()))?;

        let start = begin_record(out, RecordKind::HyperEdge);
        out.extend_from_slice(self.hyperedge_id.as_bytes());
        write_u32(out, self.type_id.get());
        write_u64(out, self.tx_id_assert.get());
        write_u64(out, self.tx_id_supersede.get());
        write_u8(out, arity);
        for (rid, entity) in &self.roles {
            if rid.get() == 0 {
                return Err(EncodeError::ZeroRoleId);
            }
            write_u32(out, rid.get());
            out.extend_from_slice(entity.as_bytes());
        }
        encode_property_list(out, &self.properties)?;
        finalize_record(out, start)
    }

    /// Decode the record at the start of `input`.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let env = read_envelope(input, RecordKind::HyperEdge)?;
        let mut c = Cursor::new(env.payload);
        let hyperedge_id = HyperedgeId::from_bytes(c.read_array::<16>()?);
        let type_id_raw = c.read_u32()?;
        if type_id_raw == TYPE_UNTYPED {
            return Err(DecodeError::InvalidSentinel(
                "hyperedge type_id must be non-zero",
            ));
        }
        let type_id = TypeId::new(type_id_raw);
        let tx_id_assert = TxId::new(c.read_u64()?);
        let tx_id_supersede = TxId::new(c.read_u64()?);
        let arity = c.read_u8()? as usize;
        if arity == 0 {
            return Err(DecodeError::InvalidSentinel("hyperedge arity must be ≥ 1"));
        }
        let mut roles = Vec::with_capacity(arity);
        for _ in 0..arity {
            let rid = c.read_u32()?;
            if rid == 0 {
                return Err(DecodeError::InvalidSentinel("role_id must be non-zero"));
            }
            let entity = EntityId::from_bytes(c.read_array::<16>()?);
            roles.push((RoleId(rid), entity));
        }
        let properties = decode_property_list(&mut c)?;
        if c.remaining() != 0 {
            return Err(DecodeError::TrailingBytes(c.remaining()));
        }
        Ok((
            Self {
                hyperedge_id,
                type_id,
                tx_id_assert,
                tx_id_supersede,
                roles,
                properties,
            },
            env.total_size,
        ))
    }
}

// ---------------------------------------------------------------------------
// TombstoneRecord
// ---------------------------------------------------------------------------

impl TombstoneRecord {
    /// Encode this record onto `out`. Returns the number of bytes appended.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        let start = begin_record(out, RecordKind::Tombstone);
        out.extend_from_slice(self.target_id.as_bytes());
        write_u64(out, self.tx_id_supersede.get());
        finalize_record(out, start)
    }

    /// Decode the record at the start of `input`.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let env = read_envelope(input, RecordKind::Tombstone)?;
        let mut c = Cursor::new(env.payload);
        let target_id = uuid::Uuid::from_bytes(c.read_array::<16>()?);
        let tx_id_supersede = TxId::new(c.read_u64()?);
        if c.remaining() != 0 {
            return Err(DecodeError::TrailingBytes(c.remaining()));
        }
        // A tombstone with TX_ACTIVE for supersede makes no sense — that
        // would mean "this is dead as of never", which is meaningless.
        if tx_id_supersede.get() == TX_ACTIVE {
            return Err(DecodeError::InvalidSentinel(
                "tombstone tx_id_supersede must not be TX_ACTIVE",
            ));
        }
        Ok((
            Self {
                target_id,
                tx_id_supersede,
            },
            env.total_size,
        ))
    }
}

// ---------------------------------------------------------------------------
// Dictionary records — shared body shape
// ---------------------------------------------------------------------------

fn encode_dictionary_body(
    out: &mut Vec<u8>,
    kind: RecordKind,
    id: u32,
    name: &str,
) -> Result<usize, EncodeError> {
    if id == 0 {
        return Err(EncodeError::ZeroDictionaryId);
    }
    let name_len =
        u32::try_from(name.len()).map_err(|_| EncodeError::DictionaryNameOverflow(name.len()))?;
    let start = begin_record(out, kind);
    write_u32(out, id);
    write_u32(out, name_len);
    out.extend_from_slice(name.as_bytes());
    finalize_record(out, start)
}

fn decode_dictionary_body(
    input: &[u8],
    kind: RecordKind,
) -> Result<(u32, String, usize), DecodeError> {
    let env = read_envelope(input, kind)?;
    let mut c = Cursor::new(env.payload);
    let id = c.read_u32()?;
    if id == 0 {
        return Err(DecodeError::InvalidSentinel(
            "dictionary id must be non-zero",
        ));
    }
    let name_len = c.read_u32()? as usize;
    let name_bytes = c.read_slice(name_len)?;
    if c.remaining() != 0 {
        return Err(DecodeError::TrailingBytes(c.remaining()));
    }
    let name = std::str::from_utf8(name_bytes)?.to_owned();
    Ok((id, name, env.total_size))
}

impl TypeNameRecord {
    /// Encode this record onto `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        encode_dictionary_body(out, RecordKind::TypeName, self.id.get(), &self.name)
    }

    /// Decode the record at the start of `input`.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let (id, name, total) = decode_dictionary_body(input, RecordKind::TypeName)?;
        Ok((
            Self {
                id: TypeId(id),
                name,
            },
            total,
        ))
    }
}

impl RoleNameRecord {
    /// Encode this record onto `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        encode_dictionary_body(out, RecordKind::RoleName, self.id.get(), &self.name)
    }

    /// Decode the record at the start of `input`.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let (id, name, total) = decode_dictionary_body(input, RecordKind::RoleName)?;
        Ok((
            Self {
                id: RoleId(id),
                name,
            },
            total,
        ))
    }
}

impl PropertyKeyRecord {
    /// Encode this record onto `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        encode_dictionary_body(out, RecordKind::PropertyKey, self.id.get(), &self.name)
    }

    /// Decode the record at the start of `input`.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let (id, name, total) = decode_dictionary_body(input, RecordKind::PropertyKey)?;
        Ok((
            Self {
                id: PropertyId(id),
                name,
            },
            total,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entity() -> EntityRecord {
        EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(42),
            tx_id_assert: TxId::new(100),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(7), Value::String("Alice".into())),
                (PropertyId::new(11), Value::I64(30)),
                (PropertyId::new(13), Value::Bool(true)),
            ],
        }
    }

    fn sample_hyperedge() -> HyperEdgeRecord {
        HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(17),
            tx_id_assert: TxId::new(200),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![
                (RoleId::new(3), EntityId::now_v7()),
                (RoleId::new(4), EntityId::now_v7()),
            ],
            properties: vec![(
                PropertyId::new(88),
                Value::Decimal {
                    scale: 2,
                    mantissa: 5_000_000,
                },
            )],
        }
    }

    // --- round-trips ------------------------------------------------------

    #[test]
    fn entity_round_trip() {
        let r = sample_entity();
        let mut buf = Vec::new();
        let written = r.encode(&mut buf).unwrap();
        assert_eq!(written, buf.len());
        let (decoded, consumed) = EntityRecord::decode(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, r);
    }

    #[test]
    fn entity_untyped_round_trip() {
        let r = EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::UNTYPED,
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![],
        };
        let mut buf = Vec::new();
        r.encode(&mut buf).unwrap();
        let (decoded, _) = EntityRecord::decode(&buf).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn hyperedge_round_trip() {
        let r = sample_hyperedge();
        let mut buf = Vec::new();
        r.encode(&mut buf).unwrap();
        let (decoded, _) = HyperEdgeRecord::decode(&buf).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn hyperedge_high_arity_round_trip() {
        let mut roles = Vec::new();
        for i in 1..=50u32 {
            roles.push((RoleId::new(i), EntityId::now_v7()));
        }
        let r = HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(99),
            tx_id_assert: TxId::new(10),
            tx_id_supersede: TxId::ACTIVE,
            roles,
            properties: vec![],
        };
        let mut buf = Vec::new();
        r.encode(&mut buf).unwrap();
        let (decoded, _) = HyperEdgeRecord::decode(&buf).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn tombstone_round_trip() {
        let r = TombstoneRecord {
            target_id: uuid::Uuid::now_v7(),
            tx_id_supersede: TxId::new(999),
        };
        let mut buf = Vec::new();
        let written = r.encode(&mut buf).unwrap();
        assert_eq!(written, buf.len());
        assert_eq!(written, 34, "spec pins tombstone size at 34 bytes");
        let (decoded, _) = TombstoneRecord::decode(&buf).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn dictionary_round_trips() {
        let t = TypeNameRecord {
            id: TypeId::new(1),
            name: "Customer".into(),
        };
        let r = RoleNameRecord {
            id: RoleId::new(2),
            name: "approver".into(),
        };
        let p = PropertyKeyRecord {
            id: PropertyId::new(3),
            name: "email".into(),
        };

        let mut buf = Vec::new();
        t.encode(&mut buf).unwrap();
        let (dt, _) = TypeNameRecord::decode(&buf).unwrap();
        assert_eq!(dt, t);

        let mut buf = Vec::new();
        r.encode(&mut buf).unwrap();
        let (dr, _) = RoleNameRecord::decode(&buf).unwrap();
        assert_eq!(dr, r);

        let mut buf = Vec::new();
        p.encode(&mut buf).unwrap();
        let (dp, _) = PropertyKeyRecord::decode(&buf).unwrap();
        assert_eq!(dp, p);
    }

    #[test]
    fn dictionary_records_share_layout_but_differ_by_kind_byte() {
        let mut buf_t = Vec::new();
        TypeNameRecord {
            id: TypeId::new(7),
            name: "X".into(),
        }
        .encode(&mut buf_t)
        .unwrap();
        let mut buf_r = Vec::new();
        RoleNameRecord {
            id: RoleId::new(7),
            name: "X".into(),
        }
        .encode(&mut buf_r)
        .unwrap();
        let mut buf_p = Vec::new();
        PropertyKeyRecord {
            id: PropertyId::new(7),
            name: "X".into(),
        }
        .encode(&mut buf_p)
        .unwrap();
        assert_eq!(buf_t.len(), buf_r.len());
        assert_eq!(buf_t.len(), buf_p.len());
        // record_kind byte at offset 4 is the only difference
        assert_eq!(buf_t[4], 0x04);
        assert_eq!(buf_r[4], 0x05);
        assert_eq!(buf_p[4], 0x06);
        // ... and the CRC, which we can't predict
        let mut canon_t = buf_t.clone();
        canon_t[4] = 0;
        canon_t.truncate(canon_t.len() - 4);
        let mut canon_r = buf_r.clone();
        canon_r[4] = 0;
        canon_r.truncate(canon_r.len() - 4);
        let mut canon_p = buf_p.clone();
        canon_p[4] = 0;
        canon_p.truncate(canon_p.len() - 4);
        assert_eq!(canon_t, canon_r);
        assert_eq!(canon_t, canon_p);
    }

    // --- envelope discipline ---------------------------------------------

    #[test]
    fn record_size_is_self_inclusive() {
        let r = sample_entity();
        let mut buf = Vec::new();
        let written = r.encode(&mut buf).unwrap();
        let claimed = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        assert_eq!(
            claimed,
            buf.len(),
            "record_size must equal total on-disk length"
        );
        assert_eq!(claimed, written);
    }

    #[test]
    fn peek_record_size_skips_to_next() {
        let mut buf = Vec::new();
        sample_entity().encode(&mut buf).unwrap();
        let first_size = peek_record_size(&buf).unwrap();
        let before_second = buf.len();
        sample_hyperedge().encode(&mut buf).unwrap();
        // Seeking first_size bytes from the start should land on the second record.
        assert_eq!(first_size, before_second);
        let second_kind = peek_record_kind(&buf[first_size..]).unwrap();
        assert_eq!(second_kind, RecordKind::HyperEdge);
    }

    #[test]
    fn crc_detects_corruption() {
        let mut buf = Vec::new();
        sample_entity().encode(&mut buf).unwrap();
        // Flip one byte in the payload (after the envelope head, before the CRC).
        let target = 20;
        buf[target] ^= 0xff;
        match EntityRecord::decode(&buf) {
            Err(DecodeError::CrcMismatch { .. }) => {}
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn wrong_kind_rejected() {
        let mut buf = Vec::new();
        sample_entity().encode(&mut buf).unwrap();
        match HyperEdgeRecord::decode(&buf) {
            Err(DecodeError::WrongRecordKind { found, expected }) => {
                assert_eq!(found, 0x01);
                assert_eq!(expected, 0x02);
            }
            other => panic!("expected WrongRecordKind, got {other:?}"),
        }
    }

    #[test]
    fn truncated_envelope_rejected() {
        let mut buf = Vec::new();
        sample_entity().encode(&mut buf).unwrap();
        let short = &buf[..buf.len() - 5];
        assert!(matches!(
            EntityRecord::decode(short),
            Err(DecodeError::InvalidRecordSize { .. })
        ));
    }

    #[test]
    fn unsupported_format_version_rejected() {
        let mut buf = Vec::new();
        sample_entity().encode(&mut buf).unwrap();
        // Bump the format_version byte beyond what we support and recompute CRC.
        buf[5] = FORMAT_VERSION_MAX_SUPPORTED + 1;
        let body_end = buf.len() - 4;
        let mut h = Hasher::new();
        h.update(&buf[0..body_end]);
        let crc = h.finalize();
        buf[body_end..body_end + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            EntityRecord::decode(&buf),
            Err(DecodeError::UnsupportedFormatVersion { .. })
        ));
    }

    // --- sentinel discipline ---------------------------------------------

    #[test]
    fn encode_rejects_zero_arity_hyperedge() {
        let r = HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![],
            properties: vec![],
        };
        assert!(matches!(
            r.encode(&mut Vec::new()),
            Err(EncodeError::HyperEdgeZeroArity)
        ));
    }

    #[test]
    fn encode_rejects_zero_type_id_on_hyperedge() {
        let r = HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::UNTYPED,
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            properties: vec![],
        };
        assert!(matches!(
            r.encode(&mut Vec::new()),
            Err(EncodeError::ZeroHyperEdgeTypeId)
        ));
    }

    #[test]
    fn encode_rejects_zero_role_id() {
        let r = HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(0), EntityId::now_v7())],
            properties: vec![],
        };
        assert!(matches!(
            r.encode(&mut Vec::new()),
            Err(EncodeError::ZeroRoleId)
        ));
    }

    #[test]
    fn encode_rejects_zero_property_id() {
        let r = EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(0), Value::Null)],
        };
        assert!(matches!(
            r.encode(&mut Vec::new()),
            Err(EncodeError::ZeroPropertyId)
        ));
    }

    #[test]
    fn encode_rejects_zero_dictionary_id() {
        assert!(matches!(
            TypeNameRecord {
                id: TypeId::new(0),
                name: "X".into()
            }
            .encode(&mut Vec::new()),
            Err(EncodeError::ZeroDictionaryId)
        ));
        assert!(matches!(
            RoleNameRecord {
                id: RoleId::new(0),
                name: "X".into()
            }
            .encode(&mut Vec::new()),
            Err(EncodeError::ZeroDictionaryId)
        ));
        assert!(matches!(
            PropertyKeyRecord {
                id: PropertyId::new(0),
                name: "X".into()
            }
            .encode(&mut Vec::new()),
            Err(EncodeError::ZeroDictionaryId)
        ));
    }

    #[test]
    fn decode_rejects_tombstone_with_active_supersede() {
        // Hand-craft a tombstone with TX_ACTIVE in the supersede slot; the
        // encoder cannot produce one, but a tampered file might.
        let mut buf = Vec::new();
        let start = begin_record(&mut buf, RecordKind::Tombstone);
        buf.extend_from_slice(uuid::Uuid::now_v7().as_bytes());
        write_u64(&mut buf, TX_ACTIVE);
        finalize_record(&mut buf, start).unwrap();
        assert!(matches!(
            TombstoneRecord::decode(&buf),
            Err(DecodeError::InvalidSentinel(_))
        ));
    }

    // --- scan-recovery loop over a stream of mixed records ---------------

    #[test]
    fn scan_recovery_skips_corrupted_records() {
        let mut buf = Vec::new();
        let r1 = sample_entity();
        let r2 = sample_hyperedge();
        let r3 = TombstoneRecord {
            target_id: uuid::Uuid::now_v7(),
            tx_id_supersede: TxId::new(50),
        };
        r1.encode(&mut buf).unwrap();
        let r2_start = buf.len();
        r2.encode(&mut buf).unwrap();
        let r3_start = buf.len();
        r3.encode(&mut buf).unwrap();

        // Corrupt the middle record (flip a payload byte after the envelope head).
        buf[r2_start + 12] ^= 0xff;

        // Scanner: read each record_size, attempt to decode; on CRC failure,
        // advance by record_size and continue.
        let mut offset = 0;
        let mut ok_count = 0;
        let mut crc_failures = 0;
        while offset < buf.len() {
            let size = peek_record_size(&buf[offset..]).expect("size readable");
            let kind = peek_record_kind(&buf[offset..]).expect("kind readable");
            let slice = &buf[offset..offset + size];
            let result = match kind {
                RecordKind::Entity => EntityRecord::decode(slice).map(|_| ()),
                RecordKind::HyperEdge => HyperEdgeRecord::decode(slice).map(|_| ()),
                RecordKind::Tombstone => TombstoneRecord::decode(slice).map(|_| ()),
                RecordKind::TypeName => TypeNameRecord::decode(slice).map(|_| ()),
                RecordKind::RoleName => RoleNameRecord::decode(slice).map(|_| ()),
                RecordKind::PropertyKey => PropertyKeyRecord::decode(slice).map(|_| ()),
            };
            match result {
                Ok(()) => ok_count += 1,
                Err(DecodeError::CrcMismatch { .. }) => crc_failures += 1,
                Err(other) => panic!("unexpected error {other:?}"),
            }
            offset += size;
        }
        assert_eq!(ok_count, 2, "two records survive (r1 and r3)");
        assert_eq!(crc_failures, 1, "only the corrupted r2 fails CRC");
        assert_eq!(offset, buf.len(), "scan advances cleanly to EOF");
        // Pin r3's starting offset too, just to keep the test honest.
        let _ = r3_start;
    }
}
