//! On-disk property index sidecar `<seq>.pidx` (low-RAM core, Option B,
//! Phase 1 — see docs/specs/2026-05-29-low-ram-core-option-b.md).
#![allow(clippy::doc_markdown)]
//!
//! An immutable, sorted, block-indexed map
//! `(TypeId, PropertyId, value_bytes) → [EntityId]`, written once alongside
//! its sibling `<seq>.ndb` SSTable at flush/compaction and `mmap`'d on
//! open. Mirrors the `.idx` block-index sidecar: self-contained magic +
//! version, CRC trailer, graceful fallback (missing/corrupt → caller
//! rebuilds the property index in RAM).
//!
//! This module is deliberately **value-agnostic**: it deals only in raw
//! `value_bytes` (the order-preserving encoding the engine already uses,
//! `property_btree::value_to_index_bytes`). On-disk byte order over the
//! composite key equals `(type, prop, value)` logical order, so `range`
//! and `top_k` are contiguous scans.
//!
//! Resident RAM = a **sparse** in-memory block index (one marker per
//! ~`block_size` of entries) + the mmap (OS-paged). The full entry table
//! never enters resident RAM on the read path.
//!
//! File format (little-endian, no padding):
//!
//! ```text
//! header                                    32 bytes
//!   magic           4  = b"NDPX"
//!   format_version  u8  (1)
//!   reserved        u8 [3]
//!   block_size      u32
//!   entry_count     u32
//!   entries_len     u64       byte length of the entries region
//!   bi_count        u32       sparse block-index entry count
//!   reserved2       u32
//! entries region   (entries_len bytes, sorted ascending by composite key)
//!   per entry:
//!     key_len       u16
//!     key_bytes     key_len   = type(4 BE) | prop(4 BE) | value_bytes
//!     entity_count  u32
//!     entity_ids    16 × entity_count
//! block-index region   (bi_count entries; one per ~block_size of entries)
//!   per entry:
//!     key_len       u16
//!     key_bytes     key_len
//!     offset        u64       byte offset into the entries region
//! trailer                                   4 bytes
//!   crc32           u32       over header + entries + block-index
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use thiserror::Error;

use crate::id::{EntityId, PropertyId, TypeId};

/// File extension for the property index sidecar.
pub const PROPERTY_INDEX_EXTENSION: &str = "pidx";
/// Magic bytes at the start of a `.pidx` file.
pub const PROPERTY_INDEX_MAGIC: &[u8; 4] = b"NDPX";
/// Only on-disk layout version.
pub const PROPERTY_INDEX_FORMAT_VERSION: u8 = 1;
/// Highest layout version this build can read.
pub const PROPERTY_INDEX_FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;
/// Default target entry-block size (bytes) for the sparse block index.
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;

const HEADER_LEN: usize = 32;

/// Errors reading/decoding a `.pidx` file.
#[derive(Debug, Error)]
pub enum PropertyIndexError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Magic mismatch.
    #[error("invalid property-index magic: got {got:?}, expected {expected:?}")]
    InvalidMagic {
        /// Magic bytes read.
        got: [u8; 4],
        /// Expected.
        expected: [u8; 4],
    },
    /// Format version newer than supported.
    #[error("unsupported property-index version {version} (supported up to {supported})")]
    UnsupportedFormatVersion {
        /// Version read.
        version: u8,
        /// Highest supported.
        supported: u8,
    },
    /// CRC trailer mismatch.
    #[error("property-index CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC read.
        stored: u32,
        /// CRC computed.
        computed: u32,
    },
    /// File shorter than the fixed header + trailer.
    #[error("property-index too short: {len} bytes")]
    TooShort {
        /// File length.
        len: u64,
    },
    /// Region boundaries inconsistent with the file length.
    #[error("property-index malformed: {0}")]
    Malformed(&'static str),
}

/// Convert an SSTable `.ndb` path into the sibling `.pidx` path.
#[must_use]
pub fn sidecar_path_for(sstable_path: &Path) -> PathBuf {
    let mut p = sstable_path.to_path_buf();
    p.set_extension(PROPERTY_INDEX_EXTENSION);
    p
}

/// Composite sort key `type(4 BE) | prop(4 BE) | value_bytes`. Big-endian
/// type/prop so a plain byte compare matches `(type, prop, value)` order.
#[must_use]
pub fn composite_key(type_id: TypeId, prop: PropertyId, value_bytes: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + value_bytes.len());
    k.extend_from_slice(&type_id.get().to_be_bytes());
    k.extend_from_slice(&prop.get().to_be_bytes());
    k.extend_from_slice(value_bytes);
    k
}

