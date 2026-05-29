//! On-disk id-list index sidecar `<seq>.<ext>` (low-RAM core, Option B,
//! Phase 2 — see docs/specs/2026-05-29-low-ram-core-option-b.md).
#![allow(clippy::doc_markdown)]
//!
//! A generic, immutable, sorted, block-indexed map `key_bytes → [16-byte
//! id]`, mmap'd on open. Backs the four id-list secondary indexes once on
//! disk — each supplies its own key encoding and 16-byte id type:
//!
//! | index                | key_bytes        | id            |
//! |----------------------|------------------|---------------|
//! | adjacency            | entity uuid (16) | hyperedge ids |
//! | type_cluster         | type_id (4 BE)   | hyperedge ids |
//! | entity_type_cluster  | type_id (4 BE)   | entity ids    |
//! | lookup_key           | prop(4 BE)|value | entity id     |
//!
//! Only **point** lookup (`find(key) → ids`) is needed — neighbours, by-type
//! enumeration, count (= verified-`find` length), and external-key lookup
//! all reduce to it. Resident RAM = a sparse in-memory block index + the
//! mmap (OS-paged); the id lists never enter resident RAM on read.
//!
//! File format (little-endian, identical shape to `.pidx`):
//!
//! ```text
//! header                                    32 bytes
//!   magic           4   (caller-supplied 4-byte tag, e.g. b"NDADJ-"[..4])
//!   format_version  u8  (1)
//!   reserved        u8 [3]
//!   block_size      u32
//!   entry_count     u32
//!   entries_len     u64
//!   bi_count        u32
//!   reserved2       u32
//! entries (sorted ascending by key_bytes)
//!   key_len  u16 | key_bytes | id_count u32 | ids (16 × id_count)
//! block-index (sparse): key_len u16 | key_bytes | offset u64
//! trailer crc32 u32
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use thiserror::Error;

/// Only on-disk layout version.
pub const ID_LIST_FORMAT_VERSION: u8 = 1;
/// Default target entry-block size for the sparse block index.
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;

const HEADER_LEN: usize = 32;

/// Errors reading/decoding an id-list sidecar.
#[derive(Debug, Error)]
pub enum IdListIndexError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Magic mismatch (caller's tag).
    #[error("invalid id-list magic: got {got:?}, expected {expected:?}")]
    InvalidMagic {
        /// Magic bytes read.
        got: [u8; 4],
        /// Expected (caller's tag).
        expected: [u8; 4],
    },
    /// Format version newer than supported.
    #[error("unsupported id-list version {version}")]
    UnsupportedFormatVersion {
        /// Version read.
        version: u8,
    },
    /// CRC trailer mismatch.
    #[error("id-list CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC read.
        stored: u32,
        /// CRC computed.
        computed: u32,
    },
    /// File shorter than the fixed header + trailer.
    #[error("id-list too short: {len} bytes")]
    TooShort {
        /// File length.
        len: u64,
    },
    /// Region boundaries inconsistent with the file length.
    #[error("id-list malformed: {0}")]
    Malformed(&'static str),
}

/// Convert an SSTable `.ndb` path into a sibling sidecar path with `ext`.
#[must_use]
pub fn sidecar_path_for(sstable_path: &Path, ext: &str) -> PathBuf {
    let mut p = sstable_path.to_path_buf();
    p.set_extension(ext);
    p
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Accumulates `key_bytes → {16-byte id}` then writes the sidecar. Dedups
/// ids per key (BTreeSet) and emits keys ascending. Transient (one
/// SSTable's worth), freed after `finish`.
#[derive(Debug)]
pub struct IdListIndexBuilder {
    magic: [u8; 4],
    block_size: u32,
    map: BTreeMap<Vec<u8>, BTreeSet<[u8; 16]>>,
}

impl IdListIndexBuilder {
    /// New builder tagged with `magic` (the index's 4-byte discriminator).
    #[must_use]
    pub fn new(magic: [u8; 4]) -> Self {
        Self {
            magic,
            block_size: DEFAULT_BLOCK_SIZE,
            map: BTreeMap::new(),
        }
    }

    /// Record `id` under `key`.
    pub fn observe(&mut self, key: &[u8], id: [u8; 16]) {
        self.map.entry(key.to_vec()).or_default().insert(id);
    }

    /// True when nothing was observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Encode the file into a byte buffer (pure; tests + `finish`).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
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
            entries.extend_from_slice(&u16::try_from(key.len()).unwrap_or(u16::MAX).to_le_bytes());
            entries.extend_from_slice(key);
            entries.extend_from_slice(&u32::try_from(set.len()).unwrap_or(u32::MAX).to_le_bytes());
            for id in set {
                entries.extend_from_slice(id);
            }
            entry_count = entry_count.saturating_add(1);
        }

        let mut out = Vec::with_capacity(HEADER_LEN + entries.len() + block_index.len() * 32 + 4);
        out.extend_from_slice(&self.magic);
        out.push(ID_LIST_FORMAT_VERSION);
        out.extend_from_slice(&[0u8; 3]);
        out.extend_from_slice(&self.block_size.to_le_bytes());
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&u32::try_from(block_index.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        debug_assert_eq!(out.len(), HEADER_LEN);
        let bi_start = HEADER_LEN + entries.len();
        out.extend_from_slice(&entries);
        for (key, off) in &block_index {
            out.extend_from_slice(&u16::try_from(key.len()).unwrap_or(u16::MAX).to_le_bytes());
            out.extend_from_slice(key);
            out.extend_from_slice(&off.to_le_bytes());
        }
        // CRC over header + block-index region only (not the entries bulk),
        // so open never faults the whole mmap'd file.
        let mut h = Hasher::new();
        h.update(&out[..HEADER_LEN]);
        h.update(&out[bi_start..]);
        out.extend_from_slice(&h.finalize().to_le_bytes());
        out
    }

    /// Write the sidecar atomically (temp → fsync → rename → fsync dir).
    pub fn finish(self, path: &Path) -> Result<(), IdListIndexError> {
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
        f.write_str(match self {
            Self::Mmap(_) => "Backing::Mmap",
            Self::Owned(_) => "Backing::Owned",
        })
    }
}

