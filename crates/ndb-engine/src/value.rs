//! Self-describing tagged-union property value (§11.2).
//!
//! Layout on disk is `tag: u8` followed by a tag-specific payload. All
//! multi-byte fields are little-endian. The full tag table:
//!
//! | tag    | variant          | payload                                       |
//! |--------|------------------|-----------------------------------------------|
//! | `0x01` | `Null`           | (none)                                        |
//! | `0x02` | `Bool`           | `u8` (0 = false, anything else = true)        |
//! | `0x03` | `I64`            | `i64`                                         |
//! | `0x04` | `F64`            | `f64`                                         |
//! | `0x05` | `String`         | `u32` byte-length + UTF-8 bytes               |
//! | `0x06` | `Bytes`          | `u32` length + raw bytes                      |
//! | `0x07` | `Timestamp`      | `i64` microseconds since Unix epoch           |
//! | `0x08` | `EntityRef`      | `[u8; 16]` UUID v7                            |
//! | `0x09` | `Decimal`        | `u8` scale + `i128` mantissa                  |
//! | `0x0A` | `Vector`         | `u32` length + that many `f32`s               |
//! | `0xFF` | `Extension`      | `u32` length + arbitrary bytes (forward-compat) |

use crate::codec::{Cursor, write_f32, write_f64, write_i64, write_i128, write_u8, write_u32};
use crate::error::{DecodeError, EncodeError};
use crate::id::EntityId;

/// `Value::Null` tag.
pub const TAG_NULL: u8 = 0x01;
/// `Value::Bool` tag.
pub const TAG_BOOL: u8 = 0x02;
/// `Value::I64` tag.
pub const TAG_I64: u8 = 0x03;
/// `Value::F64` tag.
pub const TAG_F64: u8 = 0x04;
/// `Value::String` tag.
pub const TAG_STRING: u8 = 0x05;
/// `Value::Bytes` tag.
pub const TAG_BYTES: u8 = 0x06;
/// `Value::Timestamp` tag.
pub const TAG_TIMESTAMP: u8 = 0x07;
/// `Value::EntityRef` tag.
pub const TAG_UUID: u8 = 0x08;
/// `Value::Decimal` tag.
pub const TAG_DECIMAL: u8 = 0x09;
/// `Value::Vector` tag.
pub const TAG_VECTOR: u8 = 0x0A;
/// `Value::Extension` tag — reserved for forward-compatible payloads the
/// current build does not understand semantically.
pub const TAG_EXTENSION: u8 = 0xFF;

/// A property value as stored in `EntityRecord` and `HyperEdgeRecord`.
///
/// Equality on `F64` and `Vector` uses bitwise equality (`f64::to_bits`); two
/// `NaN`s with identical bit patterns therefore compare equal. This matches
/// the on-disk byte-for-byte semantics that round-trip tests need.
#[derive(Debug, Clone)]
pub enum Value {
    /// Absence of a value — explicit null, not the missing-property case.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    I64(i64),
    /// IEEE-754 double.
    F64(f64),
    /// UTF-8 text. Length stored as `u32`, max ~4 GiB.
    String(String),
    /// Opaque byte string.
    Bytes(Vec<u8>),
    /// Microseconds since the Unix epoch (positive or negative).
    Timestamp(i64),
    /// Reference to an entity by its internal UUID v7. The wire form does not
    /// distinguish entity vs hyperedge UUIDs — semantics live in indexes.
    EntityRef(EntityId),
    /// Fixed-point decimal: `value = mantissa × 10^-scale`. Range is the full
    /// `i128`, scale fits in `u8`.
    Decimal {
        /// Number of decimal places (0..=255).
        scale: u8,
        /// Signed mantissa.
        mantissa: i128,
    },
    /// Dense `f32` vector — embedding payloads. Length stored as `u32`.
    Vector(Vec<f32>),
    /// Forward-compatibility escape hatch. Carries opaque bytes the current
    /// build cannot interpret but must round-trip.
    Extension(Vec<u8>),
}

impl PartialEq for Value {
    // Explicit per-variant arms (rather than merged identical bodies) so adding
    // a new variant produces a small, mechanical diff.
    #[allow(clippy::match_same_arms)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::I64(a), Value::I64(b)) => a == b,
            (Value::F64(a), Value::F64(b)) => a.to_bits() == b.to_bits(),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::EntityRef(a), Value::EntityRef(b)) => a == b,
            (
                Value::Decimal {
                    scale: sa,
                    mantissa: ma,
                },
                Value::Decimal {
                    scale: sb,
                    mantissa: mb,
                },
            ) => sa == sb && ma == mb,
            (Value::Vector(a), Value::Vector(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| x.to_bits() == y.to_bits())
            }
            (Value::Extension(a), Value::Extension(b)) => a == b,
            _ => false,
        }
    }
}