/// 8-byte `(type, prop)` bucket prefix — every entry of one indexed column
/// shares it.
#[must_use]
fn bucket_prefix(type_id: TypeId, prop: PropertyId) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&type_id.get().to_be_bytes());
    p[4..8].copy_from_slice(&prop.get().to_be_bytes());
    p
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Accumulates `(type, prop, value_bytes) → entities` while a flush /
/// compaction streams records, then writes the sorted sidecar. The map is
/// transient (one SSTable's worth) and freed after `finish`.
#[derive(Debug)]
pub struct PropertyIndexBuilder {
    block_size: u32,
    map: BTreeMap<Vec<u8>, BTreeSet<[u8; 16]>>,
}

impl PropertyIndexBuilder {
    /// New builder with the default block size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_block_size(DEFAULT_BLOCK_SIZE)
    }

    /// New builder with a custom block size.
    #[must_use]
    pub fn with_block_size(block_size: u32) -> Self {
        Self {
            block_size: block_size.max(64),
            map: BTreeMap::new(),
        }
    }

    /// Record `entity` under `(type, prop, value_bytes)`.
    pub fn observe(
        &mut self,
        type_id: TypeId,
        prop: PropertyId,
        value_bytes: &[u8],
        entity: EntityId,
    ) {
        let key = composite_key(type_id, prop, value_bytes);
        self.map.entry(key).or_default().insert(*entity.as_bytes());
    }

    /// True when no entries were observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Distinct composite keys.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.map.len()
    }

    /// Encode the full file into a byte buffer (pure; used by tests and
    /// by `finish`).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        // 1. Entries region + remember block-index markers.
        let mut entries = Vec::new();
        let mut block_index: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut next_threshold: u64 = 0;
        let bs = u64::from(self.block_size);
        let mut entry_count: u32 = 0;
        for (key, set) in &self.map {
            let off = entries.len() as u64;
            if off >= next_threshold {
                block_index.push((key.clone(), off));
                next_threshold = ((off / bs) + 1) * bs;
            }
            let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
            entries.extend_from_slice(&key_len.to_le_bytes());
            entries.extend_from_slice(key);
            let ecount = u32::try_from(set.len()).unwrap_or(u32::MAX);
            entries.extend_from_slice(&ecount.to_le_bytes());
            for e in set {
                entries.extend_from_slice(e);
            }
            entry_count = entry_count.saturating_add(1);
        }

        // 2. Header.
        let mut out = Vec::with_capacity(HEADER_LEN + entries.len() + block_index.len() * 32 + 4);
        out.extend_from_slice(PROPERTY_INDEX_MAGIC);
        out.push(PROPERTY_INDEX_FORMAT_VERSION);
        out.extend_from_slice(&[0u8; 3]);
        out.extend_from_slice(&self.block_size.to_le_bytes());
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&u32::try_from(block_index.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // reserved2
        debug_assert_eq!(out.len(), HEADER_LEN);

        // 3. Entries, then block index.
        out.extend_from_slice(&entries);
        for (key, off) in &block_index {
            let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&key_len.to_le_bytes());
            out.extend_from_slice(key);
            out.extend_from_slice(&off.to_le_bytes());
        }

        // 4. CRC trailer.
        let mut h = Hasher::new();
        h.update(&out);
        out.extend_from_slice(&h.finalize().to_le_bytes());
        out
    }

    /// Write the sidecar atomically (temp → fsync → rename → fsync dir).
    pub fn finish(self, path: &Path) -> Result<(), PropertyIndexError> {
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

impl Default for PropertyIndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Backing bytes for a [`PropertyIndexFile`] — `mmap` in production,
/// owned buffer in tests.
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

/// mmap-backed reader over a `.pidx` file. Holds only a sparse block index
/// resident; entries are read from the mmap on demand.
#[derive(Debug)]
pub struct PropertyIndexFile {
    backing: Backing,
    block_size: u32,
    entry_count: u32,
    entries_off: usize,
    entries_len: usize,
    /// Sparse `(composite_key, offset-into-entries-region)`, ascending.
    block_index: Vec<(Vec<u8>, u64)>,
}

impl PropertyIndexFile {
    /// Open + mmap a `.pidx` file. `Ok(None)` if the file is absent
    /// (caller falls back to a RAM rebuild).
    pub fn open(path: &Path) -> Result<Option<Self>, PropertyIndexError> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(PropertyIndexError::Io(e)),
        };
        // SAFETY: `.pidx` files are immutable after publish (write-temp-
        // then-rename), same invariant as the SSTable mmap.
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Self::from_backing(Backing::Mmap(mmap)).map(Some)
    }

    /// Build a reader from an owned byte buffer (tests / in-memory).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, PropertyIndexError> {
        Self::from_backing(Backing::Owned(bytes.into_boxed_slice()))
    }

    fn from_backing(backing: Backing) -> Result<Self, PropertyIndexError> {
        let bytes = backing.bytes();
        let total = bytes.len();
        if total < HEADER_LEN + 4 {
            return Err(PropertyIndexError::TooShort { len: total as u64 });
        }
        if &bytes[0..4] != PROPERTY_INDEX_MAGIC {
            let mut got = [0u8; 4];
            got.copy_from_slice(&bytes[0..4]);
            return Err(PropertyIndexError::InvalidMagic {
                got,
                expected: *PROPERTY_INDEX_MAGIC,
            });
        }
        let version = bytes[4];
        if version > PROPERTY_INDEX_FORMAT_VERSION_MAX_SUPPORTED {
            return Err(PropertyIndexError::UnsupportedFormatVersion {
                version,
                supported: PROPERTY_INDEX_FORMAT_VERSION_MAX_SUPPORTED,
            });
        }
        let block_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let entry_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let entries_len =
            usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().unwrap())).unwrap_or(usize::MAX);
        let bi_count = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;

        let entries_off = HEADER_LEN;
        let bi_off = entries_off
            .checked_add(entries_len)
            .ok_or(PropertyIndexError::Malformed("entries_len overflow"))?;
        let trailer_off = total
            .checked_sub(4)
            .ok_or(PropertyIndexError::TooShort { len: total as u64 })?;
        if bi_off > trailer_off {
            return Err(PropertyIndexError::Malformed("entries region overruns file"));
        }

        // CRC over everything before the trailer.
        let stored = u32::from_le_bytes(bytes[trailer_off..].try_into().unwrap());
        let mut h = Hasher::new();
        h.update(&bytes[..trailer_off]);
        let computed = h.finalize();
        if stored != computed {
            return Err(PropertyIndexError::CrcMismatch { stored, computed });
        }

        // Parse the sparse block index into RAM.
        let mut block_index = Vec::with_capacity(bi_count);
        let mut pos = bi_off;
        for _ in 0..bi_count {
            if pos + 2 > trailer_off {
                return Err(PropertyIndexError::Malformed("block-index truncated"));
            }
            let key_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + key_len + 8 > trailer_off {
                return Err(PropertyIndexError::Malformed("block-index entry truncated"));
            }
            let key = bytes[pos..pos + key_len].to_vec();
            pos += key_len;
            let off = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
            pos += 8;
            block_index.push((key, off));
        }

        Ok(Self {
            backing,
            block_size,
            entry_count,
            entries_off,
            entries_len,
            block_index,
        })
    }

    /// Distinct composite keys in the file.
    #[must_use]
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// Resident heap estimate (the sparse block index + struct).
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        let mut n = std::mem::size_of::<Self>();
        for (k, _) in &self.block_index {
            n += k.len() + 8 + 24;
        }
        n
    }

    /// Block size used when written (diagnostic).
    #[must_use]
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    // -- internal entry decoding -------------------------------------------

    /// Decode the entry starting at `rel` (offset within the entries
    /// region). Returns `(key, entity_id_bytes_slice, next_rel)`.
    fn entry_at<'a>(&self, bytes: &'a [u8], rel: usize) -> Option<(&'a [u8], &'a [u8], usize)> {
        let base = self.entries_off;
        let abs = base + rel;
        let end = base + self.entries_len;
        if abs + 2 > end {
            return None;
        }
        let key_len = u16::from_le_bytes(bytes[abs..abs + 2].try_into().ok()?) as usize;
        let kp = abs + 2;
        if kp + key_len + 4 > end {
            return None;
        }
        let key = &bytes[kp..kp + key_len];
        let cp = kp + key_len;
        let ecount = u32::from_le_bytes(bytes[cp..cp + 4].try_into().ok()?) as usize;
        let ep = cp + 4;
        let elen = ecount * 16;
        if ep + elen > end {
            return None;
        }
        let ents = &bytes[ep..ep + elen];
        Some((key, ents, ep + elen - base))
    }

    /// Seek offset (into the entries region) to start scanning for
    /// `target` — the largest block-index marker whose key ≤ target, or 0.
    fn seek_rel(&self, target: &[u8]) -> usize {
        if self.block_index.is_empty() {
            return 0;
        }
        let idx = self.block_index.partition_point(|(k, _)| k.as_slice() <= target);
        if idx == 0 {
            0
        } else {
            usize::try_from(self.block_index[idx - 1].1).unwrap_or(usize::MAX)
        }
    }

    /// Point lookup: entities whose `(type, prop)` value equals `value_bytes`.
    #[must_use]
    pub fn find(&self, type_id: TypeId, prop: PropertyId, value_bytes: &[u8]) -> Vec<EntityId> {
        let target = composite_key(type_id, prop, value_bytes);
        let bytes = self.backing.bytes();
        let mut rel = self.seek_rel(&target);
        while let Some((key, ents, next)) = self.entry_at(bytes, rel) {
            match key.cmp(target.as_slice()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return decode_entities(ents),
                std::cmp::Ordering::Greater => break,
            }
            rel = next;
        }
        Vec::new()
    }

    /// Range query over `[lo, hi]` (inclusive; `None` = unbounded on that
    /// side, within the `(type, prop)` bucket).
    #[must_use]
    pub fn range(
        &self,
        type_id: TypeId,
        prop: PropertyId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Vec<EntityId> {
        let prefix = bucket_prefix(type_id, prop);
        let lo_key = composite_key(type_id, prop, lo.unwrap_or(&[]));
        let hi_key = hi.map(|h| composite_key(type_id, prop, h));
        let bytes = self.backing.bytes();
        let mut rel = self.seek_rel(&lo_key);
        let mut out = Vec::new();
        while let Some((key, ents, next)) = self.entry_at(bytes, rel) {
            rel = next;
            if key.len() < 8 || key[0..8] != prefix {
                // Past the bucket (or before it — keep scanning if before).
                if key[0..key.len().min(8)] > prefix[..key.len().min(8)] {
                    break;
                }
                continue;
            }
            if key < lo_key.as_slice() {
                continue;
            }
            if let Some(hk) = &hi_key
                && key > hk.as_slice()
            {
                break;
            }
            out.extend(decode_entities(ents));
        }
        out
    }

    /// Top-`k` by value, **highest first**, within the `(type, prop)`
    /// bucket. Returns `(value_bytes, entity)` so callers can k-way-merge
    /// across sidecars. Bounded memory: scans the bucket forward keeping a
    /// trailing window of at most `k` (+ one entry) entities.
    #[must_use]
    pub fn top_k(
        &self,
        type_id: TypeId,
        prop: PropertyId,
        k: usize,
    ) -> Vec<(Vec<u8>, EntityId)> {
        if k == 0 {
            return Vec::new();
        }
        let prefix = bucket_prefix(type_id, prop);
        let start_key = composite_key(type_id, prop, &[]);
        let bytes = self.backing.bytes();
        let mut rel = self.seek_rel(&start_key);
        // Trailing window of entries (value_bytes, entities), ascending.
        let mut tail: VecDeque<(Vec<u8>, Vec<EntityId>)> = VecDeque::new();
        let mut tail_total = 0usize;
        while let Some((key, ents, next)) = self.entry_at(bytes, rel) {
            rel = next;
            if key.len() < 8 || key[0..8] != prefix {
                if key[0..key.len().min(8)] > prefix[..key.len().min(8)] {
                    break;
                }
                continue;
            }
            let value_bytes = key[8..].to_vec();
            let entities = decode_entities(ents);
            tail_total += entities.len();
            tail.push_back((value_bytes, entities));
            // Drop the front while doing so still leaves ≥ k entities.
            while let Some(front) = tail.front() {
                if tail_total - front.1.len() >= k {
                    tail_total -= front.1.len();
                    tail.pop_front();
                } else {
                    break;
                }
            }
        }
        // Flatten back-to-front (highest value first), cap at k.
        let mut out = Vec::with_capacity(k);
        for (value_bytes, entities) in tail.iter().rev() {
            for e in entities {
                out.push((value_bytes.clone(), *e));
                if out.len() >= k {
                    return out;
                }
            }
        }
        out
    }
}

