//! Bloom filter sidecar — `<seq>.bloom` files that let a point lookup
//! skip an SSTable entirely when the key is provably absent.
#![allow(clippy::doc_markdown)]
//!
//! A read that misses the memtable must consult every candidate SSTable.
//! Without a per-table summary, each consult costs a block-index binary
//! search plus an in-block linear scan even when the key was never in that
//! table — pure read amplification that grows with the number of tables.
//!
//! The bloom sidecar stores a probabilistic membership summary of every
//! [`SSTableKey`] in the table. `may_contain` has a tunable false-positive
//! rate but **never** a false negative: if it returns `false` the key is
//! definitely not in the table and the reader returns immediately; if it
//! returns `true` the reader falls back to the block-index path. This is
//! the classic LSM read-amplification fix.
//!
//! Like the block-index sidecar, the bloom file is best-effort: a missing
//! or corrupt sidecar simply disables the skip (the reader scans as
//! before). v1.3 SSTables that ship without a sidecar open cleanly.
//!
//! File format (little-endian, no padding):
//!
//! ```text
//! header                                  24 bytes
//!   magic           4 bytes = b"NDBL"
//!   format_version  u8       (currently 1)
//!   reserved        u8 [3]
//!   num_hashes      u32       k — hash probes per key
//!   num_bits        u64       m — filter size in bits
//!   word_count      u32       ceil(m / 64)
//!
//! bit words (word_count u64s, little-endian)
//!
//! trailer                                  4 bytes
//!   crc32           u32       CRC32 of header + bit words
//! ```

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use thiserror::Error;

use crate::sstable::SSTableKey;

/// File extension for the bloom filter sidecar.
pub const BLOOM_EXTENSION: &str = "bloom";

/// Magic bytes at the start of a sidecar file.
pub const BLOOM_MAGIC: &[u8; 4] = b"NDBL";

/// Currently the only on-disk layout version.
pub const BLOOM_FORMAT_VERSION: u8 = 1;

/// Highest layout version this build can read.
pub const BLOOM_FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;

/// Default target false-positive rate when sizing a filter (1%).
pub const DEFAULT_FALSE_POSITIVE_RATE: f64 = 0.01;

/// Fixed-overhead bytes (24-byte header + 4-byte trailer CRC).
const FIXED_OVERHEAD: usize = 24 + 4;

const LN2: f64 = std::f64::consts::LN_2;

/// Errors raised while reading or writing a bloom sidecar.
#[derive(Debug, Error)]
pub enum BloomError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Magic bytes don't match `BLOOM_MAGIC`.
    #[error("invalid bloom magic: got {got:?}, expected {expected:?}")]
    InvalidMagic {
        /// Magic bytes read.
        got: [u8; 4],
        /// Expected magic.
        expected: [u8; 4],
    },

    /// Format version is newer than this build supports.
    #[error("unsupported bloom format_version {version} (this build supports up to {supported})")]
    UnsupportedFormatVersion {
        /// Version read.
        version: u8,
        /// Highest supported.
        supported: u8,
    },

    /// CRC32 over header + bit words didn't match the stored trailer CRC.
    #[error("bloom CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC read from trailer.
        stored: u32,
        /// CRC computed over the parsed body.
        computed: u32,
    },

    /// File is shorter than the fixed header + trailer overhead.
    #[error("bloom too short: {len} bytes, need at least {needed}")]
    TooShort {
        /// File length.
        len: u64,
        /// Minimum bytes required.
        needed: u64,
    },

    /// `word_count` in the header disagrees with the bytes present.
    #[error("bloom truncated: header claims {expected} words, file holds {got}")]
    Truncated {
        /// Words the header claims.
        expected: u64,
        /// Words actually present.
        got: u64,
    },
}

// ---------------------------------------------------------------------------
// Hashing — two independent 64-bit hashes, combined via Kirsch–Mitzenmacher.
// No external hash dependency: FNV-1a with two distinct offset bases gives
// us h1 and h2. The k probes are h1 + i*h2 (mod m), with h2 forced odd so
// the probe sequence covers the whole bit space.
// ---------------------------------------------------------------------------

