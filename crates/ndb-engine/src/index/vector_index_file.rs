//! On-disk vector index sidecar `<seq>.vidx` (low-RAM core, Option B,
//! Phase 2 — see docs/specs/2026-05-29-low-ram-core-option-b.md).
#![allow(clippy::doc_markdown)]
//!
//! An immutable, `mmap`'d store of the embeddings in one SSTable, grouped
//! by property. The engine's vector index is **brute-force** (exact k-NN),
//! so moving the vectors to disk is lossless: `search` reads the same
//! vectors from the mmap and computes the same distances — no recall
//! tradeoff. Resident RAM = a tiny per-property directory + the mmap
//! (OS-paged); the embeddings never enter resident RAM on the read path.
//!
//! Within a property every vector shares one dimension, so entries are
//! fixed-stride (`16 + dim*4` bytes) — random-access, no block index.
//!
//! File format (little-endian, no padding):
//!
//! ```text
//! header                                   16 bytes
//!   magic            4  = b"NDVX"
//!   format_version   u8  (1)
//!   reserved         u8 [3]
//!   property_count   u32
//!   reserved2        u32
//! per property (property_count, ascending by property_id):
//!   property_id      u32
//!   dim              u32
//!   entry_count      u32
//!   entries          entry_count × (entity_id[16] | f32 × dim)
//! trailer                                   4 bytes
//!   crc32            u32   over header + property sections
//! ```

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use thiserror::Error;

use crate::id::{EntityId, PropertyId};
use crate::index::vector::{Distance, distance};

/// File extension for the vector index sidecar.
pub const VECTOR_INDEX_EXTENSION: &str = "vidx";
/// Magic bytes at the start of a `.vidx` file.
pub const VECTOR_INDEX_MAGIC: &[u8; 4] = b"NDVX";
/// Only on-disk layout version.
pub const VECTOR_INDEX_FORMAT_VERSION: u8 = 1;
/// Highest layout version this build can read.
pub const VECTOR_INDEX_FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;

const HEADER_LEN: usize = 16;

/// Errors reading/decoding a `.vidx` file.
#[derive(Debug, Error)]
pub enum VectorIndexError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Magic mismatch.
    #[error("invalid vector-index magic: got {got:?}, expected {expected:?}")]
    InvalidMagic {
        /// Magic bytes read.
        got: [u8; 4],
        /// Expected.
        expected: [u8; 4],
    },
    /// Format version newer than supported.
    #[error("unsupported vector-index version {version} (supported up to {supported})")]
    UnsupportedFormatVersion {
        /// Version read.
        version: u8,
        /// Highest supported.
        supported: u8,
    },
    /// CRC trailer mismatch.
    #[error("vector-index CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC read.
        stored: u32,
        /// CRC computed.
        computed: u32,
    },
    /// File shorter than the fixed header + trailer.
    #[error("vector-index too short: {len} bytes")]
    TooShort {
        /// File length.
        len: u64,
    },
    /// Region boundaries inconsistent with the file length.
    #[error("vector-index malformed: {0}")]
    Malformed(&'static str),
}