/// mmap-backed reader over an id-list sidecar.
#[derive(Debug)]
pub struct IdListIndexFile {
    backing: Backing,
    entry_count: u32,
    entries_off: usize,
    entries_len: usize,
    block_index: Vec<(Vec<u8>, u64)>,
}

impl IdListIndexFile {
    /// Open + mmap, validating against the caller's `magic`. `Ok(None)` if
    /// the file is absent.
    pub fn open(path: &Path, magic: [u8; 4]) -> Result<Option<Self>, IdListIndexError> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(IdListIndexError::Io(e)),
        };
        // SAFETY: sidecars are immutable after publish (write-temp-rename).
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Self::from_backing(Backing::Mmap(mmap), magic).map(Some)
    }

    /// Build a reader from an owned buffer (tests).
    pub fn from_bytes(bytes: Vec<u8>, magic: [u8; 4]) -> Result<Self, IdListIndexError> {
        Self::from_backing(Backing::Owned(bytes.into_boxed_slice()), magic)
    }

    fn from_backing(backing: Backing, magic: [u8; 4]) -> Result<Self, IdListIndexError> {
        let bytes = backing.bytes();
        let total = bytes.len();
        if total < HEADER_LEN + 4 {
            return Err(IdListIndexError::TooShort { len: total as u64 });
        }
        if bytes[0..4] != magic {
            let mut got = [0u8; 4];
            got.copy_from_slice(&bytes[0..4]);
            return Err(IdListIndexError::InvalidMagic { got, expected: magic });
        }
        let version = bytes[4];
        if version > ID_LIST_FORMAT_VERSION {
            return Err(IdListIndexError::UnsupportedFormatVersion { version });
        }
        let entry_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let entries_len =
            usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().unwrap())).unwrap_or(usize::MAX);
        let bi_count = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;

        let entries_off = HEADER_LEN;
        let bi_off = entries_off
            .checked_add(entries_len)
            .ok_or(IdListIndexError::Malformed("entries_len overflow"))?;
        let trailer_off = total - 4;
        if bi_off > trailer_off {
            return Err(IdListIndexError::Malformed("entries overrun file"));
        }
        let stored = u32::from_le_bytes(bytes[trailer_off..].try_into().unwrap());
        let mut h = Hasher::new();
        h.update(&bytes[..HEADER_LEN]);
        h.update(&bytes[bi_off..trailer_off]);
        let computed = h.finalize();
        if stored != computed {
            return Err(IdListIndexError::CrcMismatch { stored, computed });
        }

        let mut block_index = Vec::with_capacity(bi_count);
        let mut pos = bi_off;
        for _ in 0..bi_count {
            if pos + 2 > trailer_off {
                return Err(IdListIndexError::Malformed("block-index truncated"));
            }
            let key_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + key_len + 8 > trailer_off {
                return Err(IdListIndexError::Malformed("block-index entry truncated"));
            }
            let key = bytes[pos..pos + key_len].to_vec();
            pos += key_len;
            let off = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
            pos += 8;
            block_index.push((key, off));
        }

        Ok(Self {
            backing,
            entry_count,
            entries_off,
            entries_len,
            block_index,
        })
    }

    /// Distinct keys in the file.
    #[must_use]
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// Resident heap estimate (sparse block index).
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.block_index.iter().map(|(k, _)| k.len() + 8 + 24).sum::<usize>()
    }

    /// Decode the entry at `rel`. Returns `(key, ids_slice, next_rel)`.
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
        let idc = u32::from_le_bytes(bytes[cp..cp + 4].try_into().ok()?) as usize;
        let ip = cp + 4;
        let ilen = idc * 16;
        if ip + ilen > end {
            return None;
        }
        Some((key, &bytes[ip..ip + ilen], ip + ilen - base))
    }

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

    /// Point lookup: all 16-byte ids stored under `key`.
    #[must_use]
    pub fn find(&self, key: &[u8]) -> Vec<[u8; 16]> {
        let bytes = self.backing.bytes();
        let mut rel = self.seek_rel(key);
        while let Some((k, ids, next)) = self.entry_at(bytes, rel) {
            match k.cmp(key) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return decode_ids(ids),
                std::cmp::Ordering::Greater => break,
            }
            rel = next;
        }
        Vec::new()
    }
}