const FNV_OFFSET_1: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_OFFSET_2: u64 = 0x9e37_79b9_7f4a_7c15;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Derive the `(h1, h2)` pair for a key. Both hashes fold the `kind` byte
/// in first so two keys that differ only by kind hash differently.
#[must_use]
fn key_hashes(key: &SSTableKey) -> (u64, u64) {
    let mut h1 = fnv1a(FNV_OFFSET_1, &[key.kind]);
    h1 = fnv1a(h1, &key.primary);
    let mut h2 = fnv1a(FNV_OFFSET_2, &[key.kind]);
    h2 = fnv1a(h2, &key.primary);
    // Force h2 odd so gcd(h2, 2^k) == 1 and the probe walk is a full cycle
    // over power-of-two-free moduli; harmless for arbitrary m.
    (h1, h2 | 1)
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// An immutable bloom filter loaded from a sidecar. `may_contain` is the
/// only query: `false` is authoritative (key absent), `true` is "maybe".
#[derive(Debug, Clone)]
pub struct BloomFilter {
    num_hashes: u32,
    num_bits: u64,
    bits: Vec<u64>,
}

impl BloomFilter {
    /// Probabilistic membership test. Returns `false` only when the key was
    /// definitely never inserted; `true` means "possibly present".
    ///
    /// An empty filter (`num_bits == 0`) reports `false` for every key —
    /// correct for an SSTable that holds zero records.
    #[must_use]
    pub fn may_contain(&self, key: &SSTableKey) -> bool {
        if self.num_bits == 0 {
            return false;
        }
        let (h1, h2) = key_hashes(key);
        for i in 0..u64::from(self.num_hashes) {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % self.num_bits;
            let word = (bit / 64) as usize;
            let mask = 1u64 << (bit % 64);
            if self.bits[word] & mask == 0 {
                return false;
            }
        }
        true
    }

    /// Number of hash probes per key (`k`).
    #[must_use]
    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// Filter size in bits (`m`).
    #[must_use]
    pub fn num_bits(&self) -> u64 {
        self.num_bits
    }
}

// ---------------------------------------------------------------------------
// Writer — accumulates key hashes during the SSTable flush, sizes the filter
// from the final count, and emits the sidecar atomically.
// ---------------------------------------------------------------------------

/// Writer-side helper. The SSTableWriter calls [`observe_key`](Self::observe_key)
/// for every appended record; [`finish`](Self::finish) sizes and writes the
/// `.bloom` sidecar once the full key count is known.
#[derive(Debug)]
pub struct BloomWriter {
    fpr: f64,
    hashes: Vec<(u64, u64)>,
}

impl BloomWriter {
    /// New writer targeting the default false-positive rate.
    #[must_use]
    pub fn new() -> Self {
        Self::with_fpr(DEFAULT_FALSE_POSITIVE_RATE)
    }

    /// New writer targeting a custom false-positive rate (clamped to a sane
    /// `[1e-6, 0.5]` band).
    #[must_use]
    pub fn with_fpr(fpr: f64) -> Self {
        Self {
            fpr: fpr.clamp(1e-6, 0.5),
            hashes: Vec::new(),
        }
    }

    /// Record one key. Duplicate keys (e.g. multiple MVCC versions of one
    /// entity) hash identically and are harmlessly re-inserted.
    pub fn observe_key(&mut self, key: &SSTableKey) {
        self.hashes.push(key_hashes(key));
    }

    /// Number of keys observed so far.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.hashes.len()
    }

    /// Size the filter from the observed count, set the bits, and write the
    /// sidecar to `<sstable_path>.bloom` via write-temp-then-rename.
    pub fn finish(self, sidecar_path: &Path) -> Result<(), BloomError> {
        let (num_bits, num_hashes) = optimal_params(self.hashes.len(), self.fpr);
        // div_ceil(64) already yields 0 words for an empty filter and ≥1 word
        // for any non-zero bit count, so no extra clamp is needed.
        let word_count = num_bits.div_ceil(64);
        let mut bits = vec![0u64; usize::try_from(word_count).unwrap_or(usize::MAX)];
        if num_bits > 0 {
            for (h1, h2) in &self.hashes {
                for i in 0..u64::from(num_hashes) {
                    let bit = h1.wrapping_add(i.wrapping_mul(*h2)) % num_bits;
                    bits[(bit / 64) as usize] |= 1u64 << (bit % 64);
                }
            }
        }
        let bytes = encode(num_hashes, num_bits, &bits);

        let tmp = tmp_sibling(sidecar_path);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        let mut w = BufWriter::new(file);
        w.write_all(&bytes)?;
        w.flush()?;
        let f = w
            .into_inner()
            .map_err(|e| std::io::Error::other(format!("BufWriter into_inner failed: {e}")))?;
        f.sync_data()?;
        std::fs::rename(&tmp, sidecar_path)?;
        if let Some(parent) = sidecar_path.parent() {
            fsync_dir(parent)?;
        }
        Ok(())
    }
}