/// Convert an SSTable `.ndb` path into the sibling `.vidx` path.
#[must_use]
pub fn sidecar_path_for(sstable_path: &Path) -> PathBuf {
    let mut p = sstable_path.to_path_buf();
    p.set_extension(VECTOR_INDEX_EXTENSION);
    p
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// One property's accumulator: locked `dim` + `(entity, vector)` rows.
type PropAccum = (usize, Vec<(EntityId, Vec<f32>)>);

/// Accumulates `(property → dim, [(entity, vector)])` while a flush /
/// compaction streams records, then writes the `.vidx` sidecar. Transient
/// (one SSTable's worth), freed after `finish`.
#[derive(Debug, Default)]
pub struct VectorIndexBuilder {
    /// property_id → accumulator. `BTreeMap` so sections are emitted in
    /// ascending property order (deterministic file).
    props: BTreeMap<u32, PropAccum>,
}

impl VectorIndexBuilder {
    /// New empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `entity`'s `vector` under `property`. The first vector seen
    /// for a property locks its dimension; later mismatched vectors are
    /// dropped (mirrors the in-RAM index).
    pub fn observe(&mut self, property: PropertyId, entity: EntityId, vector: &[f32]) {
        let slot = self
            .props
            .entry(property.get())
            .or_insert((vector.len(), Vec::new()));
        if slot.0 != vector.len() {
            return; // dimension mismatch — drop
        }
        slot.1.push((entity, vector.to_vec()));
    }

    /// True when no vectors were observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.props.values().all(|(_, v)| v.is_empty())
    }

    /// Encode the file into a byte buffer (pure; tests + `finish`).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(VECTOR_INDEX_MAGIC);
        out.push(VECTOR_INDEX_FORMAT_VERSION);
        out.extend_from_slice(&[0u8; 3]);
        out.extend_from_slice(
            &u32::try_from(self.props.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&[0u8; 4]); // reserved2
        debug_assert_eq!(out.len(), HEADER_LEN);
        for (prop_id, (dim, entries)) in &self.props {
            out.extend_from_slice(&prop_id.to_le_bytes());
            out.extend_from_slice(&u32::try_from(*dim).unwrap_or(u32::MAX).to_le_bytes());
            out.extend_from_slice(
                &u32::try_from(entries.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for (eid, v) in entries {
                out.extend_from_slice(eid.as_bytes());
                for f in v {
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
        }
        // CRC over the fixed header only — the embeddings bulk (3+ GB at
        // scale) must NOT be hashed on open or it would fault the whole
        // mmap'd file. Section headers are bounds-checked on parse; the
        // vectors rely on the atomic write + being rebuildable.
        let mut h = Hasher::new();
        h.update(&out[..HEADER_LEN]);
        out.extend_from_slice(&h.finalize().to_le_bytes());
        out
    }

    /// Write the sidecar atomically (temp → fsync → rename → fsync dir).
    pub fn finish(self, path: &Path) -> Result<(), VectorIndexError> {
        let bytes = self.encode();
        let tmp = tmp_sibling(path);
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
            .map_err(|e| std::io::Error::other(format!("BufWriter into_inner: {e}")))?;
        f.sync_data()?;
        std::fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            let _ = File::open(parent).and_then(|d| d.sync_all());
        }
        Ok(())
    }
}

/// Stream-write a single-property `.vidx` directly to disk in BOUNDED memory:
/// the entries are written one at a time as `iter` yields them, never
/// accumulated (unlike [`VectorIndexBuilder`], which holds every vector +
/// re-encodes to a second buffer — ~2×N RAM, too much at 10 GB). `dim` and
/// `count` must be known up front (a cheap counting pre-pass); vectors whose
/// length != `dim` are skipped. Same on-disk format + atomic write as
/// [`VectorIndexBuilder::finish`]. Used to build the global current-vector
/// snapshot under the app RAM cap.
pub fn write_streaming_single(
    path: &Path,
    property: PropertyId,
    dim: usize,
    count: usize,
    iter: impl Iterator<Item = (EntityId, Vec<f32>)>,
) -> Result<(), VectorIndexError> {
    let tmp = tmp_sibling(path);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    let mut w = BufWriter::new(file);

    // Header (16 bytes) — one property section.
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(VECTOR_INDEX_MAGIC);
    header.push(VECTOR_INDEX_FORMAT_VERSION);
    header.extend_from_slice(&[0u8; 3]);
    header.extend_from_slice(&1u32.to_le_bytes()); // property_count
    header.extend_from_slice(&[0u8; 4]); // reserved2
    debug_assert_eq!(header.len(), HEADER_LEN);
    w.write_all(&header)?;

    // Property section header.
    w.write_all(&property.get().to_le_bytes())?;
    w.write_all(&u32::try_from(dim).unwrap_or(u32::MAX).to_le_bytes())?;
    w.write_all(&u32::try_from(count).unwrap_or(u32::MAX).to_le_bytes())?;

    // Entries, streamed — one vector resident at a time.
    let mut written = 0usize;
    for (eid, v) in iter {
        if v.len() != dim {
            continue;
        }
        w.write_all(eid.as_bytes())?;
        for f in &v {
            w.write_all(&f.to_le_bytes())?;
        }
        written += 1;
    }
    debug_assert_eq!(
        written, count,
        "streamed entry count diverged from the pre-pass"
    );

    // CRC over the fixed header only (matches the reader + VectorIndexBuilder).
    let mut h = Hasher::new();
    h.update(&header);
    w.write_all(&h.finalize().to_le_bytes())?;

    w.flush()?;
    let f = w
        .into_inner()
        .map_err(|e| std::io::Error::other(format!("BufWriter into_inner: {e}")))?;
    f.sync_data()?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        let _ = File::open(parent).and_then(|d| d.sync_all());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

enum Backing {
    Mmap(memmap2::Mmap),
    Owned(Box<[u8]>),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Mmap(m) => &m[..],
            Self::Owned(b) => &b[..],
        }
    }
}

impl std::fmt::Debug for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mmap(_) => f.write_str("Backing::Mmap"),
            Self::Owned(_) => f.write_str("Backing::Owned"),
        }
    }
}