fn decode_entities(bytes: &[u8]) -> Vec<EntityId> {
    bytes
        .chunks_exact(16)
        .map(|c| {
            let mut b = [0u8; 16];
            b.copy_from_slice(c);
            EntityId::from_bytes(b)
        })
        .collect()
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

    /// i64 → order-preserving bytes (mirror of value_to_index_bytes for I64).
    fn ib(n: i64) -> Vec<u8> {
        ((n.cast_unsigned()) ^ (1u64 << 63)).to_be_bytes().to_vec()
    }

    fn eid() -> EntityId {
        EntityId::now_v7()
    }

    fn build(pairs: &[(u32, u32, i64, EntityId)], bs: u32) -> PropertyIndexFile {
        let mut b = PropertyIndexBuilder::with_block_size(bs);
        for (t, p, v, e) in pairs {
            b.observe(TypeId::new(*t), PropertyId::new(*p), &ib(*v), *e);
        }
        PropertyIndexFile::from_bytes(b.encode()).unwrap()
    }

    #[test]
    fn find_exact() {
        let a = eid();
        let b = eid();
        let f = build(&[(1, 10, 30, a), (1, 10, 40, b)], 64);
        assert_eq!(f.find(TypeId::new(1), PropertyId::new(10), &ib(30)), vec![a]);
        assert!(f.find(TypeId::new(1), PropertyId::new(10), &ib(99)).is_empty());
    }

    #[test]
    fn find_multiple_entities_same_value() {
        let mut ids: Vec<_> = (0..3).map(|_| eid()).collect();
        let f = build(
            &[
                (1, 10, 42, ids[0]),
                (1, 10, 42, ids[1]),
                (1, 10, 42, ids[2]),
            ],
            64,
        );
        let mut got = f.find(TypeId::new(1), PropertyId::new(10), &ib(42));
        got.sort();
        ids.sort();
        assert_eq!(got, ids);
    }

    #[test]
    fn range_inclusive() {
        let ids: Vec<_> = (0..5).map(|_| eid()).collect();
        let vals = [10i64, 20, 30, 40, 50];
        let pairs: Vec<_> = vals.iter().zip(&ids).map(|(v, e)| (1u32, 10u32, *v, *e)).collect();
        let f = build(&pairs, 64);
        let mut got = f.range(TypeId::new(1), PropertyId::new(10), Some(&ib(20)), Some(&ib(40)));
        got.sort();
        let mut want = vec![ids[1], ids[2], ids[3]];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn range_unbounded_both_sides_one_bucket() {
        let ids: Vec<_> = (0..4).map(|_| eid()).collect();
        let pairs: Vec<_> = [-5i64, 0, 5, 10]
            .iter()
            .zip(&ids)
            .map(|(v, e)| (1u32, 10u32, *v, *e))
            .collect();
        let f = build(&pairs, 64);
        let got = f.range(TypeId::new(1), PropertyId::new(10), None, None);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn range_isolates_bucket_from_other_type_and_prop() {
        let a = eid();
        let b = eid();
        let c = eid();
        // Same value across (type,prop) buckets — range must not bleed.
        let f = build(&[(1, 10, 5, a), (1, 11, 5, b), (2, 10, 5, c)], 64);
        let got = f.range(TypeId::new(1), PropertyId::new(10), None, None);
        assert_eq!(got, vec![a]);
    }

    #[test]
    fn top_k_highest_first() {
        let mut by_val = std::collections::HashMap::new();
        let mut pairs = Vec::new();
        for v in [5i64, 100, 30, 999, 7, 250] {
            let e = eid();
            by_val.insert(e, v);
            pairs.push((1u32, 10u32, v, e));
        }
        let f = build(&pairs, 64);
        let top = f.top_k(TypeId::new(1), PropertyId::new(10), 3);
        let vals: Vec<i64> = top.iter().map(|(_, e)| by_val[e]).collect();
        assert_eq!(vals, vec![999, 250, 100]);
        // k beyond column → all, descending.
        let all = f.top_k(TypeId::new(1), PropertyId::new(10), 100);
        assert_eq!(all.len(), 6);
        assert_eq!(by_val[&all[0].1], 999);
        assert_eq!(by_val[&all[5].1], 5);
        assert!(f.top_k(TypeId::new(1), PropertyId::new(10), 0).is_empty());
    }

    #[test]
    fn top_k_ties_keep_all_at_boundary_value() {
        // Three entities tie at the 2nd-highest value.
        let a = eid();
        let b = eid();
        let c = eid();
        let d = eid();
        let f = build(
            &[(1, 10, 100, a), (1, 10, 50, b), (1, 10, 50, c), (1, 10, 50, d)],
            64,
        );
        let top = f.top_k(TypeId::new(1), PropertyId::new(10), 2);
        // Highest is `a` (100); the next slot is one of the 50-tie.
        assert_eq!(top[0].1, a);
        assert!([b, c, d].contains(&top[1].1));
    }

    #[test]
    fn many_blocks_seek_correct() {
        // Force many block-index markers with a tiny block size.
        let ids: Vec<_> = (0..200).map(|_| eid()).collect();
        let pairs: Vec<_> = (0..200)
            .map(|i| (1u32, 10u32, i as i64, ids[i]))
            .collect();
        let f = build(&pairs, 64); // tiny blocks → dense sparse index
        assert!(f.block_index.len() > 1, "expected multiple block markers");
        // Spot-check exact lookups across the file.
        for i in [0usize, 1, 63, 64, 99, 150, 199] {
            assert_eq!(
                f.find(TypeId::new(1), PropertyId::new(10), &ib(i as i64)),
                vec![ids[i]]
            );
        }
        // Range in the middle.
        let mut got = f.range(TypeId::new(1), PropertyId::new(10), Some(&ib(100)), Some(&ib(102)));
        got.sort();
        let mut want = vec![ids[100], ids[101], ids[102]];
        want.sort();
        assert_eq!(got, want);
        // Global top-3.
        let top = f.top_k(TypeId::new(1), PropertyId::new(10), 3);
        assert_eq!(top.iter().map(|(_, e)| *e).collect::<Vec<_>>(), vec![ids[199], ids[198], ids[197]]);
    }

    #[test]
    fn empty_builder_round_trips() {
        let f = PropertyIndexFile::from_bytes(PropertyIndexBuilder::new().encode()).unwrap();
        assert_eq!(f.entry_count(), 0);
        assert!(f.find(TypeId::new(1), PropertyId::new(1), &ib(0)).is_empty());
        assert!(f.top_k(TypeId::new(1), PropertyId::new(1), 5).is_empty());
    }

    #[test]
    fn corrupt_crc_rejected() {
        let mut bytes = build_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        match PropertyIndexFile::from_bytes(bytes) {
            Err(PropertyIndexError::CrcMismatch { .. }) => {}
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_magic_rejected() {
        let mut bytes = build_bytes();
        bytes[0] = b'X';
        match PropertyIndexFile::from_bytes(bytes) {
            Err(PropertyIndexError::InvalidMagic { .. }) => {}
            other => panic!("expected InvalidMagic, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut bytes = build_bytes();
        bytes[4] = 99;
        match PropertyIndexFile::from_bytes(bytes) {
            Err(PropertyIndexError::UnsupportedFormatVersion { .. }) => {}
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    fn build_bytes() -> Vec<u8> {
        let mut b = PropertyIndexBuilder::with_block_size(64);
        b.observe(TypeId::new(1), PropertyId::new(10), &ib(7), eid());
        b.encode()
    }

    #[test]
    fn finish_then_open_round_trips() {
        let dir = std::env::temp_dir().join(format!("ndb-pidx-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = eid();
        let b = eid();
        let mut bld = PropertyIndexBuilder::with_block_size(64);
        bld.observe(TypeId::new(1), PropertyId::new(10), &ib(30), a);
        bld.observe(TypeId::new(1), PropertyId::new(10), &ib(40), b);
        let p = dir.join("000001.pidx");
        bld.finish(&p).unwrap();
        let f = PropertyIndexFile::open(&p).unwrap().unwrap();
        assert_eq!(f.find(TypeId::new(1), PropertyId::new(10), &ib(40)), vec![b]);
        assert_eq!(f.entry_count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_missing_returns_none() {
        let p = std::env::temp_dir().join(format!("ndb-pidx-missing-{}.pidx", uuid::Uuid::now_v7().simple()));
        assert!(PropertyIndexFile::open(&p).unwrap().is_none());
    }

    #[test]
    fn sidecar_path_swaps_extension() {
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/db/000042.ndb")),
            Path::new("/tmp/db/000042.pidx")
        );
    }
}
