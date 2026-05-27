//! Block index sidecar — `<seq>.idx` files that give O(log N) random
//! access to records inside an SSTable.
#![allow(clippy::doc_markdown)]
//!
//! The v1.3 reader does a linear O(N) scan inside `SSTableReader::find`.
//! The sidecar stores, for every ~`BLOCK_SIZE` bytes of data, one
//! `(SSTableKey, byte_offset)` pair. `find` binary-searches the sidecar
//! to identify a block, then linear-scans up to one block to find the
//! target — bounded O(log N) sidecar lookup + O(BLOCK_SIZE) scan.
//!
//! File format (little-endian, no padding):
//!
//! ```text
//! header                                  16 bytes
//!   magic           4 bytes = b"NDIX"
//!   format_version  u8       (currently 1)
//!   reserved        u8 [3]
//!   block_size      u32       target data block (bytes), e.g. 4096
//!   entry_count     u32
//!
//! entries (sorted by SSTableKey ascending, entry_count items)
//!   per entry:
//!     kind        u8
//!     key_len     u16
//!     key_bytes   key_len bytes
//!     offset      u64       byte offset in the SSTable data section
//!
//! trailer                                  4 bytes
//!   crc32          u32       CRC32 of header + entries
//! ```
//!
//! The sidecar is written via the same write-temp-then-rename pattern
//! as the main SSTable (`tmp_sibling`, fsync, rename, fsync_dir). An
//! engine that finds a missing or corrupt sidecar falls back to a
//! linear scan — sidecars are an optimisation, not a correctness
//! requirement. v1.3 databases that ship without sidecars open cleanly
//! in v2.0.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use thiserror::Error;

use crate::sstable::{SSTABLE_EXTENSION, SSTableKey};

/// File extension for the block index sidecar.
pub const BLOCK_INDEX_EXTENSION: &str = "idx";

/// Magic bytes at the start of a sidecar file.
pub const BLOCK_INDEX_MAGIC: &[u8; 4] = b"NDIX";

/// Currently the only on-disk layout version.
pub const BLOCK_INDEX_FORMAT_VERSION: u8 = 1;

/// Highest layout version this build can read.
pub const BLOCK_INDEX_FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;

/// Default target data block size, in bytes. Writers emit one index
/// entry per `BLOCK_SIZE` of data. Tunable via [`BlockIndexWriter::with_block_size`].
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Fixed-overhead bytes (16-byte header + 4-byte trailer CRC).
pub const BLOCK_INDEX_FIXED_OVERHEAD: usize = 16 + 4;

/// Errors raised while reading or writing a block index sidecar.
#[derive(Debug, Error)]
pub enum BlockIndexError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Magic bytes at the start of the file don't match `BLOCK_INDEX_MAGIC`.
    #[error("invalid block-index magic: got {got:?}, expected {expected:?}")]
    InvalidMagic {
        /// Magic bytes read.
        got: [u8; 4],
        /// Expected magic.
        expected: [u8; 4],
    },

    /// Format version is newer than this build supports.
    #[error("unsupported block-index format_version {version} (this build supports up to {supported})")]
    UnsupportedFormatVersion {
        /// Version read.
        version: u8,
        /// Highest supported.
        supported: u8,
    },

    /// CRC32 over header + entries didn't match the stored trailer CRC.
    #[error("block-index CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC read from trailer.
        stored: u32,
        /// CRC computed over the parsed body.
        computed: u32,
    },

    /// File is shorter than the fixed header + trailer overhead.
    #[error("block-index too short: {len} bytes, need at least {needed}")]
    TooShort {
        /// File length.
        len: u64,
        /// Minimum bytes required.
        needed: u64,
    },

    /// File is truncated relative to the claimed entry_count.
    #[error("block-index truncated: expected at least {expected} bytes, got {got}")]
    Truncated {
        /// Bytes needed.
        expected: u64,
        /// Bytes actually present.
        got: u64,
    },
}

/// One sidecar entry: the SSTableKey of the first record in a block + the
/// byte offset of that record in the data section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockIndexEntry {
    /// First key in the block.
    pub key: SSTableKey,
    /// Byte offset of `key`'s record in the SSTable data section.
    pub offset: u64,
}

/// In-memory block index — sorted ascending by key.
#[derive(Debug, Clone)]
pub struct BlockIndex {
    /// Target block size used when this index was written.
    pub block_size: u32,
    /// Entries in key order.
    pub entries: Vec<BlockIndexEntry>,
}

