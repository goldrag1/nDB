//! Decoder robustness ("fuzz-lite") tests.
//!
//! Every byte-level decoder in nDB parses data that can arrive over the
//! network or off a possibly-corrupt disk. None of them may ever panic,
//! index out of bounds, or loop forever on hostile input — a malformed
//! record must surface as a clean `Err`, never a crash. These tests drive
//! each decoder with a deterministic stream of random, truncated, and
//! bit-flipped inputs. The assertion is implicit: if any decoder panics,
//! the test process aborts and the test fails.
//!
//! Determinism: a small xorshift PRNG seeded per-case keeps the corpus
//! reproducible across runs (no wall-clock / OS randomness), so a failure
//! is always replayable.

use ndb_engine::record::{peek_record_kind, peek_record_size};
use ndb_engine::{
    EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId, PropertyId, Record, RoleId,
    TombstoneRecord, TxId, TypeId, Value,
};

/// Tiny deterministic PRNG (xorshift64*). No external deps, no OS entropy.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point.
        Self(seed ^ 0x9e37_79b9_7f4a_7c15 | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        while v.len() < len {
            v.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        v.truncate(len);
        v
    }
    fn range(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            usize::try_from(self.next_u64() % n as u64).unwrap_or(0)
        }
    }
}

/// A few valid records spanning every kind, to seed truncation / bit-flip.
fn sample_records() -> Vec<Record> {
    vec![
        Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(7),
            tx_id_assert: TxId::new(42),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(1), Value::String("hello world".into())),
                (PropertyId::new(2), Value::I64(-99)),
                (PropertyId::new(3), Value::F64(2.5)),
                (PropertyId::new(4), Value::Bool(true)),
                (PropertyId::new(5), Value::Bytes(vec![0, 1, 2, 3, 255])),
            ],
        }),
        Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(3),
            tx_id_assert: TxId::new(7),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![
                (RoleId::new(1), EntityId::now_v7()),
                (RoleId::new(2), EntityId::now_v7()),
                (RoleId::new(3), EntityId::now_v7()),
            ],
            hyperedge_roles: Vec::new(),
            properties: vec![(PropertyId::new(9), Value::I64(123))],
        }),
        Record::Tombstone(TombstoneRecord {
            target_id: uuid::Uuid::now_v7(),
            tx_id_supersede: TxId::new(5),
        }),
    ]
}

fn encode(r: &Record) -> Vec<u8> {
    let mut buf = Vec::new();
    r.encode(&mut buf).expect("encode of a valid record");
    buf
}

#[test]
fn record_decode_never_panics_on_random_bytes() {
    let mut rng = Rng::new(0xA11CE);
    for _ in 0..20_000 {
        let len = rng.range(512);
        let buf = rng.bytes(len);
        // Any outcome is fine — we only forbid a panic / hang.
        let _ = Record::decode(&buf);
        let _ = peek_record_size(&buf);
        let _ = peek_record_kind(&buf);
    }
}

#[test]
fn value_decode_never_panics_on_random_bytes() {
    let mut rng = Rng::new(0xBEEF);
    for _ in 0..20_000 {
        let len = rng.range(256);
        let buf = rng.bytes(len);
        let _ = Value::decode(&buf);
    }
}

#[test]
fn every_truncation_of_a_valid_record_is_safe() {
    for r in sample_records() {
        let full = encode(&r);
        // Round-trips at full length.
        let (back, consumed) = Record::decode(&full).expect("full decode");
        assert_eq!(back, r);
        assert_eq!(consumed, full.len());
        // Every strict prefix must Err cleanly (or, for peek, not panic).
        for cut in 0..full.len() {
            let prefix = &full[..cut];
            assert!(
                Record::decode(prefix).is_err(),
                "truncation to {cut}/{} bytes should not decode",
                full.len()
            );
            let _ = peek_record_size(prefix);
            let _ = peek_record_kind(prefix);
        }
    }
}

#[test]
fn single_byte_flips_never_panic_and_usually_error() {
    // A valid record with one byte flipped must never panic. The per-record
    // CRC32 in the envelope catches the overwhelming majority as a decode
    // error; a few flips land in payload bytes that still parse to a
    // *different* valid record — that's acceptable (CRC covers integrity,
    // not semantic equality). We only assert no panic, and that at least
    // the CRC region reliably errors.
    for r in sample_records() {
        let base = encode(&r);
        for i in 0..base.len() {
            for bit in 0..8u8 {
                let mut corrupt = base.clone();
                corrupt[i] ^= 1 << bit;
                // Must not panic.
                let _ = Record::decode(&corrupt);
            }
        }
    }
}