fn decode_ids(bytes: &[u8]) -> Vec<[u8; 16]> {
    bytes
        .chunks_exact(16)
        .map(|c| {
            let mut b = [0u8; 16];
            b.copy_from_slice(c);
            b
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

    const MAGIC: [u8; 4] = *b"NDIL";

    fn id(n: u8) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[15] = n;
        b
    }

    fn build(pairs: &[(&[u8], u8)]) -> IdListIndexFile {
        let mut b = IdListIndexBuilder::new(MAGIC);
        for (k, v) in pairs {
            b.observe(k, id(*v));
        }
        IdListIndexFile::from_bytes(b.encode(), MAGIC).unwrap()
    }

    #[test]
    fn find_single_and_multi() {
        let f = build(&[(b"alice", 1), (b"bob", 2), (b"bob", 3)]);
        assert_eq!(f.find(b"alice"), vec![id(1)]);
        let mut bob = f.find(b"bob");
        bob.sort();
        assert_eq!(bob, vec![id(2), id(3)]);
        assert!(f.find(b"carol").is_empty());
    }

    #[test]
    fn many_keys_block_seek() {
        let mut b = IdListIndexBuilder::new(MAGIC);
        b.block_size = 64; // force many block markers
        let mut keys = Vec::new();
        for i in 0u32..300 {
            let k = format!("key-{i:04}");
            keys.push(k.clone());
            b.observe(k.as_bytes(), id((i % 255) as u8));
        }
        let f = IdListIndexFile::from_bytes(b.encode(), MAGIC).unwrap();
        assert!(f.block_index.len() > 1);
        for i in [0u32, 1, 50, 128, 299] {
            let k = format!("key-{i:04}");
            assert_eq!(f.find(k.as_bytes()), vec![id((i % 255) as u8)]);
        }
        assert!(f.find(b"key-9999").is_empty());
    }

    #[test]
    fn type_id_be_keys() {
        // type_cluster-style: 4-byte BE type ids.
        let mut b = IdListIndexBuilder::new(MAGIC);
        b.observe(&5u32.to_be_bytes(), id(10));
        b.observe(&5u32.to_be_bytes(), id(11));
        b.observe(&9u32.to_be_bytes(), id(20));
        let f = IdListIndexFile::from_bytes(b.encode(), MAGIC).unwrap();
        let mut got = f.find(&5u32.to_be_bytes());
        got.sort();
        assert_eq!(got, vec![id(10), id(11)]);
        assert_eq!(f.find(&9u32.to_be_bytes()), vec![id(20)]);
    }

    #[test]
    fn empty_round_trips() {
        let f = IdListIndexFile::from_bytes(IdListIndexBuilder::new(MAGIC).encode(), MAGIC).unwrap();
        assert_eq!(f.entry_count(), 0);
        assert!(f.find(b"x").is_empty());
    }

    #[test]
    fn corrupt_crc_magic_version() {
        let good = build(&[(b"k", 1)]);
        let _ = good;
        let mut b = IdListIndexBuilder::new(MAGIC);
        b.observe(b"k", id(1));
        let bytes = b.encode();

        let mut x = bytes.clone();
        let last = x.len() - 1;
        x[last] ^= 0xff;
        assert!(matches!(
            IdListIndexFile::from_bytes(x, MAGIC),
            Err(IdListIndexError::CrcMismatch { .. })
        ));

        let mut x = bytes.clone();
        x[0] = b'Z';
        assert!(matches!(
            IdListIndexFile::from_bytes(x, MAGIC),
            Err(IdListIndexError::InvalidMagic { .. })
        ));

        // Wrong magic at open time (right file, wrong caller tag).
        assert!(matches!(
            IdListIndexFile::from_bytes(bytes.clone(), *b"XXXX"),
            Err(IdListIndexError::InvalidMagic { .. })
        ));

        let mut x = bytes;
        x[4] = 99;
        assert!(matches!(
            IdListIndexFile::from_bytes(x, MAGIC),
            Err(IdListIndexError::UnsupportedFormatVersion { .. })
        ));
    }

    #[test]
    fn finish_then_open_round_trips() {
        let dir = std::env::temp_dir().join(format!("ndb-idl-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut b = IdListIndexBuilder::new(MAGIC);
        b.observe(b"entity-uuid", id(7));
        let p = dir.join("000001.adjx");
        b.finish(&p).unwrap();
        let f = IdListIndexFile::open(&p, MAGIC).unwrap().unwrap();
        assert_eq!(f.find(b"entity-uuid"), vec![id(7)]);
        assert!(IdListIndexFile::open(&dir.join("missing.adjx"), MAGIC).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sidecar_path_for_swaps_ext() {
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/db/000042.ndb"), "adjx"),
            Path::new("/tmp/db/000042.adjx")
        );
    }
}