/// One property section's location in the file.
#[derive(Debug, Clone, Copy)]
struct Section {
    dim: usize,
    count: usize,
    /// Byte offset of the first entry.
    data_off: usize,
}

/// mmap-backed reader over a `.vidx` file. Resident state = the small
/// per-property directory; embeddings stay in the mmap.
#[derive(Debug)]
pub struct VectorIndexFile {
    backing: Backing,
    /// property_id → section.
    dir: BTreeMap<u32, Section>,
}

impl VectorIndexFile {
    /// Open + mmap a `.vidx` file. `Ok(None)` if the file is absent.
    pub fn open(path: &Path) -> Result<Option<Self>, VectorIndexError> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(VectorIndexError::Io(e)),
        };
        // SAFETY: `.vidx` files are immutable after publish (write-temp-
        // then-rename), same invariant as the SSTable mmap.
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Self::from_backing(Backing::Mmap(mmap)).map(Some)
    }

    /// Build a reader from an owned byte buffer (tests / in-memory).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, VectorIndexError> {
        Self::from_backing(Backing::Owned(bytes.into_boxed_slice()))
    }

    fn from_backing(backing: Backing) -> Result<Self, VectorIndexError> {
        let bytes = backing.bytes();
        let total = bytes.len();
        if total < HEADER_LEN + 4 {
            return Err(VectorIndexError::TooShort { len: total as u64 });
        }
        if &bytes[0..4] != VECTOR_INDEX_MAGIC {
            let mut got = [0u8; 4];
            got.copy_from_slice(&bytes[0..4]);
            return Err(VectorIndexError::InvalidMagic {
                got,
                expected: *VECTOR_INDEX_MAGIC,
            });
        }
        let version = bytes[4];
        if version > VECTOR_INDEX_FORMAT_VERSION_MAX_SUPPORTED {
            return Err(VectorIndexError::UnsupportedFormatVersion {
                version,
                supported: VECTOR_INDEX_FORMAT_VERSION_MAX_SUPPORTED,
            });
        }
        let property_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let trailer_off = total - 4;

        // CRC over the fixed header only (matches the writer) — avoids
        // faulting the embeddings bulk on open.
        let stored = u32::from_le_bytes(bytes[trailer_off..].try_into().unwrap());
        let mut h = Hasher::new();
        h.update(&bytes[..HEADER_LEN]);
        let computed = h.finalize();
        if stored != computed {
            return Err(VectorIndexError::CrcMismatch { stored, computed });
        }

        // Walk property sections to build the directory.
        let mut dir = BTreeMap::new();
        let mut pos = HEADER_LEN;
        for _ in 0..property_count {
            if pos + 12 > trailer_off {
                return Err(VectorIndexError::Malformed("property header truncated"));
            }
            let prop_id = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            let dim = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let count = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap()) as usize;
            pos += 12;
            let stride = 16 + dim * 4;
            let section_len = stride
                .checked_mul(count)
                .ok_or(VectorIndexError::Malformed("section size overflow"))?;
            if pos + section_len > trailer_off {
                return Err(VectorIndexError::Malformed("entries overrun file"));
            }
            dir.insert(
                prop_id,
                Section {
                    dim,
                    count,
                    data_off: pos,
                },
            );
            pos += section_len;
        }

        Ok(Self { backing, dir })
    }

    /// Dimension locked for `property`, if present.
    #[must_use]
    pub fn dimension(&self, property: PropertyId) -> Option<usize> {
        self.dir.get(&property.get()).map(|s| s.dim)
    }

    /// Number of vectors stored for `property`.
    #[must_use]
    pub fn len(&self, property: PropertyId) -> usize {
        self.dir.get(&property.get()).map_or(0, |s| s.count)
    }

    /// True iff `property` has no vectors here.
    #[must_use]
    pub fn is_empty(&self, property: PropertyId) -> bool {
        self.len(property) == 0
    }

    /// Resident heap estimate (the directory; vectors stay in mmap).
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.dir.len() * (4 + std::mem::size_of::<Section>() + 24)
    }

    /// Read the vector at entry `i` of a section into `buf` (len = dim).
    fn read_vector(bytes: &[u8], sec: &Section, i: usize, buf: &mut [f32]) -> EntityId {
        let stride = 16 + sec.dim * 4;
        let base = sec.data_off + i * stride;
        let mut eid = [0u8; 16];
        eid.copy_from_slice(&bytes[base..base + 16]);
        let vbase = base + 16;
        for (j, slot) in buf.iter_mut().enumerate() {
            let o = vbase + j * 4;
            *slot = f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        }
        EntityId::from_bytes(eid)
    }

    /// Brute-force k-NN over `property`, ascending by distance. Empty if
    /// the property is absent or the query dimension mismatches. Bounded
    /// memory: keeps only the running top-`k`.
    #[must_use]
    pub fn search(
        &self,
        property: PropertyId,
        query: &[f32],
        k: usize,
        metric: Distance,
    ) -> Vec<(EntityId, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let Some(sec) = self.dir.get(&property.get()) else {
            return Vec::new();
        };
        if sec.dim != query.len() {
            return Vec::new();
        }
        let bytes = self.backing.bytes();
        // Max-heap of the k smallest distances (largest at the top to evict).
        let mut heap: Vec<(f32, EntityId)> = Vec::with_capacity(k + 1);
        let mut buf = vec![0.0f32; sec.dim];
        for i in 0..sec.count {
            let eid = Self::read_vector(bytes, sec, i, &mut buf);
            let d = distance(query, &buf, metric);
            if heap.len() < k {
                heap.push((d, eid));
                if heap.len() == k {
                    // Heapify lazily once full: keep max at index 0.
                    heap.sort_by(|a, b| b.0.total_cmp(&a.0));
                }
            } else if d < heap[0].0 {
                // Replace current worst (index 0) and bubble it back.
                heap[0] = (d, eid);
                // Re-establish max at index 0 (small k → linear is fine).
                let mut max_i = 0;
                for (j, item) in heap.iter().enumerate() {
                    if item.0 > heap[max_i].0 {
                        max_i = j;
                    }
                }
                heap.swap(0, max_i);
            }
        }
        heap.sort_by(|a, b| a.0.total_cmp(&b.0));
        heap.into_iter().map(|(d, e)| (e, d)).collect()
    }
}

