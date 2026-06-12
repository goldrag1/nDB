//! Columnar batch substrate (step 3 / GPU step) — the contiguous,
//! type-homogeneous buffer that both the CPU SIMD reductions and the
//! optional GPU kernel consume.
//!
//! [`Pipeline::run_columnar`](crate::Pipeline::run_columnar) fuses
//! projection and grouping and never needs a standalone buffer. The GPU,
//! however, wants exactly one flat array it can DMA across PCIe in a
//! single transfer (see the SSD→RAM→VRAM discussion in the design
//! notes). [`F64Column`] is that array: one numeric column extracted
//! from a record stream, plus a validity mask, laid out for a streaming
//! reduction.
//!
//! The reductions here are written as plain chunked loops over `&[f64]`
//! with no `unsafe` (the workspace forbids it). With `lto = "thin"` and
//! `codegen-units = 1` the release build auto-vectorises them to SIMD;
//! the GPU kernel in [`crate::gpu`] is the same math on a different
//! processor, and [`F64Column::sum`] is its CPU reference + fallback.

use ndb_engine::record::Record;
use ndb_engine::value::Value;

use crate::{extract, ColumnSource};

/// A single numeric column materialised as a contiguous `f64` buffer.
///
/// `data[i]` is meaningful only when `valid[i]` is true; invalid slots
/// (nulls / non-numeric cells) hold `0.0` so a sum over the raw `data`
/// slice is still correct and branch-free.
#[derive(Debug, Clone, Default)]
pub struct F64Column {
    /// Values, with nulls coerced to `0.0`.
    pub data: Vec<f64>,
    /// Per-element validity (false = null / non-numeric).
    pub valid: Vec<bool>,
}

impl F64Column {
    /// Materialise one numeric column from a record stream by reading
    /// `source` from each record. `I64`/`F64`/`Timestamp` cells coerce to
    /// `f64`; everything else (and missing properties) is recorded as a
    /// null slot. The result is the buffer a GPU kernel uploads verbatim.
    pub fn build<I>(source: &ColumnSource, records: I) -> Self
    where
        I: IntoIterator<Item = Record>,
    {
        let mut data = Vec::new();
        let mut valid = Vec::new();
        for rec in records {
            match extract(&rec, source) {
                Some(Value::I64(n)) => {
                    data.push(n as f64);
                    valid.push(true);
                }
                Some(Value::F64(f)) => {
                    data.push(f);
                    valid.push(true);
                }
                Some(Value::Timestamp(t)) => {
                    data.push(t as f64);
                    valid.push(true);
                }
                _ => {
                    data.push(0.0);
                    valid.push(false);
                }
            }
        }
        Self { data, valid }
    }

    /// Number of elements (valid + null).
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True iff the column holds no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Count of non-null elements.
    #[must_use]
    pub fn count(&self) -> usize {
        self.valid.iter().filter(|&&v| v).count()
    }

    /// Sum of the valid elements. Because null slots are stored as `0.0`,
    /// this is a straight reduction over `data` — no per-element branch,
    /// which is what lets the compiler vectorise it (and what makes the
    /// GPU port trivial). Uses 8 independent partial accumulators so the
    /// vectoriser can issue parallel FP adds instead of one dependent
    /// chain.
    #[must_use]
    pub fn sum(&self) -> f64 {
        sum_slice(&self.data)
    }

    /// Mean of the valid elements, or `None` when there are none.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        let n = self.count();
        if n == 0 {
            None
        } else {
            Some(self.sum() / n as f64)
        }
    }
}

/// SIMD-friendly sum: eight lanes of independent partial sums collapsed
/// at the end. The lane split breaks the loop-carried dependency so the
/// auto-vectoriser (and the GPU's parallel reduction) can run the adds
/// concurrently.
#[must_use]
pub fn sum_slice(data: &[f64]) -> f64 {
    const LANES: usize = 8;
    let mut acc = [0.0f64; LANES];
    let chunks = data.chunks_exact(LANES);
    let tail = chunks.remainder();
    for c in chunks {
        for l in 0..LANES {
            acc[l] += c[l];
        }
    }
    let mut total = 0.0;
    for a in acc {
        total += a;
    }
    for &x in tail {
        total += x;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::id::PropertyId;
    use ndb_engine::{EntityId, EntityRecord, TxId, TypeId};

    fn ent(props: Vec<(u32, Value)>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::ACTIVE,
            properties: props.into_iter().map(|(p, v)| (PropertyId::new(p), v)).collect(),
        })
    }

    fn src() -> ColumnSource {
        ColumnSource::EntityProperty { type_id: None, property: PropertyId::new(7) }
    }

    #[test]
    fn build_coerces_and_masks() {
        let recs = vec![
            ent(vec![(7, Value::I64(10))]),
            ent(vec![(7, Value::F64(2.5))]),
            ent(vec![(8, Value::I64(99))]), // missing prop 7 → null slot
            ent(vec![(7, Value::String("x".into()))]), // non-numeric → null slot
        ];
        let col = F64Column::build(&src(), recs);
        assert_eq!(col.len(), 4);
        assert_eq!(col.count(), 2);
        assert!((col.sum() - 12.5).abs() < 1e-9);
        assert!((col.mean().unwrap() - 6.25).abs() < 1e-9);
    }

    #[test]
    fn sum_slice_matches_naive_across_sizes() {
        // Cover sizes around the lane boundary so the tail handling is
        // exercised (7, 8, 9, ... 33 elements).
        for n in 0..40usize {
            let data: Vec<f64> = (0..n).map(|i| (i as f64) * 1.5 - 3.0).collect();
            let naive: f64 = data.iter().sum();
            assert!((sum_slice(&data) - naive).abs() < 1e-6, "n={n}");
        }
    }

    #[test]
    fn empty_column_has_no_mean() {
        let col = F64Column::default();
        assert!(col.is_empty());
        assert_eq!(col.count(), 0);
        assert_eq!(col.sum(), 0.0);
        assert!(col.mean().is_none());
    }
}
