//! Apache Arrow IPC interop for nDB.
//!
//! Bridges [`Engine::snapshot_iter`] output to Arrow `RecordBatch` and to the
//! IPC byte stream. Consumers: Polars, pandas (via pyarrow), DuckDB, anything
//! else that speaks Arrow. Single-batch, in-memory; for very large databases
//! callers should chunk by `(record_kind, type_id)` themselves.
//!
//! # v1 decisions baked in here
//!
//! - **Schema is denormalised.** One Arrow column per
//!   `(record_kind, type_id, property_id)` tuple actually observed in the
//!   record set, plus a fixed prefix of identity columns:
//!
//!   | column        | type           | notes                                          |
//!   |---------------|----------------|------------------------------------------------|
//!   | `record_kind` | `Utf8`         | `"entity"` / `"hyperedge"` / `"tombstone"`     |
//!   | `primary_id`  | `FixedSizeBinary(16)` | UUID v7 bytes                           |
//!   | `type_id`     | `UInt32` (nullable) | `None` for tombstones                     |
//!   | `tx_id_assert`| `UInt64` (nullable) | `None` for tombstones                     |
//!   | `tx_id_supersede` | `UInt64` (nullable) | `None` if active                      |
//!   | `prop:<kind>:<type_id>:<property_id>` | per-property | one per observed prop |
//!
//!   Property column names use the dictionary form so consumers can pivot
//!   trivially. Callers with a dictionary in hand can rename to human-friendly
//!   names downstream.
//!
//! - **Property column type is chosen by the *first* value observed for that
//!   column.** All subsequent values must match. Mixed-type properties (which
//!   nDB's tagged-union permits) cause a [`ArrowError::TypeMismatch`] at
//!   conversion time — by design; Arrow columns are typed, the engine is not.
//!   Workaround for mixed-type properties: filter the record set first.
//!
//! - **`Value::Vector` becomes `List<Float32>`.** `Value::Decimal` widens to
//!   `Float64` (lossy past ~15 digits — acceptable for v1; v2 will use Arrow's
//!   native decimal type). `Value::Extension` becomes `Binary`.
//!
//! - **Hyperedge roles are flattened into one `roles` column of type
//!   `List<Struct{role_id: UInt32, entity_id: FixedSizeBinary(16)}>`.** Empty
//!   list for entities and tombstones.
//!
//! - **Dictionary records are dropped.** `TypeName`, `RoleName`, and
//!   `PropertyKey` records carry no row-level data; they're metadata. Callers
//!   that want the dictionaries should consume them separately via
//!   [`build_dictionaries`].

#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::match_same_arms,
    clippy::needless_borrows_for_generic_args
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, Float64Builder,
    Int64Builder, ListBuilder, StringBuilder, StructBuilder, UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_schema::{ArrowError as SchemaArrowError, DataType, Field, Schema, SchemaRef};

use ndb_engine::id::{PropertyId, TypeId};
use ndb_engine::record::{Record, RecordKind};
use ndb_engine::value::Value;

/// Errors raised when converting nDB records to Arrow.
#[derive(Debug, thiserror::Error)]
pub enum ArrowError {
    /// A property column saw a value whose tag conflicts with the column type
    /// chosen by the first observed value.
    #[error(
        "property column {column} expected tag {expected:?} but saw tag {observed:?}; \
        Arrow columns are statically typed — filter the record set or split by tag"
    )]
    TypeMismatch {
        /// The column name (`prop:<kind>:<type_id>:<property_id>`).
        column: String,
        /// Arrow data type the column was bound to.
        expected: DataType,
        /// Arrow data type the offending value would have required.
        observed: DataType,
    },

    /// `Value::Vector` has dimension N for one row and M ≠ N for another in
    /// the same column. The Arrow `List<Float32>` representation does not
    /// require fixed inner length, so this is currently informational; v2 may
    /// upgrade vectors to `FixedSizeList` once stable per-column dimension is
    /// declared via schema metadata.
    #[error("vector dimension mismatch in column {column}: expected {expected}, observed {observed}")]
    VectorDimMismatch {
        /// Column name.
        column: String,
        /// First observed dimension.
        expected: usize,
        /// Conflicting dimension.
        observed: usize,
    },

    /// Underlying Arrow library error (schema mismatch, builder failure).
    #[error(transparent)]
    Arrow(#[from] SchemaArrowError),
}