impl BlockIndex {
    /// Find the byte offset to start linear scanning from when searching
    /// for `target`. Returns the offset of the largest entry whose key is
    /// `≤ target`. If `target` precedes the first entry, returns `Some(0)`
    /// (start from the beginning of the data section).
    ///
    /// `None` means the index is empty.
    #[must_use]
    pub fn seek_offset(&self, target: &SSTableKey) -> Option<u64> {
        if self.entries.is_empty() {
            return None;
        }
        // Binary search for the largest entry.key <= target.
        // partition_point returns the count of entries with key <= target.
        let idx = self.entries.partition_point(|e| &e.key <= target);
        if idx == 0 {
            // Target precedes the first entry — start at the very beginning.
            Some(0)
        } else {
            Some(self.entries[idx - 1].offset)
        }
    }

    /// Number of entries in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has zero entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Convert an SSTable `.ndb` path into the sibling `.idx` sidecar path.
#[must_use]
pub fn sidecar_path_for(sstable_path: &Path) -> PathBuf {
    let mut p = sstable_path.to_path_buf();
    p.set_extension(BLOCK_INDEX_EXTENSION);
    p
}

/// Writer-side helper. Tracks block boundaries while the SSTableWriter
/// streams records, and emits the sidecar via write-temp-then-rename.
///
/// The writer captures one entry every time the cumulative data bytes
/// cross a `block_size` boundary — specifically, the FIRST record of each
/// new block becomes an entry. The very first record is always an entry.
#[derive(Debug)]
pub struct BlockIndexWriter {
    block_size: u32,
    entries: Vec<BlockIndexEntry>,
    next_block_threshold: u64,
}

impl BlockIndexWriter {
    /// New writer with the default block size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_block_size(DEFAULT_BLOCK_SIZE)
    }

    /// New writer with a custom block size. Smaller blocks → bigger
    /// sidecar, fewer linear-scan bytes per lookup. Tradeoff knob.
    #[must_use]
    pub fn with_block_size(block_size: u32) -> Self {
        debug_assert!(block_size >= 64, "block_size too small to be useful");
        Self {
            block_size,
            entries: Vec::new(),
            next_block_threshold: 0,
        }
    }

    /// Called by the SSTableWriter BEFORE appending a record. `offset` is
    /// the byte position at which the record's first byte will be
    /// written. If the offset crosses a block boundary, this record's
    /// key becomes the next index entry.
    pub fn observe_record(&mut self, key: &SSTableKey, offset: u64) {
        if offset >= self.next_block_threshold {
            self.entries.push(BlockIndexEntry {
                key: key.clone(),
                offset,
            });
            // Advance threshold to the next block boundary AFTER offset.
            let bs = u64::from(self.block_size);
            self.next_block_threshold = ((offset / bs) + 1) * bs;
        }
    }

    /// Number of entries that will be in the final sidecar.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Write the sidecar atomically. Path is `<sstable_path>.idx`.
    /// `tmp_path` is the temp filename to use during write (caller picks).
    pub fn finish(self, sidecar_path: &Path) -> Result<(), BlockIndexError> {
        let tmp = tmp_sibling(sidecar_path);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        let mut w = BufWriter::new(file);
        let bytes = encode_block_index(self.block_size, &self.entries);
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

impl Default for BlockIndexWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Load a sidecar from disk. Returns `None` if the file doesn't exist
/// (caller falls back to linear scan).
pub fn load_sidecar(path: &Path) -> Result<Option<BlockIndex>, BlockIndexError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(BlockIndexError::Io(e)),
    };
    decode_block_index(&bytes).map(Some)
}

// ---------------------------------------------------------------------------
// Encode / decode
// ---------------------------------------------------------------------------

fn encode_block_index(block_size: u32, entries: &[BlockIndexEntry]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(BLOCK_INDEX_FIXED_OVERHEAD + entries.len() * (1 + 2 + 16 + 8));
    // Header.
    out.extend_from_slice(BLOCK_INDEX_MAGIC);
    out.push(BLOCK_INDEX_FORMAT_VERSION);
    out.extend_from_slice(&[0u8; 3]); // reserved
    out.extend_from_slice(&block_size.to_le_bytes());
    let entry_count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&entry_count.to_le_bytes());
    // Entries.
    for e in entries {
        out.push(e.key.kind);
        let key_len = u16::try_from(e.key.primary.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(&e.key.primary);
        out.extend_from_slice(&e.offset.to_le_bytes());
    }
    // Trailer CRC.
    let mut h = Hasher::new();
    h.update(&out);
    out.extend_from_slice(&h.finalize().to_le_bytes());
    out
}

