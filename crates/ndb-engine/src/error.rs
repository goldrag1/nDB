//! Error types for the record + value codecs.

use thiserror::Error;

/// Errors raised while serialising a record or `Value` to its on-disk form.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// `HyperEdgeRecord.arity` must be at least 1 (§11.3). A zero-role
    /// hyperedge is semantically an entity and should be written as an
    /// `EntityRecord` instead.
    #[error("hyperedge arity must be ≥ 1; a 0-arity hyperedge is an entity")]
    HyperEdgeZeroArity,

    /// `role_id == 0` is reserved and illegal everywhere (§11.3).
    #[error("role_id must be non-zero")]
    ZeroRoleId,

    /// `prop_id == 0` is reserved and illegal everywhere (§11.3).
    #[error("prop_id must be non-zero")]
    ZeroPropertyId,

    /// Hyperedges require a declared type; `TYPE_UNTYPED` is entity-only.
    #[error("hyperedge type_id must be non-zero (TYPE_UNTYPED is entity-only)")]
    ZeroHyperEdgeTypeId,

    /// Dictionary records cannot use id 0 (would clash with `TYPE_UNTYPED` /
    /// reserved role and property slots).
    #[error("dictionary record id must be non-zero")]
    ZeroDictionaryId,

    /// Arity is stored as `u8` (§11.2). A vector with more than 255 roles
    /// cannot be represented.
    #[error("arity {0} exceeds u8::MAX")]
    ArityOverflow(usize),

    /// `property_count` is stored as `u16` (§11.2).
    #[error("property_count {0} exceeds u16::MAX")]
    PropertyCountOverflow(usize),

    /// `Value::Vector` length is stored as `u32`.
    #[error("vector length {0} exceeds u32::MAX")]
    VectorLengthOverflow(usize),

    /// `Value::String` byte length is stored as `u32`.
    #[error("string length {0} exceeds u32::MAX bytes")]
    StringLengthOverflow(usize),

    /// `Value::Bytes` length is stored as `u32`.
    #[error("byte length {0} exceeds u32::MAX bytes")]
    ByteLengthOverflow(usize),

    /// `Value::Extension` length is stored as `u32`.
    #[error("extension length {0} exceeds u32::MAX bytes")]
    ExtensionLengthOverflow(usize),

    /// Dictionary name length stored as `u32` (UTF-8 byte count, not chars).
    #[error("dictionary name length {0} exceeds u32::MAX bytes")]
    DictionaryNameOverflow(usize),

    /// `record_size` is stored as `u32`.
    #[error("record_size {0} exceeds u32::MAX")]
    RecordSizeOverflow(usize),
}

/// Errors raised while parsing a record or `Value` from on-disk bytes.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// Not enough bytes remain to satisfy a read of the requested size.
    /// `offset` is the cursor position where the short read started; `needed`
    /// is the extra bytes that would have been required.
    #[error("input truncated at offset {offset}: need {needed} more byte(s)")]
    Truncated {
        /// Cursor position when the short read was attempted.
        offset: usize,
        /// Additional bytes that would have been required.
        needed: usize,
    },

    /// `record_size` field claims more bytes than the supplied slice can
    /// provide. This is recoverable in scan mode — skip the slice and try the
    /// next one — but fatal for single-record decoders.
    #[error("record_size {claimed} exceeds available bytes {available}")]
    InvalidRecordSize {
        /// Value read from the `record_size` field.
        claimed: usize,
        /// Bytes available in the input slice.
        available: usize,
    },

    /// `record_size` is smaller than the minimum legal record (header + CRC).
    #[error("record_size {claimed} too small to contain headers (minimum {minimum})")]
    RecordSizeTooSmall {
        /// Value read from the `record_size` field.
        claimed: usize,
        /// Minimum legal record byte count for this record kind.
        minimum: usize,
    },

    /// `record_kind` byte does not match any of the six defined kinds.
    #[error("unknown record_kind 0x{kind:02x}")]
    UnknownRecordKind {
        /// The unrecognised byte value.
        kind: u8,
    },

    /// `record_kind` was a valid kind, but not the one this decoder expected.
    #[error("unexpected record_kind 0x{found:02x}, expected 0x{expected:02x}")]
    WrongRecordKind {
        /// The kind byte found on disk.
        found: u8,
        /// The kind byte the caller asked to decode.
        expected: u8,
    },

    /// `format_version` is newer than this build supports.
    #[error("unsupported format_version {version} (this build supports up to {supported})")]
    UnsupportedFormatVersion {
        /// On-disk format version byte.
        version: u8,
        /// Highest format version this build can decode.
        supported: u8,
    },

    /// Stored CRC32 does not match the computed CRC32 of the record body.
    #[error("CRC32 mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC value read from the record's CRC field.
        stored: u32,
        /// CRC value computed over the on-disk bytes (excluding the CRC field
        /// itself).
        computed: u32,
    },

    /// `Value` tag byte does not match any defined tag.
    #[error("unknown Value tag 0x{tag:02x}")]
    UnknownValueTag {
        /// The unrecognised tag byte.
        tag: u8,
    },

    /// A string payload was not valid UTF-8.
    #[error("invalid UTF-8 in string payload")]
    InvalidUtf8(#[from] std::str::Utf8Error),

    /// A field that must not be zero (e.g. `role_id`, `prop_id`,
    /// dictionary id) was read as zero.
    #[error("invalid sentinel: {0}")]
    InvalidSentinel(&'static str),

    /// After decoding the declared structure, extra bytes remain inside the
    /// record body. Indicates corruption or a version mismatch the
    /// `format_version` byte didn't catch.
    #[error("trailing bytes after record body: {0} byte(s) remain")]
    TrailingBytes(usize),
}