fn tmp_sibling(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    p.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn eid() -> EntityId {
        EntityId::now_v7()
    }

    fn build(prop: u32, dim: usize, rows: &[(EntityId, Vec<f32>)]) -> VectorIndexFile {
        let mut b = VectorIndexBuilder::new();
        let _ = dim;
        for (e, v) in rows {
            b.observe(PropertyId::new(prop), *e, v);
        }
        VectorIndexFile::from_bytes(b.encode()).unwrap()
    }

    #[test]
    fn search_l2_nearest_first() {
        let a = eid();
        let b = eid();
        let c = eid();
        let f = build(
            10,
            3,
            &[
                (a, vec![1.0, 0.0, 0.0]),
                (b, vec![0.0, 1.0, 0.0]),
                (c, vec![0.9, 0.1, 0.0]),
            ],
        );
        let got = f.search(
            PropertyId::new(10),
            &[1.0, 0.0, 0.0],
            2,
            Distance::L2Squared,
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, a); // exact match, distance 0
        assert_eq!(got[1].0, c); // closest of the rest
    }

    #[test]
    fn search_matches_bruteforce_over_many() {
        // Parity with a plain brute-force over the same vectors.
        let mut rows = Vec::new();
        for i in 0..200u32 {
            let v = vec![(i as f32) * 0.01, ((i % 7) as f32) * 0.1, 1.0];
            rows.push((eid(), v));
        }
        let f = build(1, 3, &rows);
        let q = [0.5f32, 0.3, 1.0];
        let got = f.search(PropertyId::new(1), &q, 5, Distance::L2Squared);
        // Reference: sort all by distance.
        let mut reference: Vec<(EntityId, f32)> = rows
            .iter()
            .map(|(e, v)| (*e, distance(&q, v, Distance::L2Squared)))
            .collect();
        reference.sort_by(|a, b| a.1.total_cmp(&b.1));
        let want: Vec<EntityId> = reference.iter().take(5).map(|(e, _)| *e).collect();
        let got_ids: Vec<EntityId> = got.iter().map(|(e, _)| *e).collect();
        assert_eq!(got_ids, want);
    }

    #[test]
    fn dimension_mismatch_returns_empty() {
        let f = build(10, 3, &[(eid(), vec![1.0, 2.0, 3.0])]);
        assert!(
            f.search(PropertyId::new(10), &[1.0, 2.0], 5, Distance::L2Squared)
                .is_empty()
        );
        assert_eq!(f.dimension(PropertyId::new(10)), Some(3));
    }

    #[test]
    fn properties_isolated() {
        let a = eid();
        let b = eid();
        let mut bld = VectorIndexBuilder::new();
        bld.observe(PropertyId::new(1), a, &[1.0, 0.0]);
        bld.observe(PropertyId::new(2), b, &[0.0, 1.0, 0.0]);
        let f = VectorIndexFile::from_bytes(bld.encode()).unwrap();
        assert_eq!(f.len(PropertyId::new(1)), 1);
        assert_eq!(f.len(PropertyId::new(2)), 1);
        assert_eq!(f.dimension(PropertyId::new(1)), Some(2));
        assert_eq!(f.dimension(PropertyId::new(2)), Some(3));
        let g = f.search(PropertyId::new(1), &[1.0, 0.0], 5, Distance::L2Squared);
        assert_eq!(g, vec![(a, 0.0)]);
    }

    #[test]
    fn cosine_metric() {
        let a = eid();
        let b = eid();
        let f = build(1, 2, &[(a, vec![1.0, 0.0]), (b, vec![0.0, 1.0])]);
        let got = f.search(PropertyId::new(1), &[2.0, 0.0], 1, Distance::Cosine);
        assert_eq!(got[0].0, a); // same direction → cosine distance ~0
    }

    #[test]
    fn empty_and_missing() {
        let f = VectorIndexFile::from_bytes(VectorIndexBuilder::new().encode()).unwrap();
        assert!(
            f.search(PropertyId::new(1), &[1.0], 3, Distance::L2Squared)
                .is_empty()
        );
        assert_eq!(f.len(PropertyId::new(1)), 0);
    }

    #[test]
    fn dim_mismatch_on_observe_dropped() {
        let a = eid();
        let b = eid();
        let mut bld = VectorIndexBuilder::new();
        bld.observe(PropertyId::new(1), a, &[1.0, 2.0]); // locks dim=2
        bld.observe(PropertyId::new(1), b, &[1.0, 2.0, 3.0]); // dropped
        let f = VectorIndexFile::from_bytes(bld.encode()).unwrap();
        assert_eq!(f.len(PropertyId::new(1)), 1);
    }

    #[test]
    fn corrupt_crc_magic_version() {
        let mut bld = VectorIndexBuilder::new();
        bld.observe(PropertyId::new(1), eid(), &[1.0, 2.0]);
        let good = bld.encode();

        let mut b = good.clone();
        let last = b.len() - 1;
        b[last] ^= 0xff;
        assert!(matches!(
            VectorIndexFile::from_bytes(b),
            Err(VectorIndexError::CrcMismatch { .. })
        ));

        let mut b = good.clone();
        b[0] = b'X';
        assert!(matches!(
            VectorIndexFile::from_bytes(b),
            Err(VectorIndexError::InvalidMagic { .. })
        ));

        let mut b = good;
        b[4] = 99;
        assert!(matches!(
            VectorIndexFile::from_bytes(b),
            Err(VectorIndexError::UnsupportedFormatVersion { .. })
        ));
    }

    #[test]
    fn finish_then_open_round_trips() {
        let dir = std::env::temp_dir().join(format!("ndb-vidx-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = eid();
        let mut bld = VectorIndexBuilder::new();
        bld.observe(PropertyId::new(7), a, &[3.0, 4.0]);
        let p = dir.join("000001.vidx");
        bld.finish(&p).unwrap();
        let f = VectorIndexFile::open(&p).unwrap().unwrap();
        assert_eq!(
            f.search(PropertyId::new(7), &[3.0, 4.0], 1, Distance::L2Squared),
            vec![(a, 0.0)]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_missing_returns_none() {
        let p = std::env::temp_dir().join(format!(
            "ndb-vidx-missing-{}.vidx",
            uuid::Uuid::now_v7().simple()
        ));
        assert!(VectorIndexFile::open(&p).unwrap().is_none());
    }

    #[test]
    fn sidecar_path_swaps_extension() {
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/db/000042.ndb")),
            Path::new("/tmp/db/000042.vidx")
        );
    }
}