impl Value {
    /// Tag byte that would be emitted for this variant.
    #[must_use]
    pub fn tag(&self) -> u8 {
        match self {
            Value::Null => TAG_NULL,
            Value::Bool(_) => TAG_BOOL,
            Value::I64(_) => TAG_I64,
            Value::F64(_) => TAG_F64,
            Value::String(_) => TAG_STRING,
            Value::Bytes(_) => TAG_BYTES,
            Value::Timestamp(_) => TAG_TIMESTAMP,
            Value::EntityRef(_) => TAG_UUID,
            Value::Decimal { .. } => TAG_DECIMAL,
            Value::Vector(_) => TAG_VECTOR,
            Value::Extension(_) => TAG_EXTENSION,
        }
    }

    /// Append the encoded form of `self` to `out`. Returns the byte count
    /// written.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        let start = out.len();
        match self {
            Value::Null => write_u8(out, TAG_NULL),
            Value::Bool(b) => {
                write_u8(out, TAG_BOOL);
                write_u8(out, u8::from(*b));
            }
            Value::I64(n) => {
                write_u8(out, TAG_I64);
                write_i64(out, *n);
            }
            Value::F64(f) => {
                write_u8(out, TAG_F64);
                write_f64(out, *f);
            }
            Value::String(s) => {
                let len = u32::try_from(s.len())
                    .map_err(|_| EncodeError::StringLengthOverflow(s.len()))?;
                write_u8(out, TAG_STRING);
                write_u32(out, len);
                out.extend_from_slice(s.as_bytes());
            }
            Value::Bytes(b) => {
                let len =
                    u32::try_from(b.len()).map_err(|_| EncodeError::ByteLengthOverflow(b.len()))?;
                write_u8(out, TAG_BYTES);
                write_u32(out, len);
                out.extend_from_slice(b);
            }
            Value::Timestamp(us) => {
                write_u8(out, TAG_TIMESTAMP);
                write_i64(out, *us);
            }
            Value::EntityRef(id) => {
                write_u8(out, TAG_UUID);
                out.extend_from_slice(id.as_bytes());
            }
            Value::Decimal { scale, mantissa } => {
                write_u8(out, TAG_DECIMAL);
                write_u8(out, *scale);
                write_i128(out, *mantissa);
            }
            Value::Vector(v) => {
                let len = u32::try_from(v.len())
                    .map_err(|_| EncodeError::VectorLengthOverflow(v.len()))?;
                write_u8(out, TAG_VECTOR);
                write_u32(out, len);
                for f in v {
                    write_f32(out, *f);
                }
            }
            Value::Extension(b) => {
                let len = u32::try_from(b.len())
                    .map_err(|_| EncodeError::ExtensionLengthOverflow(b.len()))?;
                write_u8(out, TAG_EXTENSION);
                write_u32(out, len);
                out.extend_from_slice(b);
            }
        }
        Ok(out.len() - start)
    }

    /// Decode one `Value` starting at the beginning of `input`. Returns the
    /// value and the number of bytes consumed.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let mut c = Cursor::new(input);
        let value = Self::decode_from(&mut c)?;
        Ok((value, c.pos()))
    }

    /// Decode one `Value` from a cursor in-place. Used by record decoders that
    /// inline a value alongside other fields.
    pub fn decode_from(c: &mut Cursor<'_>) -> Result<Self, DecodeError> {
        let tag = c.read_u8()?;
        Ok(match tag {
            TAG_NULL => Value::Null,
            TAG_BOOL => Value::Bool(c.read_u8()? != 0),
            TAG_I64 => Value::I64(c.read_i64()?),
            TAG_F64 => Value::F64(c.read_f64()?),
            TAG_STRING => {
                let len = c.read_u32()? as usize;
                let bytes = c.read_slice(len)?;
                Value::String(std::str::from_utf8(bytes)?.to_owned())
            }
            TAG_BYTES => {
                let len = c.read_u32()? as usize;
                Value::Bytes(c.read_slice(len)?.to_vec())
            }
            TAG_TIMESTAMP => Value::Timestamp(c.read_i64()?),
            TAG_UUID => Value::EntityRef(EntityId::from_bytes(c.read_array::<16>()?)),
            TAG_DECIMAL => {
                let scale = c.read_u8()?;
                let mantissa = c.read_i128()?;
                Value::Decimal { scale, mantissa }
            }
            TAG_VECTOR => {
                let len = c.read_u32()? as usize;
                let mut v = Vec::with_capacity(len);
                for _ in 0..len {
                    v.push(c.read_f32()?);
                }
                Value::Vector(v)
            }
            TAG_EXTENSION => {
                let len = c.read_u32()? as usize;
                Value::Extension(c.read_slice(len)?.to_vec())
            }
            other => return Err(DecodeError::UnknownValueTag { tag: other }),
        })
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
mod tests {
    use super::*;

    fn round_trip(v: Value) {
        let mut buf = Vec::new();
        let written = v.encode(&mut buf).expect("encode succeeds");
        assert_eq!(written, buf.len(), "encode reports exact byte count");
        let (decoded, consumed) = Value::decode(&buf).expect("decode succeeds");
        assert_eq!(consumed, buf.len(), "decode consumes all bytes");
        assert_eq!(decoded, v, "round-trip preserves value");
        assert_eq!(decoded.tag(), v.tag());
    }

    #[test]
    fn null_round_trip() {
        round_trip(Value::Null);
    }

    #[test]
    fn bool_round_trip() {
        round_trip(Value::Bool(true));
        round_trip(Value::Bool(false));
    }

    #[test]
    fn i64_round_trip() {
        round_trip(Value::I64(0));
        round_trip(Value::I64(-1));
        round_trip(Value::I64(i64::MIN));
        round_trip(Value::I64(i64::MAX));
    }

    #[test]
    fn f64_round_trip() {
        round_trip(Value::F64(0.0));
        round_trip(Value::F64(-0.0));
        round_trip(Value::F64(std::f64::consts::PI));
        round_trip(Value::F64(f64::INFINITY));
        round_trip(Value::F64(f64::NEG_INFINITY));
        // NaN bit-pattern preserved
        round_trip(Value::F64(f64::from_bits(0x7ff8_0000_0000_0001)));
    }

    #[test]
    fn string_round_trip() {
        round_trip(Value::String(String::new()));
        round_trip(Value::String("hello".into()));
        round_trip(Value::String("héllo 🦀 thế giới".into()));
    }

    #[test]
    fn bytes_round_trip() {
        round_trip(Value::Bytes(vec![]));
        round_trip(Value::Bytes(b"abc\xff\x00\x01".to_vec()));
    }

    #[test]
    fn timestamp_round_trip() {
        round_trip(Value::Timestamp(0));
        round_trip(Value::Timestamp(-1));
        round_trip(Value::Timestamp(1_700_000_000_000_000));
        round_trip(Value::Timestamp(i64::MAX));
        round_trip(Value::Timestamp(i64::MIN));
    }

    #[test]
    fn uuid_round_trip() {
        let id = EntityId::now_v7();
        round_trip(Value::EntityRef(id));
        round_trip(Value::EntityRef(EntityId::from_bytes([0u8; 16])));
        round_trip(Value::EntityRef(EntityId::from_bytes([0xff; 16])));
    }

    #[test]
    fn decimal_round_trip() {
        round_trip(Value::Decimal {
            scale: 0,
            mantissa: 0,
        });
        round_trip(Value::Decimal {
            scale: 2,
            mantissa: 1_234_567_890,
        });
        round_trip(Value::Decimal {
            scale: 255,
            mantissa: i128::MIN,
        });
        round_trip(Value::Decimal {
            scale: 18,
            mantissa: i128::MAX,
        });
    }

    #[test]
    fn vector_round_trip() {
        round_trip(Value::Vector(vec![]));
        round_trip(Value::Vector(vec![1.0, -2.0, 3.5, -7.25]));
        round_trip(Value::Vector(vec![f32::NAN; 4]));
    }

    #[test]
    fn extension_round_trip() {
        round_trip(Value::Extension(vec![]));
        round_trip(Value::Extension(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn unknown_tag_errors() {
        let buf = [0x42_u8, 0x00];
        match Value::decode(&buf) {
            Err(DecodeError::UnknownValueTag { tag }) => assert_eq!(tag, 0x42),
            other => panic!("expected UnknownValueTag, got {other:?}"),
        }
    }

    #[test]
    fn truncated_string_errors() {
        let mut buf = Vec::new();
        Value::String("hello".into()).encode(&mut buf).unwrap();
        buf.pop();
        assert!(matches!(
            Value::decode(&buf),
            Err(DecodeError::Truncated { .. })
        ));
    }

    #[test]
    fn truncated_vector_errors() {
        let mut buf = Vec::new();
        Value::Vector(vec![1.0, 2.0, 3.0]).encode(&mut buf).unwrap();
        buf.truncate(buf.len() - 2);
        assert!(matches!(
            Value::decode(&buf),
            Err(DecodeError::Truncated { .. })
        ));
    }

    #[test]
    fn invalid_utf8_errors() {
        let mut buf = vec![TAG_STRING];
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        assert!(matches!(
            Value::decode(&buf),
            Err(DecodeError::InvalidUtf8(_))
        ));
    }

    #[test]
    fn on_disk_byte_layout_pins_le_encoding() {
        // Pin the exact bytes for a couple of values so an accidental
        // endianness swap or layout change fails loudly.
        let mut buf = Vec::new();
        Value::I64(1).encode(&mut buf).unwrap();
        assert_eq!(buf, vec![0x03, 0x01, 0, 0, 0, 0, 0, 0, 0]);

        let mut buf = Vec::new();
        Value::String("hi".into()).encode(&mut buf).unwrap();
        assert_eq!(buf, vec![0x05, 0x02, 0, 0, 0, b'h', b'i']);
    }
}
