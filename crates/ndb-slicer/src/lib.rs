//! nDB slicer — CPU projection + aggregation (§7, §17.1).
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,             // "Engine", "SSTable" used liberally
    clippy::cast_precision_loss,      // numeric aggregation routinely widens i64 → f64
    clippy::match_same_arms,          // explicit-per-variant arms kept for diff stability
    clippy::enum_glob_use,            // value_partial_cmp benefits from `use Value::*`
    clippy::type_complexity,          // boxed Fn predicate type is plenty readable
)]
//!
//! Engine retrieves; slicer computes (§7.6). The slicer consumes a stream
//! of [`Record`]s produced by `Engine::snapshot_iter` (or any other
//! `Iterator<Item = Record>`), applies projection / filter / group-by /
//! aggregate / sort / limit pipelines, and returns a tabular result that
//! a renderer can display or a wire format can serialise.
//!
//! v1 surface:
//!
//! - `select_columns(...)` — pick `(record_kind, property_id)` columns
//!   per record kind. Each column flattens into a row cell. Missing
//!   properties become `Value::Null`.
//! - `filter(predicate)` — drop records for which the predicate returns
//!   false. Predicate is `Fn(&Record) -> bool`.
//! - `group_by(columns)` — bucket rows by the values of the named
//!   columns. Each bucket then receives the configured aggregates.
//! - Aggregates: `Count`, `Sum`, `Avg`, `Min`, `Max`. Sum/Avg only on
//!   numeric (`I64`, `F64`, `Decimal`); Min/Max on any total-orderable
//!   value (`I64`, `F64`, `Decimal`, `String`, `Timestamp`, `Bool`).
//! - `sort(column, asc|desc)` — single-key sort over the result rows.
//! - `limit(n)` — truncate to N rows.
//!
//! What's NOT here (v2+):
//!
//! - Joins (require index lookups; relevant once query language lands).
//! - Window functions (over-partition, rank, etc.).
//! - User-defined aggregates beyond the built-in set.
//! - Float-NaN ordering policy (currently uses `partial_cmp` and treats
//!   `None` as `Less`; document if/when callers care).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use ndb_engine::id::{PropertyId, TypeId};
use ndb_engine::record::Record;
use ndb_engine::value::Value;

// ---------------------------------------------------------------------------
// Column specification
// ---------------------------------------------------------------------------

/// Which property to extract from which record kind.
#[derive(Debug, Clone)]
pub struct Column {
    /// Human-friendly header name.
    pub header: String,
    /// How to pull a value out of one record.
    pub source: ColumnSource,
}

/// Where a column's value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnSource {
    /// Property value from an Entity record. Only applies to entities of
    /// `type_id` (or any entity if `type_id` is `None`).
    EntityProperty {
        /// Restrict to entities of this type, or `None` to accept any.
        type_id: Option<TypeId>,
        /// Property id to extract.
        property: PropertyId,
    },
    /// Property value from a HyperEdge record.
    HyperEdgeProperty {
        /// Restrict to hyperedges of this type, or `None` to accept any.
        type_id: Option<TypeId>,
        /// Property id to extract.
        property: PropertyId,
    },
    /// The record's own kind (returned as `Value::String`).
    Kind,
    /// The record's primary id (entity_id / hyperedge_id / target_id /
    /// dictionary id), returned as `Value::EntityRef` for UUID-bearing
    /// kinds and `Value::I64` for dictionary kinds.
    PrimaryId,
    /// `tx_id_assert` as `Value::I64`. `0` for tombstones (which don't
    /// have an assert tx).
    TxIdAssert,
    /// `tx_id_supersede` as `Value::I64`. `i64::MAX` for active records.
    TxIdSupersede,
}

impl Column {
    /// Shorthand: a column for `entity.<prop>` with the given header.
    #[must_use]
    pub fn entity_property(header: &str, property: PropertyId) -> Self {
        Self {
            header: header.to_owned(),
            source: ColumnSource::EntityProperty {
                type_id: None,
                property,
            },
        }
    }