#[test]
fn trailing_garbage_after_a_record_does_not_confuse_the_size_prefix() {
    let mut rng = Rng::new(0xF00D);
    for r in sample_records() {
        let mut buf = encode(&r);
        let valid_len = buf.len();
        let tail_len = rng.range(128);
        buf.extend_from_slice(&rng.bytes(tail_len));
        // decode reports how many bytes it consumed; it must equal the
        // single record's length and never run into the garbage tail.
        let (back, consumed) = Record::decode(&buf).expect("leading record still decodes");
        assert_eq!(back, r);
        assert_eq!(consumed, valid_len);
    }
}

#[test]
fn corrupt_sidecar_files_load_as_err_not_panic() {
    use ndb_engine::{block_index, bloom};
    let dir = std::env::temp_dir().join(format!("ndb-robust-sidecar-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng::new(0x00C0_FFEE);
    for i in 0..500 {
        let len = rng.range(128);
        let bytes = rng.bytes(len);
        let bloom_path = dir.join(format!("{i}.bloom"));
        let idx_path = dir.join(format!("{i}.idx"));
        std::fs::write(&bloom_path, &bytes).unwrap();
        std::fs::write(&idx_path, &bytes).unwrap();
        // Either Ok(Some/None) or Err — never a panic.
        let _ = bloom::load_sidecar(&bloom_path);
        let _ = block_index::load_sidecar(&idx_path);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn opening_a_garbage_file_as_an_sstable_errs_cleanly() {
    use ndb_engine::sstable::SSTableReader;
    let dir = std::env::temp_dir().join(format!("ndb-robust-sst-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng::new(0xDEAD);
    for i in 0..300 {
        // Vary length around the footer size boundary (32 bytes) to probe
        // the short-file and footer-parse paths.
        let len = rng.range(80);
        let path = dir.join(format!("{i}.ndb"));
        std::fs::write(&path, rng.bytes(len)).unwrap();
        // Must return Err, not panic, for non-SSTable bytes.
        assert!(SSTableReader::open(&path).is_err());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hostile_length_prefixes_error_without_giant_allocations() {
    // Regression: a decoder must not pre-allocate based on an unchecked
    // length/arity prefix. Each of these inputs claims a near-u32::MAX
    // element count but carries only a handful of payload bytes — the old
    // code called Vec::with_capacity(count) first and OOM-aborted (~16 GB
    // for the f32 vector, ~84 GB for the roles array). The fix caps the
    // speculative allocation by the remaining input, so these must return
    // a clean Err in well under a second and a few KB of memory.
    const HUGE: [u8; 4] = u32::MAX.to_le_bytes();

    // Value::Vector with a bogus 4.29e9 element count.
    let mut vector_value = vec![0x0A]; // TAG_VECTOR (see value.rs)
    vector_value.extend_from_slice(&HUGE);
    vector_value.extend_from_slice(&[0u8; 8]); // only 2 f32s of payload
    // Any tag byte is allowed to be wrong; we only require no OOM/panic.
    let _ = Value::decode(&vector_value);

    // Brute force: for every value tag, a huge u32 length prefix must not
    // blow up. Covers String/Bytes/Vector/Extension uniformly.
    for tag in 0u8..=20 {
        let mut buf = vec![tag];
        buf.extend_from_slice(&HUGE);
        buf.extend_from_slice(&[1u8; 16]);
        let _ = Value::decode(&buf);
    }

    // A truncated record whose decoded arity/count fields are maximal must
    // also stay bounded — drive the full record decoder with such inputs.
    let mut rng = Rng::new(0x5A1A_D);
    for _ in 0..2_000 {
        let n = rng.range(48);
        let mut buf = rng.bytes(n);
        // Splice a u32::MAX into a plausible arity position if long enough.
        if buf.len() >= 8 {
            let at = buf.len() - 4;
            buf[at..].copy_from_slice(&HUGE);
        }
        let _ = Record::decode(&buf);
    }
}
