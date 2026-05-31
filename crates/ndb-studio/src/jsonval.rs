//! Conversion between the engine's `Value` and JSON, plus a stable type-hint
//! string per `Value` variant for the catalog.
//!
//! Tables and record detail serialize `Value` *out* to JSON; create/edit
//! parses simple JSON scalars (null / bool / number / string) *in* to `Value`.
//! Rich variants (Vector, Bytes, Timestamp, Decimal, `EntityRef`) round-trip out
//! for display but are not user-editable in v1 — they render as tagged objects
//! so the UI can show them without shipping large payloads (a vector becomes
//! `{"$vec": <len>}`, not the whole embedding).

use ndb_engine::value::Value;
use serde_json::{Value as J, json};

/// A short, stable label for a `Value` variant — used by the catalog so the UI
/// can show each property's inferred type without re-deriving it.
#[must_use]
pub fn type_hint(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::I64(_) => "int",
        Value::F64(_) => "float",
        Value::String(_) => "string",
        Value::Bytes(_) => "bytes",
        Value::Timestamp(_) => "timestamp",
        Value::EntityRef(_) => "ref",
        Value::Decimal { .. } => "decimal",
        Value::Vector(_) => "vector",
        Value::Extension(_) => "extension",
    }
}

/// Serialize a `Value` to JSON for display.
#[must_use]
pub fn to_json(v: &Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => json!(b),
        Value::I64(n) => json!(n),
        Value::F64(f) => json!(f),
        Value::String(s) => json!(s),
        Value::Timestamp(t) => json!({ "$ts": t }),
        Value::EntityRef(e) => json!({ "$ref": e.into_uuid().to_string() }),
        Value::Decimal { scale, mantissa } => json!({ "$dec": format_decimal(*scale, *mantissa) }),
        Value::Vector(xs) => json!({ "$vec": xs.len() }),
        Value::Bytes(b) => json!({ "$bytes": b.len() }),
        Value::Extension(b) => json!({ "$ext": b.len() }),
    }
}

/// Parse a simple JSON scalar into a `Value` for create/edit. Integers map to
/// `I64`, other numbers to `F64`; objects/arrays are rejected in v1.
///
/// # Errors
/// Returns `Err` with a human message if the JSON is a shape v1 cannot store
/// (array or object).
pub fn from_json(j: &J) -> Result<Value, String> {
    match j {
        J::Null => Ok(Value::Null),
        J::Bool(b) => Ok(Value::Bool(*b)),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::I64(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::F64(f))
            } else {
                Err("number out of range".to_string())
            }
        }
        J::String(s) => Ok(Value::String(s.clone())),
        J::Array(_) | J::Object(_) => {
            Err("v1 can only edit scalar values (null, bool, number, string)".to_string())
        }
    }
}

fn format_decimal(scale: u8, mantissa: i128) -> String {
    if scale == 0 {
        return mantissa.to_string();
    }
    let neg = mantissa < 0;
    let digits = mantissa.unsigned_abs().to_string();
    let scale = scale as usize;
    let s = if digits.len() <= scale {
        let pad = "0".repeat(scale - digits.len() + 1);
        format!("{pad}{digits}")
    } else {
        digits
    };
    let point = s.len() - scale;
    let out = format!("{}.{}", &s[..point], &s[point..]);
    if neg { format!("-{out}") } else { out }
}