impl Default for BloomWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Classic bloom sizing: `m = -n·ln p / (ln 2)^2`, `k = round((m/n)·ln 2)`,
/// clamped so `k ∈ [1, 30]`. Returns `(num_bits, num_hashes)`. An `n` of 0
/// yields `(0, 0)` — an empty filter that rejects everything.
#[must_use]
fn optimal_params(n: usize, fpr: f64) -> (u64, u32) {
    if n == 0 {
        return (0, 0);
    }
    #[allow(clippy::cast_precision_loss)]
    let nf = n as f64;
    let m = (-nf * fpr.ln() / (LN2 * LN2)).ceil().max(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let num_bits = m as u64;
    let k = ((m / nf) * LN2).round().clamp(1.0, 30.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let num_hashes = k as u32;
    (num_bits, num_hashes)
}

/// Convert an SSTable `.ndb` path into the sibling `.bloom` sidecar path.
#[must_use]
pub fn sidecar_path_for(sstable_path: &Path) -> PathBuf {
    let mut p = sstable_path.to_path_buf();
    p.set_extension(BLOOM_EXTENSION);
    p
}

/// Load a sidecar from disk. Returns `None` if the file doesn't exist
/// (caller falls back to scanning every table).
pub fn load_sidecar(path: &Path) -> Result<Option<BloomFilter>, BloomError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(BloomError::Io(e)),
    };
    decode(&bytes).map(Some)
}

// ---------------------------------------------------------------------------
// Encode / decode
// ---------------------------------------------------------------------------

fn encode(num_hashes: u32, num_bits: u64, bits: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FIXED_OVERHEAD + bits.len() * 8);
    out.extend_from_slice(BLOOM_MAGIC);
    out.push(BLOOM_FORMAT_VERSION);
    out.extend_from_slice(&[0u8; 3]); // reserved
    out.extend_from_slice(&num_hashes.to_le_bytes());
    out.extend_from_slice(&num_bits.to_le_bytes());
    let word_count = u32::try_from(bits.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&word_count.to_le_bytes());
    for w in bits {
        out.extend_from_slice(&w.to_le_bytes());
    }
    let mut h = Hasher::new();
    h.update(&out);
    out.extend_from_slice(&h.finalize().to_le_bytes());
    out
}

fn decode(bytes: &[u8]) -> Result<BloomFilter, BloomError> {
    let total = bytes.len() as u64;
    if total < FIXED_OVERHEAD as u64 {
        return Err(BloomError::TooShort {
            len: total,
            needed: FIXED_OVERHEAD as u64,
        });
    }
    let magic = &bytes[0..4];
    if magic != BLOOM_MAGIC {
        let mut got = [0u8; 4];
        got.copy_from_slice(magic);
        return Err(BloomError::InvalidMagic {
            got,
            expected: *BLOOM_MAGIC,
        });
    }
    let format_version = bytes[4];
    if format_version > BLOOM_FORMAT_VERSION_MAX_SUPPORTED {
        return Err(BloomError::UnsupportedFormatVersion {
            version: format_version,
            supported: BLOOM_FORMAT_VERSION_MAX_SUPPORTED,
        });
    }
    // bytes[5..8] reserved.
    let num_hashes = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let num_bits = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    let word_count = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;

    let trailer_off = bytes.len() - 4;
    let stored_crc = u32::from_le_bytes(bytes[trailer_off..].try_into().unwrap());
    let mut h = Hasher::new();
    h.update(&bytes[..trailer_off]);
    let computed = h.finalize();
    if stored_crc != computed {
        return Err(BloomError::CrcMismatch {
            stored: stored_crc,
            computed,
        });
    }

    let words_off = 24;
    let words_end = words_off + word_count * 8;
    if words_end > trailer_off {
        return Err(BloomError::Truncated {
            expected: word_count as u64,
            got: ((trailer_off - words_off) / 8) as u64,
        });
    }
    let mut bits = Vec::with_capacity(word_count);
    let mut pos = words_off;
    for _ in 0..word_count {
        bits.push(u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap()));
        pos += 8;
    }

    Ok(BloomFilter {
        num_hashes,
        num_bits,
        bits,
    })
}

