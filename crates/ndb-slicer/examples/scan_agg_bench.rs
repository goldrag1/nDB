//! Baseline benchmark for the SSD → records → aggregation path.
//!
//! Stage-by-stage timing so each cost in the scan pipeline is visible
//! in isolation:
//!
//!   1. `drain`        — `snapshot_iter_streaming` only: SSTable mmap →
//!                       `Record::decode` → k-way merge. No slicer work.
//!   2. `project`      — drain + slicer projection (extract 2 columns),
//!                       no group/aggregate.
//!   3. `group_sum`    — drain + project + group_by(region) + Sum(amount).
//!   4. `filter_agg`   — drain + record filter (1-in-10 pass) + group/sum.
//!
//! Stage N includes the cost of stages before it, so (stage N − stage
//! N−1) isolates the marginal cost of each step.
//!
//! Run with:
//!     cargo run --release -p ndb-slicer --example scan_agg_bench
//!     cargo run --release -p ndb-slicer --example scan_agg_bench -- 500000 5
//!                                                       (entities, iters)
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use ndb_engine::record::Record;
use ndb_engine::{Engine, EntityId, EntityRecord, PropertyId, TxId, TypeId, Value};
use ndb_slicer::{AggSpec, Aggregate, Column, Pipeline};
use std::time::Instant;