// ---------------------------------------------------------------------------
// Column identity
// ---------------------------------------------------------------------------

/// `(record_kind_byte, type_id, property_id)` — the keying tuple for property
/// columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PropColKey {
    kind: u8,
    type_id: u32,
    property_id: u32,
}

impl PropColKey {
    fn column_name(self) -> String {
        let kind = match self.kind {
            0x01 => "entity",
            0x02 => "hyperedge",
            _ => "other",
        };
        format!("prop:{}:{}:{}", kind, self.type_id, self.property_id)
    }
}

// ---------------------------------------------------------------------------
// Public conversion entry points
// ---------------------------------------------------------------------------

/// Build a single Arrow `RecordBatch` from a slice of nDB records.
///
/// Errors if any property column observes mismatched value tags across rows.
pub fn records_to_batch(records: &[Record]) -> Result<RecordBatch, ArrowError> {
    // Two passes: discover the property column set + per-column Arrow data
    // type, then build each column.
    let prop_cols = discover_prop_columns(records)?;
    build_batch(records, &prop_cols)
}

/// Convenience: convert records and serialise to an in-memory Arrow IPC stream
/// (the "Arrow streaming format") suitable for handing to a Python consumer.
pub fn records_to_ipc_stream(records: &[Record]) -> Result<Vec<u8>, ArrowError> {
    let batch = records_to_batch(records)?;
    let mut buf = Vec::with_capacity(64 * 1024);
    {
        let mut writer = StreamWriter::try_new_with_options(
            &mut buf,
            batch.schema_ref(),
            IpcWriteOptions::default(),
        )?;
        writer.write(&batch)?;
        writer.finish()?;
    }
    Ok(buf)
}

/// Strip dictionary records out of a slice and return them grouped by kind.
/// The first member of each tuple is the dictionary id, the second the name.
#[derive(Debug, Default, Clone)]
pub struct Dictionaries {
    /// Type-name dictionary (`type_id` → name).
    pub types: Vec<(u32, String)>,
    /// Role-name dictionary (`role_id` → name).
    pub roles: Vec<(u32, String)>,
    /// Property-key dictionary (`property_id` → name).
    pub properties: Vec<(u32, String)>,
}

/// Pull `TypeName` / `RoleName` / `PropertyKey` records into a [`Dictionaries`]
/// bundle. Useful for renaming the denormalised columns downstream.
pub fn build_dictionaries(records: &[Record]) -> Dictionaries {
    let mut d = Dictionaries::default();
    for r in records {
        match r {
            Record::TypeName(t) => d.types.push((t.id.0, t.name.clone())),
            Record::RoleName(t) => d.roles.push((t.id.0, t.name.clone())),
            Record::PropertyKey(t) => d.properties.push((t.id.0, t.name.clone())),
            _ => {}
        }
    }
    d
}

// ---------------------------------------------------------------------------
// Column discovery
// ---------------------------------------------------------------------------