fn tmp_sibling(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    p.with_file_name(name)
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: u8, primary: &[u8]) -> SSTableKey {
        SSTableKey {
            kind,
            primary: primary.to_vec(),
        }
    }

    /// Build a filter in memory by routing through encode/decode.
    fn build(keys: &[SSTableKey], fpr: f64) -> BloomFilter {
        let mut w = BloomWriter::with_fpr(fpr);
        for k in keys {
            w.observe_key(k);
        }
        let (num_bits, num_hashes) = optimal_params(w.hashes.len(), w.fpr);
        let word_count = num_bits.div_ceil(64);
        let mut bits = vec![0u64; usize::try_from(word_count).unwrap_or(usize::MAX)];
        if num_bits > 0 {
            for (h1, h2) in &w.hashes {
                for i in 0..u64::from(num_hashes) {
                    let bit = h1.wrapping_add(i.wrapping_mul(*h2)) % num_bits;
                    bits[(bit / 64) as usize] |= 1u64 << (bit % 64);
                }
            }
        }
        decode(&encode(num_hashes, num_bits, &bits)).unwrap()
    }

    #[test]
    fn no_false_negatives_for_inserted_keys() {
        let keys: Vec<_> = (0u32..1000)
            .map(|i| key(1, &i.to_be_bytes()))
            .collect();
        let f = build(&keys, 0.01);
        for k in &keys {
            assert!(f.may_contain(k), "inserted key {k:?} reported absent");
        }
    }

    #[test]
    fn empty_filter_rejects_everything() {
        let f = build(&[], 0.01);
        assert_eq!(f.num_bits(), 0);
        assert!(!f.may_contain(&key(1, b"anything")));
    }

    #[test]
    fn false_positive_rate_is_within_a_few_x_of_target() {
        let inserted: Vec<_> = (0u32..2000).map(|i| key(1, &i.to_be_bytes())).collect();
        let f = build(&inserted, 0.01);
        // Probe 20_000 keys that were never inserted (disjoint range).
        let mut fp = 0u32;
        let trials = 20_000u32;
        for i in 100_000u32..100_000 + trials {
            if f.may_contain(&key(1, &i.to_be_bytes())) {
                fp += 1;
            }
        }
        let rate = f64::from(fp) / f64::from(trials);
        // Target 1%; allow generous slack so the test is not flaky.
        assert!(rate < 0.05, "false-positive rate {rate} too high");
    }

    #[test]
    fn kind_byte_affects_membership() {
        let f = build(&[key(1, b"shared")], 0.001);
        assert!(f.may_contain(&key(1, b"shared")));
        // Same primary, different kind — should usually miss. Not
        // guaranteed (it's probabilistic) but with fpr 0.1% it's a strong
        // signal the kind byte is folded in.
        assert!(!f.may_contain(&key(2, b"shared")));
    }

    #[test]
    fn sidecar_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("ndb-bloom-rt-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("000001.bloom");
        let keys: Vec<_> = (0u32..500).map(|i| key(1, &i.to_be_bytes())).collect();
        let mut w = BloomWriter::with_fpr(0.01);
        for k in &keys {
            w.observe_key(k);
        }
        w.finish(&p).unwrap();
        let f = load_sidecar(&p).unwrap().unwrap();
        for k in &keys {
            assert!(f.may_contain(k));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_sidecar_returns_none_when_missing() {
        let dir = std::env::temp_dir().join(format!("ndb-bloom-missing-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_sidecar(&dir.join("nope.bloom")).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupted_crc_rejected() {
        let mut bytes = encode(3, 64, &[0xdead_beefu64]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(matches!(decode(&bytes), Err(BloomError::CrcMismatch { .. })));
    }

    #[test]
    fn corrupted_magic_rejected() {
        let mut bytes = encode(3, 64, &[0u64]);
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes), Err(BloomError::InvalidMagic { .. })));
    }

    #[test]
    fn sidecar_path_swaps_extension() {
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/db/000042.ndb")),
            Path::new("/tmp/db/000042.bloom")
        );
    }

    #[test]
    fn params_scale_with_n() {
        let (m0, k0) = optimal_params(0, 0.01);
        assert_eq!((m0, k0), (0, 0));
        let (m1, k1) = optimal_params(1000, 0.01);
        let (m2, _) = optimal_params(2000, 0.01);
        assert!(m1 > 0 && k1 >= 1);
        assert!(m2 > m1, "more keys → more bits");
    }
}