fn decode_block_index(bytes: &[u8]) -> Result<BlockIndex, BlockIndexError> {
    let total = bytes.len() as u64;
    if total < BLOCK_INDEX_FIXED_OVERHEAD as u64 {
        return Err(BlockIndexError::TooShort {
            len: total,
            needed: BLOCK_INDEX_FIXED_OVERHEAD as u64,
        });
    }

    // Header.
    let magic = &bytes[0..4];
    if magic != BLOCK_INDEX_MAGIC {
        let mut got = [0u8; 4];
        got.copy_from_slice(magic);
        return Err(BlockIndexError::InvalidMagic {
            got,
            expected: *BLOCK_INDEX_MAGIC,
        });
    }
    let format_version = bytes[4];
    if format_version > BLOCK_INDEX_FORMAT_VERSION_MAX_SUPPORTED {
        return Err(BlockIndexError::UnsupportedFormatVersion {
            version: format_version,
            supported: BLOCK_INDEX_FORMAT_VERSION_MAX_SUPPORTED,
        });
    }
    // bytes[5..8] reserved.
    let block_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let entry_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;

    // Trailer CRC.
    let trailer_off = bytes.len() - 4;
    let stored_crc = u32::from_le_bytes(bytes[trailer_off..].try_into().unwrap());
    let mut h = Hasher::new();
    h.update(&bytes[..trailer_off]);
    let computed = h.finalize();
    if stored_crc != computed {
        return Err(BlockIndexError::CrcMismatch {
            stored: stored_crc,
            computed,
        });
    }

    // Entries.
    let mut pos = 16;
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        if pos + 3 > trailer_off {
            return Err(BlockIndexError::Truncated {
                expected: (pos + 3) as u64,
                got: trailer_off as u64,
            });
        }
        let kind = bytes[pos];
        let key_len = u16::from_le_bytes(bytes[pos + 1..pos + 3].try_into().unwrap()) as usize;
        pos += 3;
        if pos + key_len + 8 > trailer_off {
            return Err(BlockIndexError::Truncated {
                expected: (pos + key_len + 8) as u64,
                got: trailer_off as u64,
            });
        }
        let primary = bytes[pos..pos + key_len].to_vec();
        pos += key_len;
        let offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        entries.push(BlockIndexEntry {
            key: SSTableKey { kind, primary },
            offset,
        });
    }

    Ok(BlockIndex {
        block_size,
        entries,
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

/// Test that `path` could plausibly host a sidecar (ends in `.<sstable>`).
/// Used by tooling; not on the hot path.
#[must_use]
pub fn is_sstable_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == SSTABLE_EXTENSION)
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

    #[test]
    fn empty_index_seek_returns_none() {
        let idx = BlockIndex {
            block_size: DEFAULT_BLOCK_SIZE,
            entries: vec![],
        };
        assert_eq!(idx.seek_offset(&key(1, b"abc")), None);
    }

    #[test]
    fn seek_finds_largest_le_entry() {
        let idx = BlockIndex {
            block_size: 1024,
            entries: vec![
                BlockIndexEntry {
                    key: key(1, b"aaa"),
                    offset: 0,
                },
                BlockIndexEntry {
                    key: key(1, b"ccc"),
                    offset: 1024,
                },
                BlockIndexEntry {
                    key: key(1, b"eee"),
                    offset: 2048,
                },
            ],
        };
        // Exact match → that entry.
        assert_eq!(idx.seek_offset(&key(1, b"aaa")), Some(0));
        assert_eq!(idx.seek_offset(&key(1, b"ccc")), Some(1024));
        // Between → largest ≤.
        assert_eq!(idx.seek_offset(&key(1, b"bbb")), Some(0));
        assert_eq!(idx.seek_offset(&key(1, b"ddd")), Some(1024));
        // After last → last entry.
        assert_eq!(idx.seek_offset(&key(1, b"zzz")), Some(2048));
        // Before first → start of file.
        assert_eq!(idx.seek_offset(&key(0, b"\x00")), Some(0));
    }

    #[test]
    fn writer_records_first_record_then_block_boundaries() {
        let mut w = BlockIndexWriter::with_block_size(100);
        // Pretend each "record" is 30 bytes. Offsets: 0, 30, 60, 90,
        // 120, 150, 180, 210, 240.
        // Expected entries at offsets that cross multiples of 100:
        //   0 (first), 120 (crosses 100), 210 (crosses 200), 300 (crosses 300)...
        for i in 0u64..9 {
            let offset = i * 30;
            w.observe_record(&key(1, &[u8::try_from(i).unwrap()]), offset);
        }
        // Entries at offsets: 0, 120, 210, but NOT 240 since 240 < 300.
        // Specifically:
        //  - 0 first
        //  - 30 < 100 → skip
        //  - 60 < 100 → skip
        //  - 90 < 100 → skip
        //  - 120 ≥ 100 → entry; next threshold becomes 200
        //  - 150 < 200 → skip
        //  - 180 < 200 → skip
        //  - 210 ≥ 200 → entry; next threshold 300
        //  - 240 < 300 → skip
        assert_eq!(w.entry_count(), 3);
        let entries = w.entries;
        assert_eq!(entries[0].offset, 0);
        assert_eq!(entries[1].offset, 120);
        assert_eq!(entries[2].offset, 210);
    }

    #[test]
    fn round_trip_via_encode_decode() {
        let entries = vec![
            BlockIndexEntry {
                key: key(1, b"alice"),
                offset: 0,
            },
            BlockIndexEntry {
                key: key(2, b"bob"),
                offset: 4096,
            },
            BlockIndexEntry {
                key: key(4, &[0, 0, 0, 7]),
                offset: 8192,
            },
        ];
        let bytes = encode_block_index(4096, &entries);
        let idx = decode_block_index(&bytes).unwrap();
        assert_eq!(idx.block_size, 4096);
        assert_eq!(idx.entries, entries);
    }

    #[test]
    fn corrupted_crc_rejected() {
        let entries = vec![BlockIndexEntry {
            key: key(1, b"a"),
            offset: 0,
        }];
        let mut bytes = encode_block_index(4096, &entries);
        // Flip a byte in the CRC.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        match decode_block_index(&bytes) {
            Err(BlockIndexError::CrcMismatch { .. }) => {}
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn corrupted_magic_rejected() {
        let entries = vec![];
        let mut bytes = encode_block_index(4096, &entries);
        bytes[0] = b'X';
        match decode_block_index(&bytes) {
            Err(BlockIndexError::InvalidMagic { .. }) => {}
            other => panic!("expected InvalidMagic, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_format_version_rejected() {
        let entries = vec![];
        let mut bytes = encode_block_index(4096, &entries);
        bytes[4] = 99;
        // CRC will also fail; the format check runs first.
        match decode_block_index(&bytes) {
            Err(BlockIndexError::UnsupportedFormatVersion { .. }) => {}
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    #[test]
    fn truncated_rejected() {
        let entries = vec![BlockIndexEntry {
            key: key(1, b"abc"),
            offset: 0,
        }];
        let bytes = encode_block_index(4096, &entries);
        let truncated = &bytes[..bytes.len() - 10];
        match decode_block_index(truncated) {
            Err(
                BlockIndexError::CrcMismatch { .. }
                | BlockIndexError::Truncated { .. }
                | BlockIndexError::TooShort { .. },
            ) => {}
            other => panic!("expected Truncated or CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn sidecar_path_for_swaps_extension() {
        let p = Path::new("/tmp/db/000042.ndb");
        assert_eq!(sidecar_path_for(p), Path::new("/tmp/db/000042.idx"));
    }

    #[test]
    fn load_sidecar_returns_none_when_missing() {
        let dir = std::env::temp_dir().join(format!("ndb-blockidx-missing-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nope.idx");
        assert!(load_sidecar(&p).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writer_finish_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "ndb-blockidx-rt-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = BlockIndexWriter::with_block_size(1024);
        w.observe_record(&key(1, b"first"), 0);
        w.observe_record(&key(1, b"second"), 1500);
        w.observe_record(&key(1, b"third"), 2700);
        let p = dir.join("000001.idx");
        w.finish(&p).unwrap();
        let idx = load_sidecar(&p).unwrap().unwrap();
        assert_eq!(idx.block_size, 1024);
        assert_eq!(idx.entries.len(), 3);
        assert_eq!(idx.entries[1].key.primary, b"second");
        // Cleanup
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