fn discover_prop_columns(
    records: &[Record],
) -> Result<BTreeMap<PropColKey, DataType>, ArrowError> {
    let mut out: BTreeMap<PropColKey, DataType> = BTreeMap::new();
    for r in records {
        match r {
            Record::Entity(e) => {
                for (pid, v) in &e.properties {
                    bind_prop_column(&mut out, RecordKind::Entity, e.type_id, *pid, v)?;
                }
            }
            Record::HyperEdge(h) => {
                for (pid, v) in &h.properties {
                    bind_prop_column(&mut out, RecordKind::HyperEdge, h.type_id, *pid, v)?;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn bind_prop_column(
    out: &mut BTreeMap<PropColKey, DataType>,
    kind: RecordKind,
    type_id: TypeId,
    pid: PropertyId,
    v: &Value,
) -> Result<(), ArrowError> {
    let key = PropColKey {
        kind: kind.as_byte(),
        type_id: type_id.0,
        property_id: pid.0,
    };
    let dt = value_to_dtype(v);
    match out.get(&key) {
        None => {
            out.insert(key, dt);
        }
        Some(existing) if existing == &dt => {}
        Some(_) if matches!(v, Value::Null) => {
            // Null is compatible with any existing dtype.
        }
        Some(existing) => {
            return Err(ArrowError::TypeMismatch {
                column: key.column_name(),
                expected: existing.clone(),
                observed: dt,
            });
        }
    }
    Ok(())
}

fn value_to_dtype(v: &Value) -> DataType {
    match v {
        Value::Null => DataType::Null,
        Value::Bool(_) => DataType::Boolean,
        Value::I64(_) => DataType::Int64,
        Value::F64(_) => DataType::Float64,
        Value::String(_) => DataType::Utf8,
        Value::Bytes(_) => DataType::Binary,
        Value::Timestamp(_) => DataType::Int64,
        Value::EntityRef(_) => DataType::FixedSizeBinary(16),
        // i128 mantissa would need Decimal128(38, scale); for v1 we widen to f64.
        // Acceptable up to ~15 significant digits; documented in module preamble.
        Value::Decimal { .. } => DataType::Float64,
        Value::Vector(_) => {
            DataType::List(Arc::new(Field::new("item", DataType::Float32, false)))
        }
        Value::Extension(_) => DataType::Binary,
    }
}

// ---------------------------------------------------------------------------
// Batch construction
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PropBuilder {
    Null(usize),
    Bool(BooleanBuilder),
    I64(Int64Builder),
    F64(Float64Builder),
    Utf8(StringBuilder),
    Bin(BinaryBuilder),
    Uuid(FixedSizeBinaryBuilder),
    Vector(ListBuilder<Float32Builder>),
}

impl PropBuilder {
    fn for_dtype(dt: &DataType) -> Self {
        match dt {
            DataType::Null => Self::Null(0),
            DataType::Boolean => Self::Bool(BooleanBuilder::new()),
            DataType::Int64 => Self::I64(Int64Builder::new()),
            DataType::Float64 => Self::F64(Float64Builder::new()),
            DataType::Utf8 => Self::Utf8(StringBuilder::new()),
            DataType::Binary => Self::Bin(BinaryBuilder::new()),
            DataType::FixedSizeBinary(16) => Self::Uuid(FixedSizeBinaryBuilder::new(16)),
            DataType::List(_) => Self::Vector(ListBuilder::new(Float32Builder::new())),
            _ => unreachable!("value_to_dtype produces a fixed shape"),
        }
    }

    fn append_null(&mut self) {
        match self {
            Self::Null(n) => *n += 1,
            Self::Bool(b) => b.append_null(),
            Self::I64(b) => b.append_null(),
            Self::F64(b) => b.append_null(),
            Self::Utf8(b) => b.append_null(),
            Self::Bin(b) => b.append_null(),
            Self::Uuid(b) => b.append_null(),
            Self::Vector(b) => b.append_null(),
        }
    }

    fn append_value(&mut self, v: &Value, col: &str) -> Result<(), ArrowError> {
        match (self, v) {
            (Self::Null(n), Value::Null) => {
                *n += 1;
                Ok(())
            }
            (b, Value::Null) => {
                b.append_null();
                Ok(())
            }
            (Self::Bool(b), Value::Bool(x)) => {
                b.append_value(*x);
                Ok(())
            }
            (Self::I64(b), Value::I64(x)) => {
                b.append_value(*x);
                Ok(())
            }
            (Self::I64(b), Value::Timestamp(x)) => {
                b.append_value(*x);
                Ok(())
            }
            (Self::F64(b), Value::F64(x)) => {
                b.append_value(*x);
                Ok(())
            }
            (Self::F64(b), Value::Decimal { scale, mantissa }) => {
                let denom = 10f64.powi(i32::from(*scale));
                #[allow(clippy::cast_precision_loss)]
                let raw = (*mantissa) as f64;
                b.append_value(raw / denom);
                Ok(())
            }
            (Self::Utf8(b), Value::String(x)) => {
                b.append_value(x);
                Ok(())
            }
            (Self::Bin(b), Value::Bytes(x)) => {
                b.append_value(x);
                Ok(())
            }
            (Self::Bin(b), Value::Extension(x)) => {
                b.append_value(x);
                Ok(())
            }
            (Self::Uuid(b), Value::EntityRef(uuid)) => {
                b.append_value(uuid.as_bytes()).map_err(ArrowError::Arrow)?;
                Ok(())
            }
            (Self::Vector(b), Value::Vector(xs)) => {
                let inner = b.values();
                for x in xs {
                    inner.append_value(*x);
                }
                b.append(true);
                Ok(())
            }
            (other, v) => Err(ArrowError::TypeMismatch {
                column: col.to_string(),
                expected: other.declared_type(),
                observed: value_to_dtype(v),
            }),
        }
    }

    fn declared_type(&self) -> DataType {
        match self {
            Self::Null(_) => DataType::Null,
            Self::Bool(_) => DataType::Boolean,
            Self::I64(_) => DataType::Int64,
            Self::F64(_) => DataType::Float64,
            Self::Utf8(_) => DataType::Utf8,
            Self::Bin(_) => DataType::Binary,
            Self::Uuid(_) => DataType::FixedSizeBinary(16),
            Self::Vector(_) => {
                DataType::List(Arc::new(Field::new("item", DataType::Float32, false)))
            }
        }
    }

    fn finish(self, len: usize) -> ArrayRef {
        match self {
            Self::Null(_) => Arc::new(arrow_array::NullArray::new(len)),
            Self::Bool(mut b) => Arc::new(b.finish()),
            Self::I64(mut b) => Arc::new(b.finish()),
            Self::F64(mut b) => Arc::new(b.finish()),
            Self::Utf8(mut b) => Arc::new(b.finish()),
            Self::Bin(mut b) => Arc::new(b.finish()),
            Self::Uuid(mut b) => Arc::new(b.finish()),
            Self::Vector(mut b) => Arc::new(b.finish()),
        }
    }
}

fn roles_field() -> Field {
    let struct_fields = arrow_schema::Fields::from(vec![
        Field::new("role_id", DataType::UInt32, false),
        Field::new("entity_id", DataType::FixedSizeBinary(16), false),
    ]);
    // Inner field of `ListBuilder<StructBuilder>` is constructed nullable by
    // arrow-rs; declare it nullable here to match. List itself is non-null
    // (every row has a roles list, possibly empty).
    Field::new(
        "roles",
        DataType::List(Arc::new(Field::new(
            "item",
            DataType::Struct(struct_fields),
            true,
        ))),
        false,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn build_roles_builder() -> ListBuilder<StructBuilder> {
    let fields = vec![
        Field::new("role_id", DataType::UInt32, false),
        Field::new("entity_id", DataType::FixedSizeBinary(16), false),
    ];
    let builders: Vec<Box<dyn arrow_array::builder::ArrayBuilder>> = vec![
        Box::new(UInt32Builder::new()),
        Box::new(FixedSizeBinaryBuilder::new(16)),
    ];
    ListBuilder::new(StructBuilder::new(fields, builders))
}

fn build_batch(
    records: &[Record],
    prop_cols: &BTreeMap<PropColKey, DataType>,
) -> Result<RecordBatch, ArrowError> {
    // Filter to data rows (entity / hyperedge / tombstone). Dictionary
    // records carry no row-level data — they're metadata and are dropped here.
    let rows: Vec<&Record> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                Record::Entity(_) | Record::HyperEdge(_) | Record::Tombstone(_)
            )
        })
        .collect();
    let n_rows = rows.len();

    // Identity columns.
    let mut kind_b = StringBuilder::new();
    let mut primary_b = FixedSizeBinaryBuilder::new(16);
    let mut type_b = UInt32Builder::new();
    let mut tx_assert_b = UInt64Builder::new();
    let mut tx_super_b = UInt64Builder::new();
    let mut roles_b = build_roles_builder();

    // Per-property column builders, in the same iteration order as the
    // BTreeMap (deterministic — sorted lexicographically by tuple key).
    let mut prop_builders: Vec<(PropColKey, PropBuilder, String)> = prop_cols
        .iter()
        .map(|(k, dt)| (*k, PropBuilder::for_dtype(dt), k.column_name()))
        .collect();

    for rec in &rows {
        let (kind_str, primary_bytes, type_id_opt, tx_assert_opt, tx_super_opt): (
            &str,
            [u8; 16],
            Option<u32>,
            Option<u64>,
            Option<u64>,
        ) = match rec {
            Record::Entity(e) => (
                "entity",
                *e.entity_id.as_bytes(),
                Some(e.type_id.0),
                Some(e.tx_id_assert.0),
                supersede_opt(e.tx_id_supersede),
            ),
            Record::HyperEdge(h) => (
                "hyperedge",
                *h.hyperedge_id.as_bytes(),
                Some(h.type_id.0),
                Some(h.tx_id_assert.0),
                supersede_opt(h.tx_id_supersede),
            ),
            Record::Tombstone(t) => (
                "tombstone",
                *t.target_id.as_bytes(),
                None,
                None,
                Some(t.tx_id_supersede.0),
            ),
            _ => unreachable!("filtered above"),
        };

        kind_b.append_value(kind_str);
        primary_b
            .append_value(&primary_bytes)
            .map_err(ArrowError::Arrow)?;
        match type_id_opt {
            Some(t) => type_b.append_value(t),
            None => type_b.append_null(),
        }
        match tx_assert_opt {
            Some(t) => tx_assert_b.append_value(t),
            None => tx_assert_b.append_null(),
        }
        match tx_super_opt {
            Some(t) => tx_super_b.append_value(t),
            None => tx_super_b.append_null(),
        }

        // Roles — only for hyperedges.
        if let Record::HyperEdge(h) = rec {
            let struct_builder = roles_b.values();
            for (role_id, entity_id) in &h.roles {
                struct_builder
                    .field_builder::<UInt32Builder>(0)
                    .expect("role_id builder slot")
                    .append_value(role_id.0);
                struct_builder
                    .field_builder::<FixedSizeBinaryBuilder>(1)
                    .expect("entity_id builder slot")
                    .append_value(entity_id.as_bytes())
                    .map_err(ArrowError::Arrow)?;
                struct_builder.append(true);
            }
            roles_b.append(true);
        } else {
            roles_b.append(true); // empty list, not null
        }

        // Property columns — every column gets either the row's value or null.
        for (key, builder, col_name) in &mut prop_builders {
            let props: Option<&Vec<(PropertyId, Value)>> = match rec {
                Record::Entity(e) if e.type_id.0 == key.type_id && key.kind == 0x01 => {
                    Some(&e.properties)
                }
                Record::HyperEdge(h) if h.type_id.0 == key.type_id && key.kind == 0x02 => {
                    Some(&h.properties)
                }
                _ => None,
            };
            let v = props.and_then(|ps| {
                ps.iter()
                    .find_map(|(p, v)| (p.0 == key.property_id).then_some(v))
            });
            match v {
                Some(v) => builder.append_value(v, col_name)?,
                None => builder.append_null(),
            }
        }
    }

    // Assemble schema + arrays.
    let mut fields: Vec<Field> = Vec::with_capacity(6 + prop_builders.len());
    fields.push(Field::new("record_kind", DataType::Utf8, false));
    fields.push(Field::new(
        "primary_id",
        DataType::FixedSizeBinary(16),
        false,
    ));
    fields.push(Field::new("type_id", DataType::UInt32, true));
    fields.push(Field::new("tx_id_assert", DataType::UInt64, true));
    fields.push(Field::new("tx_id_supersede", DataType::UInt64, true));
    fields.push(roles_field());
    for (key, builder, _) in &prop_builders {
        fields.push(Field::new(
            key.column_name(),
            builder.declared_type(),
            true,
        ));
    }
    let schema: SchemaRef = Arc::new(Schema::new(fields));

    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(6 + prop_builders.len());
    arrays.push(Arc::new(kind_b.finish()));
    arrays.push(Arc::new(primary_b.finish()));
    arrays.push(Arc::new(type_b.finish()));
    arrays.push(Arc::new(tx_assert_b.finish()));
    arrays.push(Arc::new(tx_super_b.finish()));
    arrays.push(Arc::new(roles_b.finish()));
    for (_, builder, _) in prop_builders {
        arrays.push(builder.finish(n_rows));
    }

    RecordBatch::try_new(schema, arrays).map_err(ArrowError::from)
}

fn supersede_opt(t: ndb_engine::id::TxId) -> Option<u64> {
    if t.is_active_sentinel() {
        None
    } else {
        Some(t.0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, BooleanArray, Float64Array, Int64Array, StringArray};
    use ndb_engine::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
    use ndb_engine::record::{
        EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, TombstoneRecord, TypeNameRecord,
    };
    use uuid::Uuid;

    fn ent_id(b: u8) -> EntityId {
        EntityId::from_uuid(Uuid::from_bytes([
            b, b, b, b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]))
    }

    fn entity(
        id: u8,
        type_id: u32,
        tx: u64,
        props: Vec<(u32, Value)>,
    ) -> Record {
        Record::Entity(EntityRecord {
            entity_id: ent_id(id),
            type_id: TypeId(type_id),
            tx_id_assert: TxId(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: props
                .into_iter()
                .map(|(p, v)| (PropertyId(p), v))
                .collect(),
        })
    }

    #[test]
    fn empty_input_produces_zero_row_batch() {
        let batch = records_to_batch(&[]).unwrap();
        assert_eq!(batch.num_rows(), 0);
        // Schema still has the identity columns.
        assert!(batch.schema().field_with_name("record_kind").is_ok());
    }

    #[test]
    fn entity_with_two_properties() {
        let recs = vec![entity(
            1,
            10,
            42,
            vec![
                (100, Value::String("alice".into())),
                (101, Value::I64(42)),
            ],
        )];
        let batch = records_to_batch(&recs).unwrap();
        assert_eq!(batch.num_rows(), 1);

        let kind = batch
            .column_by_name("record_kind")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(kind.value(0), "entity");

        let name_col = batch
            .column_by_name("prop:entity:10:100")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name_col.value(0), "alice");

        let age_col = batch
            .column_by_name("prop:entity:10:101")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(age_col.value(0), 42);
    }

    #[test]
    fn null_fills_missing_property_for_other_rows() {
        let recs = vec![
            entity(1, 10, 1, vec![(100, Value::String("alice".into()))]),
            entity(2, 10, 2, vec![(101, Value::I64(99))]),
        ];
        let batch = records_to_batch(&recs).unwrap();
        let name_col = batch
            .column_by_name("prop:entity:10:100")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name_col.value(0), "alice");
        assert!(name_col.is_null(1));
        let age_col = batch
            .column_by_name("prop:entity:10:101")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert!(age_col.is_null(0));
        assert_eq!(age_col.value(1), 99);
    }

    #[test]
    fn type_id_disambiguates_columns() {
        // Same property_id on two different types → two columns.
        let recs = vec![
            entity(1, 10, 1, vec![(100, Value::String("alice".into()))]),
            entity(2, 20, 2, vec![(100, Value::I64(7))]),
        ];
        let batch = records_to_batch(&recs).unwrap();
        assert!(batch.column_by_name("prop:entity:10:100").is_some());
        assert!(batch.column_by_name("prop:entity:20:100").is_some());
    }

    #[test]
    fn type_mismatch_in_same_column_rejected() {
        let recs = vec![
            entity(1, 10, 1, vec![(100, Value::String("alice".into()))]),
            entity(2, 10, 2, vec![(100, Value::I64(7))]),
        ];
        let err = records_to_batch(&recs).unwrap_err();
        assert!(
            matches!(err, ArrowError::TypeMismatch { .. }),
            "expected TypeMismatch, got {err:?}"
        );
    }

    #[test]
    fn null_value_is_compatible_with_any_column_type() {
        let recs = vec![
            entity(1, 10, 1, vec![(100, Value::String("alice".into()))]),
            entity(2, 10, 2, vec![(100, Value::Null)]),
            entity(3, 10, 3, vec![(100, Value::String("bob".into()))]),
        ];
        let batch = records_to_batch(&recs).unwrap();
        let col = batch
            .column_by_name("prop:entity:10:100")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "alice");
        assert!(col.is_null(1));
        assert_eq!(col.value(2), "bob");
    }

    #[test]
    fn hyperedge_row_with_roles() {
        let recs = vec![Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::from_uuid(Uuid::from_bytes([7; 16])),
            type_id: TypeId(50),
            tx_id_assert: TxId(1),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![
                (RoleId(1), ent_id(1)),
                (RoleId(2), ent_id(2)),
            ],
            properties: vec![(PropertyId(200), Value::Bool(true))],
        })];
        let batch = records_to_batch(&recs).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let prop = batch
            .column_by_name("prop:hyperedge:50:200")
            .unwrap()
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(prop.value(0));

        // Roles list-of-struct present.
        assert!(batch.column_by_name("roles").is_some());
    }

    #[test]
    fn tombstone_row_has_null_identity_fields() {
        let recs = vec![Record::Tombstone(TombstoneRecord {
            target_id: Uuid::from_bytes([9; 16]),
            tx_id_supersede: TxId(99),
        })];
        let batch = records_to_batch(&recs).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let kind = batch
            .column_by_name("record_kind")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(kind.value(0), "tombstone");

        let type_id = batch
            .column_by_name("type_id")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::UInt32Array>()
            .unwrap();
        assert!(type_id.is_null(0));
    }

    #[test]
    fn decimal_widens_to_float64() {
        let recs = vec![entity(
            1,
            10,
            1,
            vec![(
                100,
                Value::Decimal {
                    scale: 2,
                    mantissa: 12345,
                },
            )],
        )];
        let batch = records_to_batch(&recs).unwrap();
        let col = batch
            .column_by_name("prop:entity:10:100")
            .unwrap()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let got = col.value(0);
        assert!((got - 123.45).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn dictionary_records_excluded_from_rows_but_visible_via_build_dictionaries() {
        let recs = vec![
            Record::TypeName(TypeNameRecord {
                id: TypeId(10),
                name: "Customer".into(),
            }),
            Record::PropertyKey(PropertyKeyRecord {
                id: PropertyId(100),
                name: "name".into(),
            }),
            entity(1, 10, 1, vec![(100, Value::String("alice".into()))]),
        ];
        let batch = records_to_batch(&recs).unwrap();
        // The 2 dictionary records were filtered out of the row set.
        assert_eq!(batch.num_rows(), 1);
        let d = build_dictionaries(&recs);
        assert_eq!(d.types, vec![(10, "Customer".into())]);
        assert_eq!(d.properties, vec![(100, "name".into())]);
    }

    #[test]
    fn ipc_roundtrip_via_stream_reader() {
        let recs = vec![entity(1, 10, 1, vec![(100, Value::I64(7))])];
        let bytes = records_to_ipc_stream(&recs).unwrap();

        // Reader → batch → assert.
        let cursor = std::io::Cursor::new(bytes);
        let reader = arrow_ipc::reader::StreamReader::try_new(cursor, None).unwrap();
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);
    }

    #[test]
    fn supersede_active_renders_as_null() {
        let recs = vec![entity(1, 10, 1, vec![(100, Value::I64(7))])];
        let batch = records_to_batch(&recs).unwrap();
        let col = batch
            .column_by_name("tx_id_supersede")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::UInt64Array>()
            .unwrap();
        assert!(col.is_null(0));
    }
}