const TYPE_ORDER: u32 = 300;
const PROP_REGION: u32 = 40;
const PROP_AMOUNT: u32 = 41;
const PROP_NOTE: u32 = 42;
const N_REGIONS: usize = 64;
const TX_BATCH: usize = 1_000;
const FLUSH_EVERY: usize = 50_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n_entities: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let dir = std::env::temp_dir().join(format!(
        "ndb-scan-agg-bench-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_micros()
    ));
    std::fs::create_dir_all(&dir)?;
    let mut engine = Engine::create(&dir)?;

    let seed_start = Instant::now();
    seed(&mut engine, n_entities)?;
    engine.flush()?;
    eprintln!(
        "seeded {} order entities in {:.1} s ({} sstables)",
        n_entities,
        seed_start.elapsed().as_secs_f64(),
        engine.sstable_count(),
    );

    let snap = TxId::ACTIVE;

    // Warmup: touch every page once so mmap faults don't skew run 1.
    let warm = engine
        .snapshot_iter_streaming(snap)
        .filter_map(Result::ok)
        .count();
    eprintln!("warmup drain: {warm} records\n");

    bench("drain", iters, n_entities, || {
        engine
            .snapshot_iter_streaming(snap)
            .filter_map(Result::ok)
            .count()
    });

    let project = Pipeline::new()
        .select(Column::typed_entity_property(
            "region",
            TypeId::new(TYPE_ORDER),
            PropertyId::new(PROP_REGION),
        ))
        .select(Column::typed_entity_property(
            "amount",
            TypeId::new(TYPE_ORDER),
            PropertyId::new(PROP_AMOUNT),
        ));
    bench("project", iters, n_entities, || {
        run_pipeline(&engine, snap, &project)
    });

    let group_sum = Pipeline::new()
        .select(Column::typed_entity_property(
            "region",
            TypeId::new(TYPE_ORDER),
            PropertyId::new(PROP_REGION),
        ))
        .select(Column::typed_entity_property(
            "amount",
            TypeId::new(TYPE_ORDER),
            PropertyId::new(PROP_AMOUNT),
        ))
        .group_by([0])
        .aggregate(AggSpec {
            header: "total".into(),
            column: 1,
            agg: Aggregate::Sum,
        })
        .aggregate(AggSpec {
            header: "avg".into(),
            column: 1,
            agg: Aggregate::Avg,
        });
    bench("group_sum", iters, n_entities, || {
        run_pipeline(&engine, snap, &group_sum)
    });
    bench("group_sum_col", iters, n_entities, || {
        run_pipeline_columnar(&engine, snap, &group_sum)
    });

    let filter_agg = Pipeline::new()
        .select(Column::typed_entity_property(
            "region",
            TypeId::new(TYPE_ORDER),
            PropertyId::new(PROP_REGION),
        ))
        .select(Column::typed_entity_property(
            "amount",
            TypeId::new(TYPE_ORDER),
            PropertyId::new(PROP_AMOUNT),
        ))
        .filter(|rec| match rec {
            Record::Entity(e) => e
                .properties
                .iter()
                .any(|(p, v)| p.get() == PROP_AMOUNT && matches!(v, Value::I64(n) if n % 10 == 0)),
            _ => false,
        })
        .group_by([0])
        .aggregate(AggSpec {
            header: "total".into(),
            column: 1,
            agg: Aggregate::Sum,
        });
    bench("filter_agg", iters, n_entities, || {
        run_pipeline(&engine, snap, &filter_agg)
    });

    // Global numeric reduction over a single column — the shape the GPU
    // kernel targets. CPU SIMD path always runs; GPU path runs only with
    // `--features gpu` AND a real adapter, otherwise it reports a skip.
    {
        use ndb_slicer::F64Column;
        let amount_src = ndb_slicer::ColumnSource::EntityProperty {
            type_id: Some(TypeId::new(TYPE_ORDER)),
            property: PropertyId::new(PROP_AMOUNT),
        };
        let col = F64Column::build(
            &amount_src,
            engine.snapshot_iter_streaming(snap).filter_map(Result::ok),
        );
        eprintln!("\nglobal sum over {} values:", col.len());
        bench("sum_cpu_simd", iters, n_entities, || {
            std::hint::black_box(col.sum());
            col.len()
        });
        #[cfg(feature = "gpu")]
        {
            if ndb_slicer::gpu::is_available() {
                bench("sum_gpu", iters, n_entities, || {
                    std::hint::black_box(ndb_slicer::gpu::gpu_sum(&col));
                    col.len()
                });
            } else {
                eprintln!("sum_gpu     : no GPU adapter present — CPU fallback is used");
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn run_pipeline(engine: &Engine, snap: TxId, p: &Pipeline) -> usize {
    let table = p.run(engine.snapshot_iter_streaming(snap).filter_map(Result::ok));
    table.len()
}

fn run_pipeline_columnar(engine: &Engine, snap: TxId, p: &Pipeline) -> usize {
    let table = p.run_columnar(engine.snapshot_iter_streaming(snap).filter_map(Result::ok));
    table.len()
}

fn bench<F: FnMut() -> usize>(name: &str, iters: usize, n_records: usize, mut op: F) {
    let mut samples = Vec::with_capacity(iters);
    let mut out_rows = 0;
    for _ in 0..iters {
        let t = Instant::now();
        out_rows = op();
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let recs_per_s = n_records as f64 / (median / 1000.0);
    println!(
        "{name:<12} out_rows={out_rows:<8} median={median:9.1} ms   {:>10.0} records/s",
        recs_per_s
    );
}

fn seed(engine: &mut Engine, n: usize) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = engine.begin_write();
    let mut in_tx = 0usize;
    let mut since_flush = 0usize;
    for i in 0..n {
        let region = format!("REG-{:03}", i % N_REGIONS);
        tx.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(TYPE_ORDER),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(PROP_REGION), Value::String(region)),
                (
                    PropertyId::new(PROP_AMOUNT),
                    Value::I64((i as i64 * 37) % 10_000),
                ),
                (
                    PropertyId::new(PROP_NOTE),
                    Value::String(format!("order number {i} placed via web checkout")),
                ),
            ],
        });
        in_tx += 1;
        since_flush += 1;
        if in_tx >= TX_BATCH {
            tx.commit()?;
            if since_flush >= FLUSH_EVERY {
                engine.flush()?;
                since_flush = 0;
            }
            tx = engine.begin_write();
            in_tx = 0;
        }
    }
    if in_tx > 0 {
        tx.commit()?;
    }
    Ok(())
}