    /// Shorthand: scoped to a specific entity type.
    #[must_use]
    pub fn typed_entity_property(header: &str, type_id: TypeId, property: PropertyId) -> Self {
        Self {
            header: header.to_owned(),
            source: ColumnSource::EntityProperty {
                type_id: Some(type_id),
                property,
            },
        }
    }

    /// Shorthand: hyperedge property column.
    #[must_use]
    pub fn hyperedge_property(header: &str, property: PropertyId) -> Self {
        Self {
            header: header.to_owned(),
            source: ColumnSource::HyperEdgeProperty {
                type_id: None,
                property,
            },
        }
    }
}

fn extract(record: &Record, source: &ColumnSource) -> Option<Value> {
    match source {
        ColumnSource::EntityProperty { type_id, property } => match record {
            Record::Entity(e) => {
                if let Some(t) = type_id
                    && e.type_id != *t
                {
                    return None;
                }
                e.properties
                    .iter()
                    .find(|(p, _)| p == property)
                    .map(|(_, v)| v.clone())
            }
            _ => None,
        },
        ColumnSource::HyperEdgeProperty { type_id, property } => match record {
            Record::HyperEdge(h) => {
                if let Some(t) = type_id
                    && h.type_id != *t
                {
                    return None;
                }
                h.properties
                    .iter()
                    .find(|(p, _)| p == property)
                    .map(|(_, v)| v.clone())
            }
            _ => None,
        },
        ColumnSource::Kind => Some(Value::String(
            match record {
                Record::Entity(_) => "entity",
                Record::HyperEdge(_) => "hyperedge",
                Record::Tombstone(_) => "tombstone",
                Record::TypeName(_) => "type_name",
                Record::RoleName(_) => "role_name",
                Record::PropertyKey(_) => "property_key",
                Record::TxTimestamp(_) => "tx_timestamp",
                Record::RetentionPolicy(_) => "retention_policy",
            }
            .into(),
        )),
        ColumnSource::PrimaryId => Some(match record {
            Record::Entity(e) => Value::EntityRef(e.entity_id),
            Record::HyperEdge(h) => {
                Value::EntityRef(ndb_engine::EntityId::from_uuid(h.hyperedge_id.into_uuid()))
            }
            Record::Tombstone(t) => Value::EntityRef(ndb_engine::EntityId::from_uuid(t.target_id)),
            Record::TypeName(d) => Value::I64(i64::from(d.id.get())),
            Record::RoleName(d) => Value::I64(i64::from(d.id.get())),
            Record::PropertyKey(d) => Value::I64(i64::from(d.id.get())),
            // v2.0 metadata: use the relevant id as the primary key.
            Record::TxTimestamp(t) => Value::I64(i64::try_from(t.tx_id.get()).unwrap_or(i64::MAX)),
            Record::RetentionPolicy(r) => Value::I64(i64::from(r.type_id.get())),
        }),
        ColumnSource::TxIdAssert => Some(match record {
            Record::Entity(e) => {
                Value::I64(i64::try_from(e.tx_id_assert.get()).unwrap_or(i64::MAX))
            }
            Record::HyperEdge(h) => {
                Value::I64(i64::try_from(h.tx_id_assert.get()).unwrap_or(i64::MAX))
            }
            _ => Value::I64(0),
        }),
        ColumnSource::TxIdSupersede => {
            let tx = match record {
                Record::Entity(e) => e.tx_id_supersede,
                Record::HyperEdge(h) => h.tx_id_supersede,
                Record::Tombstone(t) => t.tx_id_supersede,
                _ => ndb_engine::TxId::new(0),
            };
            Some(Value::I64(i64::try_from(tx.get()).unwrap_or(i64::MAX)))
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// Aggregate function applied to a column inside a `group_by` bucket.
///
/// `Percentile` carries an `f64` fraction — `Aggregate::Percentile { p: 0.95 }`
/// for p95 — so the variant can't derive `Eq`. The remaining variants are
/// scalars; PartialEq is enough for the tests that compare aggregate
/// shapes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Aggregate {
    /// Count of non-null values.
    Count,
    /// Sum (only numeric).
    Sum,
    /// Average (only numeric).
    Avg,
    /// Minimum (any total-orderable type).
    Min,
    /// Maximum (any total-orderable type).
    Max,
    /// Percentile at fraction `p ∈ (0.0, 1.0]`. R-7 linear interpolation
    /// between adjacent samples (NumPy / pandas default). Coerces every
    /// `Value::I64 / F64 / Timestamp` to f64 for the calculation;
    /// non-numeric values reduce the per-group count.
    ///
    /// Memory cost is O(group_size × 8 bytes) — the implementation
    /// collects every value into a `Vec<f64>` and sorts. Streaming
    /// estimators (t-digest, GK) are a v3 extension if real workloads
    /// OOM on the naive variant.
    Percentile {
        /// Fraction in `(0.0, 1.0]`. Out-of-range values return `Null`.
        p: f64,
    },
}

impl Aggregate {
    /// Convenience: 50th percentile.
    pub const P50: Self = Self::Percentile { p: 0.50 };
    /// Convenience: 95th percentile.
    pub const P95: Self = Self::Percentile { p: 0.95 };
    /// Convenience: 99th percentile.
    pub const P99: Self = Self::Percentile { p: 0.99 };
}

/// An aggregate to compute over a column.
#[derive(Debug, Clone)]
pub struct AggSpec {
    /// Display header for the resulting column.
    pub header: String,
    /// Column index (0-based, indexing the input columns).
    pub column: usize,
    /// Which aggregate to compute.
    pub agg: Aggregate,
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Slicer pipeline. Build with `Pipeline::new` and call `run`.
#[derive(Default)]
pub struct Pipeline {
    columns: Vec<Column>,
    filter: Option<Box<dyn Fn(&Record) -> bool + Send + Sync>>,
    group_by: Vec<usize>,
    aggregates: Vec<AggSpec>,
    sort_column: Option<usize>,
    sort_asc: bool,
    limit: Option<usize>,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("columns", &self.columns)
            .field("filter", &self.filter.as_ref().map(|_| "<fn>"))
            .field("group_by", &self.group_by)
            .field("aggregates", &self.aggregates)
            .field("sort_column", &self.sort_column)
            .field("sort_asc", &self.sort_asc)
            .field("limit", &self.limit)
            .finish()
    }
}

impl Pipeline {
    /// Empty pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a column to the select list.
    #[must_use]
    pub fn select(mut self, c: Column) -> Self {
        self.columns.push(c);
        self
    }

    /// Set a record-level filter (only records passing the predicate are
    /// considered).
    #[must_use]
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Record) -> bool + Send + Sync + 'static,
    {
        self.filter = Some(Box::new(f));
        self
    }

    /// Group result rows by the listed column indexes (0-based into
    /// `columns`).
    #[must_use]
    pub fn group_by(mut self, columns: impl IntoIterator<Item = usize>) -> Self {
        self.group_by = columns.into_iter().collect();
        self
    }

    /// Add an aggregate column (only meaningful when `group_by` is set,
    /// though `count` works on the whole stream too — that's "global
    /// group" with empty `group_by`).
    #[must_use]
    pub fn aggregate(mut self, spec: AggSpec) -> Self {
        self.aggregates.push(spec);
        self
    }

    /// Sort by one column, ascending.
    #[must_use]
    pub fn sort_asc(mut self, column: usize) -> Self {
        self.sort_column = Some(column);
        self.sort_asc = true;
        self
    }

    /// Sort by one column, descending.
    #[must_use]
    pub fn sort_desc(mut self, column: usize) -> Self {
        self.sort_column = Some(column);
        self.sort_asc = false;
        self
    }

    /// Limit result rows.
    #[must_use]
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Run the pipeline over an iterator of records.
    pub fn run<I>(&self, records: I) -> Table
    where
        I: IntoIterator<Item = Record>,
    {
        // 1. Filter + project → Vec<Row>
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for rec in records {
            if let Some(f) = &self.filter
                && !f(&rec)
            {
                continue;
            }
            let row: Vec<Value> = self
                .columns
                .iter()
                .map(|c| extract(&rec, &c.source).unwrap_or(Value::Null))
                .collect();
            rows.push(row);
        }

        // 2. Group + aggregate (if requested).
        let (headers, mut rows) = if !self.group_by.is_empty() || !self.aggregates.is_empty() {
            self.apply_group_and_agg(rows)
        } else {
            let headers: Vec<String> = self.columns.iter().map(|c| c.header.clone()).collect();
            (headers, rows)
        };

        // 3. Sort.
        if let Some(col) = self.sort_column {
            let asc = self.sort_asc;
            rows.sort_by(|a, b| {
                let ord = value_partial_cmp(&a[col], &b[col]).unwrap_or(Ordering::Equal);
                if asc { ord } else { ord.reverse() }
            });
        }

        // 4. Limit.
        if let Some(n) = self.limit
            && rows.len() > n
        {
            rows.truncate(n);
        }

        Table { headers, rows }
    }

    fn apply_group_and_agg(&self, rows: Vec<Vec<Value>>) -> (Vec<String>, Vec<Vec<Value>>) {
        // Bucket rows by the group-by columns. Use canonical bytes-key for
        // each row's group tuple so floats with same bit-pattern dedupe.
        let mut buckets: BTreeMap<Vec<u8>, (Vec<Value>, Vec<Vec<Value>>)> = BTreeMap::new();
        for row in rows {
            let key: Vec<u8> = self
                .group_by
                .iter()
                .flat_map(|&i| value_key_bytes(&row[i]))
                .collect();
            // Track the actual group-by values for the result header.
            let group_vals: Vec<Value> = self.group_by.iter().map(|&i| row[i].clone()).collect();
            let entry = buckets
                .entry(key)
                .or_insert_with(|| (group_vals, Vec::new()));
            entry.1.push(row);
        }

        // Build headers: group-by column headers first, then aggregate
        // headers.
        let mut headers: Vec<String> = Vec::new();
        for &i in &self.group_by {
            headers.push(self.columns[i].header.clone());
        }
        for agg in &self.aggregates {
            headers.push(agg.header.clone());
        }

        // For each bucket, emit one result row.
        let mut out: Vec<Vec<Value>> = Vec::new();
        for (_key, (group_vals, group_rows)) in buckets {
            let mut row: Vec<Value> = group_vals;
            for agg in &self.aggregates {
                row.push(compute_aggregate(agg.agg, agg.column, &group_rows));
            }
            out.push(row);
        }
        (headers, out)
    }
}

// ---------------------------------------------------------------------------
// Aggregate computation
// ---------------------------------------------------------------------------

fn compute_aggregate(agg: Aggregate, col: usize, rows: &[Vec<Value>]) -> Value {
    match agg {
        Aggregate::Count => {
            let n = rows
                .iter()
                .filter(|r| !matches!(r[col], Value::Null))
                .count();
            Value::I64(i64::try_from(n).unwrap_or(i64::MAX))
        }
        Aggregate::Sum => {
            // f64 if any cell is f64, decimal if all decimal w/ same scale,
            // else i64. Mixed numeric types fall back to f64.
            let mut acc_i = 0i64;
            let mut acc_f = 0.0f64;
            let mut as_float = false;
            let mut any = false;
            for r in rows {
                match r[col] {
                    Value::I64(n) => {
                        any = true;
                        if as_float {
                            acc_f += n as f64;
                        } else {
                            acc_i = acc_i.saturating_add(n);
                        }
                    }
                    Value::F64(f) => {
                        any = true;
                        if !as_float {
                            acc_f = acc_i as f64;
                            as_float = true;
                        }
                        acc_f += f;
                    }
                    _ => {}
                }
            }
            if !any {
                Value::Null
            } else if as_float {
                Value::F64(acc_f)
            } else {
                Value::I64(acc_i)
            }
        }
        Aggregate::Avg => {
            let mut n = 0i64;
            let mut acc = 0.0f64;
            for r in rows {
                match r[col] {
                    Value::I64(v) => {
                        acc += v as f64;
                        n += 1;
                    }
                    Value::F64(v) => {
                        acc += v;
                        n += 1;
                    }
                    _ => {}
                }
            }
            if n == 0 {
                Value::Null
            } else {
                #[allow(clippy::cast_precision_loss)]
                Value::F64(acc / n as f64)
            }
        }
        Aggregate::Min => extremum(rows, col, true),
        Aggregate::Max => extremum(rows, col, false),
        Aggregate::Percentile { p } => percentile(rows, col, p),
    }
}

/// R-7 (linear) percentile over the numeric values in `col`. Matches
/// NumPy / pandas default. Returns `Value::Null` for an empty group or
/// out-of-range `p`.
fn percentile(rows: &[Vec<Value>], col: usize, p: f64) -> Value {
    if !(p > 0.0 && p <= 1.0) {
        return Value::Null;
    }
    let mut xs: Vec<f64> = Vec::with_capacity(rows.len());
    for r in rows {
        match r[col] {
            Value::I64(n) => {
                #[allow(clippy::cast_precision_loss)]
                xs.push(n as f64);
            }
            Value::F64(f) => xs.push(f),
            Value::Timestamp(t) => {
                #[allow(clippy::cast_precision_loss)]
                xs.push(t as f64);
            }
            _ => {}
        }
    }
    if xs.is_empty() {
        return Value::Null;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n == 1 {
        return Value::F64(xs[0]);
    }
    // R-7: index h = p * (n - 1); lerp between floor(h) and ceil(h).
    #[allow(clippy::cast_precision_loss)]
    let h = p * (n as f64 - 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = h.floor() as usize;
    let hi = lo + 1;
    if hi >= n {
        return Value::F64(xs[n - 1]);
    }
    let frac = h - h.floor();
    Value::F64(xs[lo] + (xs[hi] - xs[lo]) * frac)
}

fn extremum(rows: &[Vec<Value>], col: usize, want_min: bool) -> Value {
    let mut best: Option<Value> = None;
    for r in rows {
        let v = &r[col];
        if matches!(v, Value::Null) {
            continue;
        }
        match &best {
            None => best = Some(v.clone()),
            Some(b) => {
                let ord = value_partial_cmp(v, b).unwrap_or(Ordering::Equal);
                let take = if want_min {
                    ord == Ordering::Less
                } else {
                    ord == Ordering::Greater
                };
                if take {
                    best = Some(v.clone());
                }
            }
        }
    }
    best.unwrap_or(Value::Null)
}

/// Total-orderish comparison over `Value`. Returns `None` only for
/// incomparable type pairs.
#[must_use]
pub fn value_partial_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Null, Null) => Some(Ordering::Equal),
        (Null, _) => Some(Ordering::Less),
        (_, Null) => Some(Ordering::Greater),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (I64(x), I64(y)) => Some(x.cmp(y)),
        (F64(x), F64(y)) => x.partial_cmp(y),
        (I64(x), F64(y)) => (*x as f64).partial_cmp(y),
        (F64(x), I64(y)) => x.partial_cmp(&(*y as f64)),
        (String(x), String(y)) => Some(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp(y)),
        (
            Decimal {
                scale: sa,
                mantissa: ma,
            },
            Decimal {
                scale: sb,
                mantissa: mb,
            },
        ) if sa == sb => Some(ma.cmp(mb)),
        (EntityRef(a), EntityRef(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
        _ => None,
    }
}

fn value_key_bytes(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Null => out.push(0x00),
        Value::Bool(b) => {
            out.push(0x02);
            out.push(u8::from(*b));
        }
        Value::I64(n) => {
            out.push(0x03);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::F64(f) => {
            out.push(0x04);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        Value::String(s) => {
            out.push(0x05);
            out.extend_from_slice(s.as_bytes());
            out.push(0); // separator
        }
        Value::Timestamp(t) => {
            out.push(0x07);
            out.extend_from_slice(&t.to_be_bytes());
        }
        Value::EntityRef(id) => {
            out.push(0x08);
            out.extend_from_slice(id.as_bytes());
        }
        Value::Decimal { scale, mantissa } => {
            out.push(0x09);
            out.push(*scale);
            out.extend_from_slice(&mantissa.to_be_bytes());
        }
        _ => {
            // Bytes / Vector / Extension: skip for key purposes; group_by
            // on these is rarely meaningful and would bloat keys.
            out.push(0xff);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Tabular result of running a pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Column headers (display strings).
    pub headers: Vec<String>,
    /// Result rows; each row has `headers.len()` cells.
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True iff zero rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::{
        EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId, PropertyId, RoleId, TxId, TypeId,
        Value,
    };

    fn entity(eid: EntityId, type_id: u32, props: Vec<(u32, Value)>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            properties: props
                .into_iter()
                .map(|(p, v)| (PropertyId::new(p), v))
                .collect(),
        })
    }

    fn _hyper_one(type_id: u32) -> Record {
        Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            properties: vec![],
        })
    }

    #[test]
    fn project_and_limit() {
        let records = vec![
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("alice".into()))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("bob".into()))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("carol".into()))],
            ),
        ];
        let p = Pipeline::new()
            .select(Column::entity_property("name", PropertyId::new(10)))
            .limit(2);
        let t = p.run(records);
        assert_eq!(t.headers, vec!["name".to_string()]);
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn filter_keeps_only_matches() {
        let records = vec![
            entity(EntityId::now_v7(), 1, vec![(10, Value::I64(5))]),
            entity(EntityId::now_v7(), 1, vec![(10, Value::I64(15))]),
            entity(EntityId::now_v7(), 1, vec![(10, Value::I64(25))]),
        ];
        let p = Pipeline::new()
            .select(Column::entity_property("age", PropertyId::new(10)))
            .filter(|r| match r {
                Record::Entity(e) => match &e.properties[0].1 {
                    Value::I64(n) => *n > 10,
                    _ => false,
                },
                _ => false,
            });
        let t = p.run(records);
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn group_by_and_count() {
        let records = vec![
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("red".into()))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("red".into()))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("blue".into()))],
            ),
        ];
        let p = Pipeline::new()
            .select(Column::entity_property("color", PropertyId::new(10)))
            .group_by([0])
            .aggregate(AggSpec {
                header: "n".into(),
                column: 0,
                agg: Aggregate::Count,
            });
        let t = p.run(records);
        assert_eq!(t.headers, vec!["color".to_string(), "n".to_string()]);
        assert_eq!(t.rows.len(), 2);
        for row in &t.rows {
            let count = match &row[1] {
                Value::I64(n) => *n,
                _ => panic!("count must be I64"),
            };
            let expected = if matches!(&row[0], Value::String(s) if s == "red") {
                2
            } else {
                1
            };
            assert_eq!(count, expected);
        }
    }

    #[test]
    fn group_by_and_sum() {
        let records = vec![
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("A".into())), (11, Value::I64(100))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("A".into())), (11, Value::I64(50))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("B".into())), (11, Value::I64(7))],
            ),
        ];
        let p = Pipeline::new()
            .select(Column::entity_property("class", PropertyId::new(10)))
            .select(Column::entity_property("amount", PropertyId::new(11)))
            .group_by([0])
            .aggregate(AggSpec {
                header: "total".into(),
                column: 1,
                agg: Aggregate::Sum,
            });
        let t = p.run(records);
        // Two groups: A=150, B=7. Order alphabetical by group key.
        assert_eq!(t.rows.len(), 2);
        let a_row = t
            .rows
            .iter()
            .find(|r| matches!(&r[0], Value::String(s) if s == "A"))
            .unwrap();
        assert_eq!(a_row[1], Value::I64(150));
        let b_row = t
            .rows
            .iter()
            .find(|r| matches!(&r[0], Value::String(s) if s == "B"))
            .unwrap();
        assert_eq!(b_row[1], Value::I64(7));
    }

    #[test]
    fn sort_asc_and_desc() {
        let records = vec![
            entity(EntityId::now_v7(), 1, vec![(10, Value::I64(30))]),
            entity(EntityId::now_v7(), 1, vec![(10, Value::I64(10))]),
            entity(EntityId::now_v7(), 1, vec![(10, Value::I64(20))]),
        ];
        let p = Pipeline::new()
            .select(Column::entity_property("n", PropertyId::new(10)))
            .sort_asc(0);
        let t = p.run(records.clone());
        assert_eq!(
            t.rows
                .iter()
                .map(|r| match &r[0] {
                    Value::I64(n) => *n,
                    _ => 0,
                })
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );

        let p = Pipeline::new()
            .select(Column::entity_property("n", PropertyId::new(10)))
            .sort_desc(0);
        let t = p.run(records);
        assert_eq!(
            t.rows
                .iter()
                .map(|r| match &r[0] {
                    Value::I64(n) => *n,
                    _ => 0,
                })
                .collect::<Vec<_>>(),
            vec![30, 20, 10]
        );
    }

    #[test]
    fn min_max_avg() {
        let records = vec![
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("g".into())), (11, Value::I64(10))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("g".into())), (11, Value::I64(20))],
            ),
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("g".into())), (11, Value::I64(30))],
            ),
        ];
        let p = Pipeline::new()
            .select(Column::entity_property("g", PropertyId::new(10)))
            .select(Column::entity_property("v", PropertyId::new(11)))
            .group_by([0])
            .aggregate(AggSpec {
                header: "mn".into(),
                column: 1,
                agg: Aggregate::Min,
            })
            .aggregate(AggSpec {
                header: "mx".into(),
                column: 1,
                agg: Aggregate::Max,
            })
            .aggregate(AggSpec {
                header: "av".into(),
                column: 1,
                agg: Aggregate::Avg,
            });
        let t = p.run(records);
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0][1], Value::I64(10));
        assert_eq!(t.rows[0][2], Value::I64(30));
        assert_eq!(t.rows[0][3], Value::F64(20.0));
    }

    #[test]
    fn missing_property_yields_null() {
        let records = vec![
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("present".into()))],
            ),
            entity(EntityId::now_v7(), 1, vec![]), // missing prop 10
        ];
        let p = Pipeline::new().select(Column::entity_property("name", PropertyId::new(10)));
        let t = p.run(records);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1][0], Value::Null);
    }

    #[test]
    fn type_id_restriction_filters() {
        let records = vec![
            entity(
                EntityId::now_v7(),
                1,
                vec![(10, Value::String("ok".into()))],
            ),
            entity(
                EntityId::now_v7(),
                2,
                vec![(10, Value::String("nope".into()))],
            ),
        ];
        let p = Pipeline::new().select(Column::typed_entity_property(
            "name",
            TypeId::new(1),
            PropertyId::new(10),
        ));
        let t = p.run(records);
        // Both records appear, but only the typed-1 one has the property
        // extracted; the other contributes null.
        assert_eq!(t.rows.len(), 2);
        let strs: Vec<&Value> = t.rows.iter().map(|r| &r[0]).collect();
        assert!(
            strs.iter()
                .any(|v| matches!(v, Value::String(s) if s == "ok"))
        );
        assert!(strs.iter().any(|v| matches!(v, Value::Null)));
    }

    // ---------------------------------------------------------------------
    // §2.3 Percentile aggregates — R-7 linear interpolation
    // ---------------------------------------------------------------------

    fn percentile_value(rows: &[Vec<Value>], p: f64) -> Value {
        compute_aggregate(Aggregate::Percentile { p }, 0, rows)
    }

    #[test]
    fn percentile_r7_canonical_values_match_numpy() {
        // numpy.percentile([1,2,3,4,5], 50) = 3.0; 95 = 4.8; 99 = 4.96
        let rows: Vec<Vec<Value>> = (1..=5).map(|n| vec![Value::I64(n)]).collect();
        assert!(matches!(
            percentile_value(&rows, 0.50),
            Value::F64(v) if (v - 3.0).abs() < 1e-9
        ));
        assert!(matches!(
            percentile_value(&rows, 0.95),
            Value::F64(v) if (v - 4.8).abs() < 1e-9
        ));
        assert!(matches!(
            percentile_value(&rows, 0.99),
            Value::F64(v) if (v - 4.96).abs() < 1e-9
        ));
    }

    #[test]
    fn percentile_empty_group_is_null() {
        let rows: Vec<Vec<Value>> = vec![vec![Value::Null], vec![Value::Null]];
        assert!(matches!(percentile_value(&rows, 0.5), Value::Null));
    }

    #[test]
    fn percentile_mixed_numeric_types_coerce() {
        let rows: Vec<Vec<Value>> = vec![
            vec![Value::I64(10)],
            vec![Value::F64(20.0)],
            vec![Value::Timestamp(30)],
        ];
        assert!(matches!(
            percentile_value(&rows, 0.50),
            Value::F64(v) if (v - 20.0).abs() < 1e-9
        ));
    }

    #[test]
    fn percentile_out_of_range_returns_null() {
        let rows: Vec<Vec<Value>> = vec![vec![Value::I64(1)], vec![Value::I64(2)]];
        assert!(matches!(percentile_value(&rows, 0.0), Value::Null));
        assert!(matches!(percentile_value(&rows, 1.5), Value::Null));
        assert!(matches!(percentile_value(&rows, -0.1), Value::Null));
    }

    #[test]
    fn percentile_single_value_returns_that_value() {
        let rows: Vec<Vec<Value>> = vec![vec![Value::I64(42)]];
        assert!(matches!(
            percentile_value(&rows, 0.95),
            Value::F64(v) if (v - 42.0).abs() < 1e-9
        ));
    }

    #[test]
    fn percentile_p50_p95_p99_constants_match_named_values() {
        // P50/P95/P99 constants should produce identical results to the
        // explicit Percentile { p: ... } variants.
        let rows: Vec<Vec<Value>> = (1..=100).map(|n| vec![Value::I64(n)]).collect();
        let p50 = compute_aggregate(Aggregate::P50, 0, &rows);
        let p95 = compute_aggregate(Aggregate::P95, 0, &rows);
        let p99 = compute_aggregate(Aggregate::P99, 0, &rows);
        assert_eq!(p50, compute_aggregate(Aggregate::Percentile { p: 0.50 }, 0, &rows));
        assert_eq!(p95, compute_aggregate(Aggregate::Percentile { p: 0.95 }, 0, &rows));
        assert_eq!(p99, compute_aggregate(Aggregate::Percentile { p: 0.99 }, 0, &rows));
        // Sanity: p50 ≈ 50.5 (R-7 on 1..=100)
        assert!(matches!(p50, Value::F64(v) if (v - 50.5).abs() < 1e-9));
    }
}
