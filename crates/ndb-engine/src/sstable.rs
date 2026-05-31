//! Sorted String Table — the immutable `.ndb` file (§11.5).
#![allow(clippy::doc_markdown)] // "SSTable" is a well-known database term used liberally here.
//!
//! An SSTable is a sorted, append-only sequence of records sharing the same
//! envelope format as the WAL, followed by a fixed-size footer that lets a
//! reader validate the file without scanning it. Each `.ndb` file is the
//! product of one memtable flush or one compaction step; once written it is
//! never mutated again (compaction produces new files and atomically swaps
//! the MANIFEST).
//!
//! v1 decisions, made here in the introducing commit:
//!
//! - **Records first, footer last.** Scan-friendly recovery: a reader that
//!   starts at byte 0 can stream records sequentially without consulting
//!   the footer. The footer is the validator and the addressable-data
//!   handle, not a parser dispatch table.
//! - **One sorted run per file.** The memtable (not yet implemented) is
//!   responsible for sort order; the SSTable writer trusts the caller and
//!   enforces non-decreasing primary key as a debug-time invariant only.
//! - **No block index in v1.** The footer carries `record_count` and
//!   `data_size`; lookups are linear scans. A `<seq>.idx` sidecar file
//!   (§11.5) lands in a later commit that introduces block boundaries.
//! - **Atomic publish via `write-temp + fsync + rename + fsync_dir`.** A
//!   crashed writer never leaves a half-finished `<seq>.ndb` visible to
//!   the MANIFEST; either the rename completes (file is fully there) or
//!   it does not (file is absent).
//! - **Sort key: `(record_kind, primary_id)`.** Closes one of §11.4's open
//!   sub-questions. Primary key is the record's unique identifier:
//!   `entity_id` for entities, `hyperedge_id` for hyperedges, `target_id`
//!   for tombstones, dictionary `id` (`u32` widened to big-endian-bytes
//!   for sort) for type/role/property records. Records inside one SSTable
//!   are sorted by `(kind_byte, primary_key_bytes)` ascending. Other
//!   orderings (by `tx_id_assert`, by foreign-key adjacency) are the
//!   province of secondary indexes, not the primary store.
//!
//! Footer layout (32 bytes, fixed):
//!
//! ```text
//! ┌────────────────┬─────────┬───────┬──────────┬──────────────┬─────────────┬────────────┐
//! │ magic (8B)     │ format  │ flags │ reserved │ record_count │  data_size  │ footer_crc │
//! │ "NDBSST01"     │  u8     │  u8   │  [u8;2]  │     u64      │     u64     │    u32     │
//! └────────────────┴─────────┴───────┴──────────┴──────────────┴─────────────┴────────────┘
//! ```
//!
//! `data_size` is the byte count from the start of the file up to (but not
//! including) the footer. `footer_crc` covers every footer byte except
//! itself. Per-record CRCs cover the data section, so no whole-file CRC is
//! needed.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::encryption::{Cipher, DEFAULT_CHUNK_SIZE, EncryptedFile};

use crc32fast::Hasher;
use thiserror::Error;

use crate::codec::{write_u8, write_u32, write_u64};
use crate::error::DecodeError;
#[cfg(test)]
use crate::record::peek_record_size;
use crate::record::{Record, RecordKind};

/// Canonical extension for SSTable files (§11.5).
pub const SSTABLE_EXTENSION: &str = "ndb";

/// Magic bytes at the start of every SSTable footer. Distinguishes
/// nDB SSTables from foreign files with the same `.ndb` extension.
pub const SSTABLE_MAGIC: &[u8; 8] = b"NDBSST01";

/// Current SSTable on-disk format version this build emits.
pub const SSTABLE_FORMAT_VERSION: u8 = 1;

/// Highest SSTable `format_version` this build can read. v1 = uncompressed
/// record stream; v2 = block-compressed (see [`crate::compression`]). A v2
/// writer is opt-in; v2 readers read both, older readers reject v2.
pub const SSTABLE_FORMAT_VERSION_MAX_SUPPORTED: u8 = 2;

/// `format_version` written for block-compressed SSTables.
pub const SSTABLE_FORMAT_VERSION_COMPRESSED: u8 = 2;

/// Size of the fixed footer in bytes.
pub const SSTABLE_FOOTER_SIZE: usize = 32;

const FOOTER_MAGIC_LEN: usize = 8;
const FOOTER_FMT_LEN: usize = 1;
const FOOTER_FLAGS_LEN: usize = 1;
const FOOTER_RESERVED_LEN: usize = 2;
const FOOTER_COUNT_LEN: usize = 8;
const FOOTER_DATA_SIZE_LEN: usize = 8;
const FOOTER_CRC_LEN: usize = 4;

const _: () = {
    assert!(
        FOOTER_MAGIC_LEN
            + FOOTER_FMT_LEN
            + FOOTER_FLAGS_LEN
            + FOOTER_RESERVED_LEN
            + FOOTER_COUNT_LEN
            + FOOTER_DATA_SIZE_LEN
            + FOOTER_CRC_LEN
            == SSTABLE_FOOTER_SIZE,
        "SSTABLE_FOOTER_SIZE inconsistent with field sizes"
    );
};

/// Parsed SSTable footer — `record_count`, `data_size`, and format-version
/// info. Used by readers to set up bounded iteration without scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SSTableFooter {
    /// Number of records in the data section.
    pub record_count: u64,
    /// Bytes from start of file to start of footer.
    pub data_size: u64,
    /// On-disk format version.
    pub format_version: u8,
    /// Bit flags; reserved for future use, must be 0 in v1.
    pub flags: u8,
}

/// Sort key used to order records inside an SSTable. Comparison is
/// lexicographic on `(kind_byte, primary_key_bytes)`.
///
/// The `primary_key_bytes` representation is fixed and big-endian where
/// natively numeric so byte-order comparison matches semantic comparison:
///
/// - Entity, HyperEdge, Tombstone records: 16-byte UUID raw bytes.
///   `Uuid::as_bytes` is big-endian per RFC 9562 §5, so UUID v7 records
///   sort by creation time then by random tail — which is what we want.
/// - TypeName, RoleName, PropertyKey records: `u32` id encoded big-endian.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SSTableKey {
    /// `RecordKind` discriminant byte.
    pub kind: u8,
    /// Primary-identifier bytes (16 for UUID records, 4 for dictionary).
    pub primary: Vec<u8>,
}

impl SSTableKey {
    /// Derive the sort key for a [`Record`].
    #[must_use]
    pub fn for_record(r: &Record) -> Self {
        match r {
            Record::Entity(e) => Self {
                kind: RecordKind::Entity.as_byte(),
                primary: e.entity_id.as_bytes().to_vec(),
            },
            Record::HyperEdge(h) => Self {
                kind: RecordKind::HyperEdge.as_byte(),
                primary: h.hyperedge_id.as_bytes().to_vec(),
            },
            Record::Tombstone(t) => Self {
                kind: RecordKind::Tombstone.as_byte(),
                primary: t.target_id.as_bytes().to_vec(),
            },
            Record::TypeName(d) => Self {
                kind: RecordKind::TypeName.as_byte(),
                primary: d.id.get().to_be_bytes().to_vec(),
            },
            Record::RoleName(d) => Self {
                kind: RecordKind::RoleName.as_byte(),
                primary: d.id.get().to_be_bytes().to_vec(),
            },
            Record::PropertyKey(d) => Self {
                kind: RecordKind::PropertyKey.as_byte(),
                primary: d.id.get().to_be_bytes().to_vec(),
            },
            Record::TxTimestamp(t) => Self {
                kind: RecordKind::TxTimestamp.as_byte(),
                primary: t.tx_id.get().to_be_bytes().to_vec(),
            },
            Record::RetentionPolicy(r) => Self {
                kind: RecordKind::RetentionPolicy.as_byte(),
                primary: r.type_id.get().to_be_bytes().to_vec(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised while reading or writing an SSTable.
#[derive(Debug, Error)]
pub enum SSTableError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A compressed (v2) data block failed to decode (codec/CRC/length).
    #[error("SSTable block decode failed: {0}")]
    Compression(#[from] crate::compression::CompressionError),

    /// A record in the data section failed to decode (envelope, CRC,
    /// sentinel).
    #[error("SSTable record at offset {offset} failed to decode: {source}")]
    Decode {
        /// Byte offset of the offending record's first byte.
        offset: u64,
        /// Underlying decode error.
        #[source]
        source: DecodeError,
    },

    /// Magic bytes at the start of the footer don't match `SSTABLE_MAGIC`.
    #[error("invalid SSTable magic: got {got:?}, expected {expected:?}")]
    InvalidMagic {
        /// Magic bytes read from disk.
        got: [u8; 8],
        /// Expected magic.
        expected: [u8; 8],
    },

    /// Footer CRC didn't match the computed CRC.
    #[error("SSTable footer CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    FooterCrcMismatch {
        /// CRC read from the footer.
        stored: u32,
        /// CRC computed over the footer body.
        computed: u32,
    },

    /// Footer `format_version` is newer than this build supports.
    #[error("unsupported SSTable format_version {version} (this build supports up to {supported})")]
    UnsupportedFormatVersion {
        /// Format version byte read from disk.
        version: u8,
        /// Highest version this build can read.
        supported: u8,
    },

    /// File is too short to contain a footer.
    #[error("SSTable too short: {len} bytes, need at least {needed}")]
    TooShort {
        /// File length.
        len: u64,
        /// Minimum bytes required.
        needed: u64,
    },

    /// Record count from the footer doesn't match the number of records
    /// actually decoded from the data section.
    #[error("SSTable record_count mismatch: footer says {expected}, found {found} records on scan")]
    RecordCountMismatch {
        /// Count from the footer.
        expected: u64,
        /// Count from scanning.
        found: u64,
    },

    /// Writer was given records out of sort order. Only fires in debug builds
    /// (sort order is the caller's contract in v1).
    #[error("SSTable records appended out of sort order at index {index}")]
    OutOfOrder {
        /// Index of the offending record.
        index: u64,
    },
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Build an SSTable atomically.
///
/// Usage:
///
/// ```ignore
/// let mut w = SSTableWriter::create(path)?;
/// for r in sorted_records { w.append(&r)?; }
/// let footer = w.finish()?;
/// ```
///
/// The file is written to `<path>.tmp` and renamed to `<path>` inside
/// [`finish`](Self::finish). The parent directory is `fsync`'d after the
/// rename so the link change is also durable.
///
/// When created via [`SSTableWriter::create_with_cipher`], the on-disk
/// file is AES-GCM-chunk-encrypted (the same envelope used by the WAL).
/// The footer + per-record CRC live inside the plaintext stream — readers
/// decrypt first, then parse — so `data_size` and record offsets in the
/// block-index sidecar are still PLAINTEXT byte positions.
#[derive(Debug)]
pub struct SSTableWriter {
    final_path: PathBuf,
    tmp_path: PathBuf,
    sink: SSTableSink,
    record_count: u64,
    bytes_written: u64,
    last_key: Option<SSTableKey>,
    /// Builds the `<seq>.idx` sidecar (v2.0+). One entry per
    /// `BlockIndexWriter` block boundary; the very first record is
    /// always indexed so seek_offset(target) before the first key
    /// returns 0.
    block_index: crate::block_index::BlockIndexWriter,
    /// Builds the `<seq>.bloom` sidecar (v1.3+). Records every appended
    /// key so a reader can prove a key absent and skip the table entirely.
    bloom: crate::bloom::BloomWriter,
    /// Block compression codec. [`Codec::Stored`](crate::compression::Codec)
    /// = uncompressed v1 format (records written straight to the sink). Any
    /// other codec = v2: records are buffered into `block_buf`, compressed per
    /// block, and the footer records `format_version = 2`.
    codec: crate::compression::Codec,
    /// v2 only: records accumulate here until a block is sealed + compressed.
    block_buf: Vec<u8>,
    /// v2 only: cumulative UNCOMPRESSED byte position — the offset space the
    /// block index + the reader's reconstructed stream use (so block-index
    /// offsets are codec-independent).
    uncompressed_pos: u64,
}

/// Either a plaintext `BufWriter<File>` or a chunked-AEAD `EncryptedFile<File>`.
/// Chosen at create time based on whether the engine has a cipher loaded.
///
/// `EncryptedFile` is dramatically larger than `BufWriter<File>` (it owns
/// the AES-GCM state + a chunk-sized in-memory buffer), so the variants
/// differ in size by several KiB. We accept that — boxing the encrypted
/// variant adds an indirection to the hot per-record write path with no
/// measurable benefit. Clippy's `large_enum_variant` lint is opt-out
/// here on purpose.
#[allow(clippy::large_enum_variant)]
enum SSTableSink {
    Plain(BufWriter<File>),
    Encrypted(EncryptedFile<File>),
}

impl std::fmt::Debug for SSTableSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("SSTableSink::Plain"),
            Self::Encrypted(_) => f.write_str("SSTableSink::Encrypted"),
        }
    }
}

impl SSTableSink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(w) => w.write_all(bytes),
            Self::Encrypted(w) => w.write_all(bytes),
        }
    }

    /// Drive the sink to "all bytes plaintext-flushed, file descriptor
    /// surfaced". For the encrypted path this seals the final chunk via
    /// [`EncryptedFile::finish`]; for the plain path it just unwraps
    /// the `BufWriter`.
    fn into_file(self) -> io::Result<File> {
        match self {
            Self::Plain(w) => w
                .into_inner()
                .map_err(|e| io::Error::other(format!("BufWriter into_inner failed: {e}"))),
            Self::Encrypted(w) => w
                .finish()
                .map_err(|e| io::Error::other(format!("encrypted SSTable finish: {e}"))),
        }
    }
}

impl SSTableWriter {
    /// Open a temp file alongside `final_path` and prepare to receive records.
    ///
    /// Equivalent to `create_with_cipher(path, None)` — writes plaintext.
    pub fn create<P: AsRef<Path>>(final_path: P) -> Result<Self, SSTableError> {
        Self::create_with_cipher(final_path, None)
    }

    /// Like [`SSTableWriter::create`] but optionally wraps the on-disk
    /// file with [`EncryptedFile`]. Block-index entry offsets and the
    /// footer's `data_size` field remain PLAINTEXT — the reader's
    /// invariants don't change.
    pub fn create_with_cipher<P: AsRef<Path>>(
        final_path: P,
        cipher: Option<Cipher>,
    ) -> Result<Self, SSTableError> {
        Self::create_with_cipher_codec(final_path, cipher, crate::compression::Codec::Stored)
    }

    /// Like [`SSTableWriter::create_with_cipher`] but with an explicit block
    /// compression [`Codec`](crate::compression::Codec). `Codec::Stored`
    /// writes the v1 uncompressed format (byte-for-byte the historical
    /// output); any other codec writes the v2 block-compressed format. Block
    /// index offsets + the bloom are codec-independent (they use uncompressed
    /// positions / keys), so the sidecars are identical either way.
    pub fn create_with_cipher_codec<P: AsRef<Path>>(
        final_path: P,
        cipher: Option<Cipher>,
        codec: crate::compression::Codec,
    ) -> Result<Self, SSTableError> {
        let final_path = final_path.as_ref().to_path_buf();
        let tmp_path = tmp_sibling(&final_path);
        // O_TRUNC to clean up a crashed prior write attempt.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        let sink = match cipher {
            None => SSTableSink::Plain(BufWriter::new(file)),
            Some(c) => SSTableSink::Encrypted(
                EncryptedFile::create(file, c, DEFAULT_CHUNK_SIZE)
                    .map_err(|e| io::Error::other(format!("encrypted SSTable create: {e}")))?,
            ),
        };
        Ok(Self {
            final_path,
            tmp_path,
            sink,
            record_count: 0,
            bytes_written: 0,
            last_key: None,
            block_index: crate::block_index::BlockIndexWriter::new(),
            bloom: crate::bloom::BloomWriter::new(),
            codec,
            block_buf: Vec::new(),
            uncompressed_pos: 0,
        })
    }

    /// Append one record. In debug builds asserts that successive keys are
    /// non-decreasing; in release builds the caller is trusted.
    pub fn append(&mut self, record: &Record) -> Result<(), SSTableError> {
        let key = SSTableKey::for_record(record);
        if let Some(prev) = &self.last_key
            && key < *prev
        {
            return Err(SSTableError::OutOfOrder {
                index: self.record_count,
            });
        }
        // Observe BEFORE writing, against the UNCOMPRESSED position — the
        // offset space the reader reconstructs and the block index seeks in.
        // The very first record is always indexed (offset 0); subsequent
        // records become entries only at block-size boundaries.
        self.block_index.observe_record(&key, self.uncompressed_pos);
        self.bloom.observe_key(&key);
        let mut buf = Vec::with_capacity(128);
        record
            .encode(&mut buf)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("encode failed: {e}")))?;
        match self.codec {
            crate::compression::Codec::Stored => {
                // v1: stream the record straight to the sink.
                self.sink.write_all(&buf)?;
                self.bytes_written += buf.len() as u64;
            }
            _ => {
                // v2: accumulate, sealing a compressed block at the threshold.
                self.block_buf.extend_from_slice(&buf);
                if self.block_buf.len() >= crate::compression::DEFAULT_BLOCK_BYTES {
                    self.seal_block()?;
                }
            }
        }
        self.uncompressed_pos += buf.len() as u64;
        self.record_count += 1;
        self.last_key = Some(key);
        Ok(())
    }

    /// Append already-encoded record bytes. Skips the sort-order check
    /// (caller already encoded the record, so they own the contract).
    ///
    /// v1 (`Codec::Stored`) only — this raw escape hatch bypasses block
    /// buffering. Unused by the engine.
    pub fn append_raw(&mut self, bytes: &[u8]) -> Result<(), SSTableError> {
        debug_assert!(
            matches!(self.codec, crate::compression::Codec::Stored),
            "append_raw is not supported for compressed SSTables"
        );
        self.sink.write_all(bytes)?;
        self.bytes_written += bytes.len() as u64;
        self.uncompressed_pos += bytes.len() as u64;
        self.record_count += 1;
        Ok(())
    }

    /// v2: compress the accumulated `block_buf` into one block, write it to
    /// the sink, and advance the COMPRESSED byte counter. No-op on an empty
    /// buffer.
    fn seal_block(&mut self) -> Result<(), SSTableError> {
        if self.block_buf.is_empty() {
            return Ok(());
        }
        let block = crate::compression::encode_block(&self.block_buf, self.codec);
        self.sink.write_all(&block)?;
        self.bytes_written += block.len() as u64;
        self.block_buf.clear();
        Ok(())
    }

    /// Write the footer, fsync the temp file, rename onto `final_path`,
    /// then write the block-index sidecar (also write-temp-then-rename),
    /// and finally fsync the parent directory.
    ///
    /// Ordering note: the main `.ndb` lands first so a crash between the
    /// two renames produces a v1.3-compatible state — the reader will
    /// find a valid SSTable with no sidecar and gracefully fall back to
    /// linear scan.
    pub fn finish(mut self) -> Result<SSTableFooter, SSTableError> {
        // v2: flush the final partial block before the footer.
        if !matches!(self.codec, crate::compression::Codec::Stored) {
            self.seal_block()?;
        }
        let format_version = if matches!(self.codec, crate::compression::Codec::Stored) {
            SSTABLE_FORMAT_VERSION
        } else {
            SSTABLE_FORMAT_VERSION_COMPRESSED
        };
        let footer = SSTableFooter {
            record_count: self.record_count,
            data_size: self.bytes_written,
            format_version,
            flags: 0,
        };
        let footer_bytes = encode_footer(&footer);
        self.sink.write_all(&footer_bytes)?;
        let f = self.sink.into_file()?;
        f.sync_data()?;
        std::fs::rename(&self.tmp_path, &self.final_path)?;
        // Sidecar is best-effort but should normally succeed. If it fails
        // (out of space, permission), the SSTable is still valid and the
        // reader falls back to linear scan.
        let sidecar = crate::block_index::sidecar_path_for(&self.final_path);
        self.block_index.finish(&sidecar).map_err(|e| {
            io::Error::other(format!("block-index sidecar write failed: {e}"))
        })?;
        // Bloom sidecar — only emit it when it provably covers EVERY record.
        // `append` observes each key; `append_raw` (raw escape hatch) does
        // not. Writing a partial bloom would produce false negatives, so if
        // the observed key count ever disagrees with `record_count` we skip
        // the sidecar entirely and the reader falls back to scanning.
        if self.bloom.key_count() as u64 == self.record_count {
            let bloom_path = crate::bloom::sidecar_path_for(&self.final_path);
            self.bloom.finish(&bloom_path).map_err(|e| {
                io::Error::other(format!("bloom sidecar write failed: {e}"))
            })?;
        }
        // fsync the parent directory so the renames are durable.
        if let Some(parent) = self.final_path.parent() {
            fsync_dir(parent)?;
        }
        Ok(footer)
    }

    /// Abort the build: close the temp file and remove it. Idempotent.
    pub fn abort(self) -> io::Result<()> {
        drop(self.sink);
        match std::fs::remove_file(&self.tmp_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Bytes written to the data section so far (excluding the footer).
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Records written so far.
    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.record_count
    }
}

fn tmp_sibling(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    p.with_file_name(name)
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()
}

// ---------------------------------------------------------------------------
// Footer encode + decode
// ---------------------------------------------------------------------------

fn encode_footer(f: &SSTableFooter) -> [u8; SSTABLE_FOOTER_SIZE] {
    let mut buf = Vec::with_capacity(SSTABLE_FOOTER_SIZE);
    buf.extend_from_slice(SSTABLE_MAGIC);
    write_u8(&mut buf, f.format_version);
    write_u8(&mut buf, f.flags);
    buf.extend_from_slice(&[0u8; FOOTER_RESERVED_LEN]);
    write_u64(&mut buf, f.record_count);
    write_u64(&mut buf, f.data_size);
    let mut h = Hasher::new();
    h.update(&buf);
    write_u32(&mut buf, h.finalize());
    debug_assert_eq!(buf.len(), SSTABLE_FOOTER_SIZE);
    let mut out = [0u8; SSTABLE_FOOTER_SIZE];
    out.copy_from_slice(&buf);
    out
}

fn decode_footer(bytes: &[u8; SSTABLE_FOOTER_SIZE]) -> Result<SSTableFooter, SSTableError> {
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&bytes[0..8]);
    if &magic != SSTABLE_MAGIC {
        return Err(SSTableError::InvalidMagic {
            got: magic,
            expected: *SSTABLE_MAGIC,
        });
    }
    let format_version = bytes[8];
    let flags = bytes[9];
    if format_version > SSTABLE_FORMAT_VERSION_MAX_SUPPORTED {
        return Err(SSTableError::UnsupportedFormatVersion {
            version: format_version,
            supported: SSTABLE_FORMAT_VERSION_MAX_SUPPORTED,
        });
    }
    // bytes[10..12] reserved
    let record_count = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    let data_size = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
    let stored_crc = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    let mut h = Hasher::new();
    h.update(&bytes[..28]);
    let computed = h.finalize();
    if stored_crc != computed {
        return Err(SSTableError::FooterCrcMismatch {
            stored: stored_crc,
            computed,
        });
    }
    Ok(SSTableFooter {
        record_count,
        data_size,
        format_version,
        flags,
    })
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Random-access reader for an SSTable file.
///
/// `open()` validates the footer up front; iteration streams records one at
/// a time so large tables don't load entirely into memory.
///
/// Two backings:
/// - **Plain SSTables** are memory-mapped; reads are zero-copy.
/// - **Encrypted SSTables** are decrypted once at open time into a heap
///   buffer; reads come from the buffer. The plaintext byte layout
///   (records + footer + per-record CRCs + block-index offsets) is
///   identical to the plain case, so the iter / find / footer logic is
///   shared.
#[derive(Debug)]
pub struct SSTableReader {
    path: PathBuf,
    backing: SSTableBacking,
    /// Length of the PLAINTEXT byte stream. For plain SSTables this
    /// equals the file size on disk; for encrypted SSTables this is the
    /// decrypted size (smaller than disk size because chunks carry
    /// nonce + tag overhead).
    file_len: u64,
    footer: SSTableFooter,
    /// Length of the iterable (uncompressed) record stream in
    /// `backing.as_bytes()`. For v1 SSTables this equals `footer.data_size`.
    /// For v2 (block-compressed) SSTables the backing holds the RECONSTRUCTED
    /// uncompressed stream (decompressed at open), so this is its decompressed
    /// length — and every downstream path (block index offsets, iter, find)
    /// operates on uncompressed positions exactly as for v1.
    data_len: u64,
    /// Optional block-index sidecar. Present for SSTables written by
    /// v2.0+ writers; absent for v1.3-era SSTables. When present,
    /// `find()` binary-searches it to bound the linear-scan range.
    block_index: Option<crate::block_index::BlockIndex>,
    /// Optional bloom sidecar. Present for SSTables written by v1.3+
    /// writers. When present and it reports a key absent, `find` /
    /// `find_all` return immediately without touching the data section.
    bloom: Option<crate::bloom::BloomFilter>,
}

/// Backing storage for an [`SSTableReader`]. Plain files are mmap'd
/// (zero-copy); encrypted files are decrypted to a heap buffer.
enum SSTableBacking {
    Mmap {
        mmap: memmap2::Mmap,
        /// Keeps the FD alive for the mmap lifetime. We never write.
        _file: File,
    },
    Decrypted(Box<[u8]>),
    /// Reconstructed uncompressed record stream for a v2 (block-compressed)
    /// SSTable, decompressed once at open. Like `Decrypted` it is owned heap
    /// bytes; the downstream record layout is identical to a plain SSTable.
    Decompressed(Box<[u8]>),
}

impl std::fmt::Debug for SSTableBacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mmap { .. } => f.write_str("SSTableBacking::Mmap"),
            Self::Decrypted(_) => f.write_str("SSTableBacking::Decrypted"),
            Self::Decompressed(_) => f.write_str("SSTableBacking::Decompressed"),
        }
    }
}

impl SSTableBacking {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Mmap { mmap, .. } => mmap,
            Self::Decrypted(b) | Self::Decompressed(b) => b,
        }
    }
}

impl SSTableReader {
    /// Open `path`, validate the footer, mmap the file, and return a
    /// handle ready for iteration / lookup.
    ///
    /// Equivalent to `open_with_cipher(path, None)` — reads plaintext.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, SSTableError> {
        Self::open_with_cipher(path, None)
    }

    /// Open `path` for read. When `cipher` is `Some`, the file is
    /// decrypted via [`EncryptedFile`] into a heap buffer at open time
    /// and subsequent reads use that buffer instead of mmap.
    pub fn open_with_cipher<P: AsRef<Path>>(
        path: P,
        cipher: Option<Cipher>,
    ) -> Result<Self, SSTableError> {
        let path = path.as_ref().to_path_buf();
        let needed = SSTABLE_FOOTER_SIZE as u64;
        let (mut backing, file_len) = match cipher {
            None => {
                let file = File::open(&path)?;
                let file_len = file.metadata()?.len();
                if file_len < needed {
                    return Err(SSTableError::TooShort {
                        len: file_len,
                        needed,
                    });
                }
                // SAFETY: We open the file read-only and rely on the engine's
                // append-only + write-temp-then-rename invariants to guarantee
                // the file content doesn't mutate under us. SSTable files are
                // immutable after publish; the only modification is deletion
                // (which leaves an existing mmap valid via the inode lifetime).
                #[allow(unsafe_code)]
                let mmap = unsafe { memmap2::Mmap::map(&file)? };
                let mmap_len = mmap.len() as u64;
                if mmap_len != file_len {
                    return Err(SSTableError::TooShort {
                        len: mmap_len,
                        needed: file_len,
                    });
                }
                (
                    SSTableBacking::Mmap { mmap, _file: file },
                    file_len,
                )
            }
            Some(c) => {
                let file = File::open(&path)?;
                let mut enc = EncryptedFile::open(file, c)
                    .map_err(|e| io::Error::other(format!("encrypted SSTable open: {e}")))?;
                let mut buf = Vec::new();
                enc.read_to_end(&mut buf)?;
                let len = buf.len() as u64;
                if len < needed {
                    return Err(SSTableError::TooShort {
                        len,
                        needed,
                    });
                }
                (SSTableBacking::Decrypted(buf.into_boxed_slice()), len)
            }
        };
        let bytes = backing.as_bytes();
        let footer_off = usize::try_from(file_len - needed)
            .map_err(|_| SSTableError::TooShort { len: file_len, needed: usize::MAX as u64 })?;
        let mut footer_bytes = [0u8; SSTABLE_FOOTER_SIZE];
        footer_bytes.copy_from_slice(&bytes[footer_off..footer_off + SSTABLE_FOOTER_SIZE]);
        let footer = decode_footer(&footer_bytes)?;
        if footer.data_size + needed != file_len {
            // data_size in the footer disagrees with the file length — clear
            // sign of truncation or extra trailing bytes.
            return Err(SSTableError::TooShort {
                len: file_len,
                needed: footer.data_size + needed,
            });
        }

        // v2 (block-compressed): reconstruct the uncompressed record stream
        // once, here, so the entire downstream pipeline (block index, bloom,
        // iter, find) sees a plain record stream identical to v1. `data_len`
        // becomes the decompressed length. Composes with encryption: by this
        // point `backing` already holds decrypted bytes.
        let data_len = if footer.format_version >= SSTABLE_FORMAT_VERSION_COMPRESSED {
            let records = {
                let bytes = backing.as_bytes();
                let compressed_end = usize::try_from(footer.data_size).unwrap_or(usize::MAX);
                let compressed = &bytes[..compressed_end];
                let mut out = Vec::with_capacity(compressed.len() * 2);
                let mut p = 0usize;
                while p < compressed.len() {
                    let (block, consumed) = crate::compression::decode_block(&compressed[p..])?;
                    out.extend_from_slice(&block);
                    p += consumed;
                }
                out
            };
            let len = records.len() as u64;
            backing = SSTableBacking::Decompressed(records.into_boxed_slice());
            len
        } else {
            footer.data_size
        };
        // Sidecar load is best-effort: missing → v1.3 SSTable, fall back
        // to linear scan. Corrupt → log + fall back (we can't bring down
        // the engine because of a bad index).
        let sidecar_path = crate::block_index::sidecar_path_for(&path);
        let block_index = match crate::block_index::load_sidecar(&sidecar_path) {
            Ok(Some(idx)) => Some(idx),
            Ok(None) => None,
            Err(e) => {
                eprintln!(
                    "ndb-engine: block-index sidecar {} corrupt ({}); falling back to linear scan",
                    sidecar_path.display(),
                    e,
                );
                None
            }
        };
        // Bloom sidecar load is best-effort too: missing → no skip, corrupt
        // → log + skip. A bad bloom can never cause a wrong answer (we only
        // ever use a `false` result to skip, and a corrupt/absent filter is
        // simply not consulted), so falling back is always safe.
        let bloom_path = crate::bloom::sidecar_path_for(&path);
        let bloom = match crate::bloom::load_sidecar(&bloom_path) {
            Ok(Some(b)) => Some(b),
            Ok(None) => None,
            Err(e) => {
                eprintln!(
                    "ndb-engine: bloom sidecar {} corrupt ({}); skipping membership filter",
                    bloom_path.display(),
                    e,
                );
                None
            }
        };
        Ok(Self {
            path,
            backing,
            file_len,
            data_len,
            footer,
            block_index,
            bloom,
        })
    }

    /// Whether this reader has a block-index sidecar loaded.
    #[must_use]
    pub fn has_block_index(&self) -> bool {
        self.block_index.is_some()
    }

    /// Whether this reader has a bloom sidecar loaded.
    #[must_use]
    pub fn has_bloom(&self) -> bool {
        self.bloom.is_some()
    }

    /// Whether the bloom filter proves `target` is absent from this table.
    /// `false` when no bloom is loaded (caller must scan) or the key may be
    /// present. Never a false "absent" — a `true` here means a scan would
    /// definitely find nothing.
    #[must_use]
    fn bloom_rejects(&self, target: &SSTableKey) -> bool {
        self.bloom.as_ref().is_some_and(|b| !b.may_contain(target))
    }

    /// Path of the underlying file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File length captured at open time.
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Footer values.
    #[must_use]
    pub fn footer(&self) -> SSTableFooter {
        self.footer
    }

    /// Streaming iterator over records. Validates the per-record CRC of every
    /// record yielded. On a CRC failure mid-stream, the iterator returns
    /// `Some(Err(_))`; subsequent calls return `None`.
    #[allow(clippy::iter_without_into_iter)]
    pub fn iter(&self) -> SSTableIter<'_> {
        let bytes = self.backing.as_bytes();
        SSTableIter {
            data: &bytes[..usize::try_from(self.data_len).unwrap_or(usize::MAX)],
            pos: 0,
            done: false,
        }
    }

    /// Lookup the record matching `target`. O(log N) when a block-index
    /// sidecar is present (binary search the sidecar to identify the
    /// block, then linear scan ≤ `block_size` bytes within it); O(N)
    /// linear scan otherwise (v1.3 SSTables, or a corrupt sidecar that
    /// was skipped on open).
    ///
    /// Returns the first record whose [`SSTableKey`] matches `target`;
    /// returns `None` if the key isn't present.
    pub fn find(&self, target: &SSTableKey) -> Result<Option<Record>, SSTableError> {
        if self.bloom_rejects(target) {
            return Ok(None);
        }
        let start_offset = self.block_index_offset(target);
        let mut iter = self.iter_from(start_offset);
        for item in &mut iter {
            let (rec, _) = item?;
            let k = SSTableKey::for_record(&rec);
            if k == *target {
                return Ok(Some(rec));
            }
            if k > *target {
                // Sorted file: we've passed where the target would be.
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Like [`find`](Self::find) but returns every record whose key
    /// matches `target` — needed by MVCC point reads, where multiple
    /// versions of the same entity may live in one SSTable.
    pub fn find_all(&self, target: &SSTableKey) -> Result<Vec<Record>, SSTableError> {
        if self.bloom_rejects(target) {
            return Ok(Vec::new());
        }
        let start_offset = self.block_index_offset(target);
        let mut out = Vec::new();
        let mut iter = self.iter_from(start_offset);
        for item in &mut iter {
            let (rec, _) = item?;
            let k = SSTableKey::for_record(&rec);
            match k.cmp(target) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => out.push(rec),
                std::cmp::Ordering::Greater => break,
            }
        }
        Ok(out)
    }

    /// Lookup the block-index entry covering `target` and return the
    /// byte offset to start scanning from. Falls back to 0 when no
    /// sidecar is loaded.
    fn block_index_offset(&self, target: &SSTableKey) -> usize {
        let off = self
            .block_index
            .as_ref()
            .and_then(|idx| idx.seek_offset(target))
            .unwrap_or(0);
        usize::try_from(off).unwrap_or(usize::MAX)
    }

    /// Iterator starting from an arbitrary byte offset in the data
    /// section. Used by `find` / `find_all` after the block index
    /// resolves the seek point.
    fn iter_from(&self, start_offset: usize) -> SSTableIter<'_> {
        let data_end =
            usize::try_from(self.data_len).unwrap_or(usize::MAX);
        let bytes = self.backing.as_bytes();
        SSTableIter {
            data: &bytes[..data_end],
            pos: start_offset,
            done: false,
        }
    }
}

/// Streaming record iterator returned by [`SSTableReader::iter`].
///
/// Backed by a mmap'd byte slice — no `Read`/`Seek` machinery. Each
/// record is decoded in-place by `Record::decode` over a sub-slice;
/// nothing is copied for the size prefix, and only the variable-length
/// payload that `Record::decode` chooses to materialise is allocated.
#[derive(Debug)]
pub struct SSTableIter<'a> {
    /// Slice covering the data section (excludes footer). Length is
    /// `footer.data_size`.
    data: &'a [u8],
    /// Position inside the data section (0 = first byte of file).
    pos: usize,
    /// Set after a CRC failure so subsequent `next()` returns `None`.
    done: bool,
}

impl SSTableIter<'_> {
    fn read_one(&mut self) -> Result<Option<(Record, u64)>, SSTableError> {
        if self.done || self.pos >= self.data.len() {
            return Ok(None);
        }
        let remaining = self.data.len() - self.pos;
        if remaining < 4 {
            return Err(SSTableError::Decode {
                offset: self.pos as u64,
                source: DecodeError::Truncated {
                    offset: self.pos,
                    needed: 4 - remaining,
                },
            });
        }
        let size_buf = &self.data[self.pos..self.pos + 4];
        let claimed = u32::from_le_bytes(size_buf.try_into().unwrap()) as usize;
        if claimed == 0 || claimed > remaining {
            return Err(SSTableError::Decode {
                offset: self.pos as u64,
                source: DecodeError::InvalidRecordSize {
                    claimed,
                    available: remaining,
                },
            });
        }
        let lsn = self.pos as u64;
        let slice = &self.data[self.pos..self.pos + claimed];
        let (rec, consumed) = Record::decode(slice).map_err(|e| SSTableError::Decode {
            offset: lsn,
            source: e,
        })?;
        debug_assert_eq!(consumed, claimed);
        self.pos += claimed;
        Ok(Some((rec, lsn)))
    }
}

impl Iterator for SSTableIter<'_> {
    type Item = Result<(Record, u64), SSTableError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.read_one() {
            Ok(Some(p)) => Some(Ok(p)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Read the footer of an SSTable without fully opening it. Cheap probe used
/// by MANIFEST recovery to enumerate tables.
pub fn read_footer<P: AsRef<Path>>(path: P) -> Result<SSTableFooter, SSTableError> {
    let mut file = File::open(path)?;
    let file_len = file.seek(SeekFrom::End(0))?;
    let needed = SSTABLE_FOOTER_SIZE as u64;
    if file_len < needed {
        return Err(SSTableError::TooShort {
            len: file_len,
            needed,
        });
    }
    file.seek(SeekFrom::Start(file_len - needed))?;
    let mut buf = [0u8; SSTABLE_FOOTER_SIZE];
    file.read_exact(&mut buf)?;
    decode_footer(&buf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
    use crate::record::{
        EntityRecord, HyperEdgeRecord, PropertyKeyRecord, RoleNameRecord, TombstoneRecord,
        TypeNameRecord,
    };
    use crate::value::Value;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ndb-sstable-{}-{}",
            name,
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn dict(id: u32, name: &str) -> Record {
        Record::TypeName(TypeNameRecord {
            id: TypeId::new(id),
            name: name.into(),
        })
    }
    fn role(id: u32, name: &str) -> Record {
        Record::RoleName(RoleNameRecord {
            id: RoleId::new(id),
            name: name.into(),
        })
    }
    fn prop(id: u32, name: &str) -> Record {
        Record::PropertyKey(PropertyKeyRecord {
            id: PropertyId::new(id),
            name: name.into(),
        })
    }
    fn entity(eid: EntityId, type_id: u32, tx: u64) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::String("x".into()))],
        })
    }
    fn hyperedge(hid: HyperedgeId, type_id: u32, tx: u64) -> Record {
        Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        })
    }
    fn tombstone(target: uuid::Uuid, tx: u64) -> Record {
        Record::Tombstone(TombstoneRecord {
            target_id: target,
            tx_id_supersede: TxId::new(tx),
        })
    }

    /// Build a small sorted record set covering every kind.
    fn sorted_corpus() -> Vec<Record> {
        let mut entities: Vec<_> = (0..3).map(|_| EntityId::now_v7()).collect();
        let mut hyperedges: Vec<_> = (0..2).map(|_| HyperedgeId::now_v7()).collect();
        let mut tombs: Vec<_> = (0..2).map(|_| uuid::Uuid::now_v7()).collect();
        // Sort by raw UUID bytes so we drive the SSTable writer in order.
        entities.sort_by_key(|e| *e.as_bytes());
        hyperedges.sort_by_key(|h| *h.as_bytes());
        tombs.sort_by_key(|t| *t.as_bytes());

        let mut out = Vec::new();
        // kind 0x01 — entities
        for (i, e) in entities.into_iter().enumerate() {
            out.push(entity(e, 1, 100 + i as u64));
        }
        // kind 0x02 — hyperedges
        for (i, h) in hyperedges.into_iter().enumerate() {
            out.push(hyperedge(h, 7, 200 + i as u64));
        }
        // kind 0x03 — tombstones
        for (i, t) in tombs.into_iter().enumerate() {
            out.push(tombstone(t, 300 + i as u64));
        }
        // kinds 0x04..0x06 — dictionary entries in id order
        out.push(dict(1, "Customer"));
        out.push(dict(2, "Supplier"));
        out.push(role(1, "approver"));
        out.push(role(2, "subject"));
        out.push(prop(1, "email"));
        out.push(prop(2, "amount"));
        // Sanity: keys must be non-decreasing by SSTable order.
        let mut prev: Option<SSTableKey> = None;
        for r in &out {
            let k = SSTableKey::for_record(r);
            if let Some(p) = &prev {
                assert!(*p <= k, "corpus not sorted at {k:?} (after {p:?})");
            }
            prev = Some(k);
        }
        out
    }

    #[test]
    fn write_read_round_trip() {
        let dir = temp_dir("rrt");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();

        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        let footer = w.finish().unwrap();
        assert_eq!(footer.record_count, records.len() as u64);

        let r = SSTableReader::open(&path).unwrap();
        assert_eq!(r.footer().record_count, records.len() as u64);
        let restored: Result<Vec<_>, _> = r.iter().map(|res| res.map(|(rec, _)| rec)).collect();
        let restored = restored.unwrap();
        assert_eq!(restored, records);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn footer_round_trip_via_read_footer() {
        let dir = temp_dir("footer_probe");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        let written = w.finish().unwrap();
        let probed = read_footer(&path).unwrap();
        assert_eq!(written, probed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn out_of_order_append_fails() {
        let dir = temp_dir("order");
        let path = dir.join("000001.ndb");
        let mut w = SSTableWriter::create(&path).unwrap();
        // Two type-name records with descending ids — out of order.
        w.append(&dict(5, "B")).unwrap();
        let err = w.append(&dict(3, "A")).unwrap_err();
        assert!(matches!(err, SSTableError::OutOfOrder { index: 1 }));
        w.abort().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finish_writes_block_index_sidecar() {
        let dir = temp_dir("sidecar");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();
        let sidecar = crate::block_index::sidecar_path_for(&path);
        assert!(sidecar.exists(), "sidecar at {sidecar:?} must exist");
        let r = SSTableReader::open(&path).unwrap();
        assert!(r.has_block_index(), "reader must pick up the sidecar");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_uses_block_index_when_present_and_works_when_absent() {
        let dir = temp_dir("find_sidecar");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        // With sidecar — fast path.
        {
            let r = SSTableReader::open(&path).unwrap();
            assert!(r.has_block_index());
            for rec in &records {
                let k = SSTableKey::for_record(rec);
                let hit = r.find(&k).unwrap();
                assert!(hit.is_some(), "key {k:?} should be present");
            }
        }

        // Delete the sidecar — slow path (v1.3-compatible).
        let sidecar = crate::block_index::sidecar_path_for(&path);
        std::fs::remove_file(&sidecar).unwrap();
        {
            let r = SSTableReader::open(&path).unwrap();
            assert!(!r.has_block_index(), "missing sidecar = no index loaded");
            for rec in &records {
                let k = SSTableKey::for_record(rec);
                let hit = r.find(&k).unwrap();
                assert!(hit.is_some(), "key {k:?} should still be found via linear scan");
            }
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_all_returns_every_match_for_multi_version_keys() {
        // Build an SSTable with two records of the same SSTableKey
        // (same kind + uuid, different tx_id_assert). find() returns
        // only the first; find_all() must return both — this is what
        // MVCC point reads need to resolve the correct visible record.
        let dir = temp_dir("find_all_mvcc");
        let path = dir.join("000001.ndb");
        let eid = uuid::Uuid::now_v7();
        let mut w = SSTableWriter::create(&path).unwrap();
        // Two versions of the same entity in sorted order (the second
        // has a higher tx_id_assert; encoder doesn't sort, so we feed
        // them in the order they'd appear on disk).
        w.append(&Record::Entity(EntityRecord {
            entity_id: EntityId::from_uuid(eid),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(5),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), crate::value::Value::I64(5))],
        })).unwrap();
        w.append(&Record::Entity(EntityRecord {
            entity_id: EntityId::from_uuid(eid),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(10),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), crate::value::Value::I64(10))],
        })).unwrap();
        w.finish().unwrap();

        let r = SSTableReader::open(&path).unwrap();
        assert!(r.has_block_index());
        let key = SSTableKey {
            kind: RecordKind::Entity.as_byte(),
            primary: eid.as_bytes().to_vec(),
        };
        let hits = r.find_all(&key).unwrap();
        assert_eq!(hits.len(), 2, "both versions must be returned");
        // find() returns only the first — verify the contract still
        // holds since snapshot_read no longer uses it.
        let one = r.find(&key).unwrap();
        assert!(one.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finish_writes_bloom_sidecar_and_reader_loads_it() {
        let dir = temp_dir("bloom_sidecar");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        let bloom_path = crate::bloom::sidecar_path_for(&path);
        assert!(bloom_path.exists(), "bloom sidecar at {bloom_path:?} must exist");

        let r = SSTableReader::open(&path).unwrap();
        assert!(r.has_bloom(), "reader must pick up the bloom sidecar");
        // Every real key must survive the bloom (no false negatives).
        for rec in &records {
            let k = SSTableKey::for_record(rec);
            assert!(r.find(&k).unwrap().is_some(), "real key {k:?} skipped by bloom");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bloom_short_circuits_absent_keys_and_find_still_correct() {
        // A table of entities. Looking up an entity UUID that was never
        // inserted should (almost always) be rejected by the bloom; either
        // way the answer must be None.
        let dir = temp_dir("bloom_absent");
        let path = dir.join("000001.ndb");
        let mut records: Vec<Record> = (0..64)
            .map(|i| entity(EntityId::now_v7(), 100 + i, u64::from(i)))
            .collect();
        records.sort_by(|a, b| SSTableKey::for_record(a).cmp(&SSTableKey::for_record(b)));
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        let r = SSTableReader::open(&path).unwrap();
        assert!(r.has_bloom());
        // 200 random absent keys must all miss; with fpr 1% the bloom
        // rejects ~99% before scanning, but correctness holds regardless.
        for _ in 0..200 {
            let absent = SSTableKey {
                kind: RecordKind::Entity.as_byte(),
                primary: EntityId::now_v7().as_bytes().to_vec(),
            };
            assert!(r.find(&absent).unwrap().is_none());
            assert!(r.find_all(&absent).unwrap().is_empty());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_bloom_sidecar_falls_back_to_scan() {
        let dir = temp_dir("bloom_missing");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();
        // Delete the bloom sidecar — reader must still find every key.
        std::fs::remove_file(crate::bloom::sidecar_path_for(&path)).unwrap();
        let r = SSTableReader::open(&path).unwrap();
        assert!(!r.has_bloom(), "deleted sidecar = no bloom loaded");
        for rec in &records {
            let k = SSTableKey::for_record(rec);
            assert!(r.find(&k).unwrap().is_some());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn abort_removes_temp_file() {
        let dir = temp_dir("abort");
        let path = dir.join("000001.ndb");
        let mut w = SSTableWriter::create(&path).unwrap();
        w.append(&dict(1, "X")).unwrap();
        let tmp = tmp_sibling(&path);
        assert!(tmp.exists());
        w.abort().unwrap();
        assert!(!tmp.exists());
        assert!(!path.exists(), "no rename happened");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_locates_records_and_misses_cleanly() {
        let dir = temp_dir("find");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        let r = SSTableReader::open(&path).unwrap();
        // Lookup a dictionary entry by exact key.
        let key = SSTableKey::for_record(&dict(2, "Supplier"));
        let hit = r.find(&key).unwrap().expect("Supplier present");
        if let Record::TypeName(d) = hit {
            assert_eq!(d.id, TypeId::new(2));
            assert_eq!(d.name, "Supplier");
        } else {
            panic!("wrong kind");
        }
        // Miss: a property key id that doesn't exist.
        let miss = r
            .find(&SSTableKey {
                kind: RecordKind::PropertyKey.as_byte(),
                primary: 99u32.to_be_bytes().to_vec(),
            })
            .unwrap();
        assert!(miss.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupted_magic_rejected() {
        let dir = temp_dir("badmagic");
        let path = dir.join("000001.ndb");
        let mut w = SSTableWriter::create(&path).unwrap();
        w.append(&dict(1, "X")).unwrap();
        w.finish().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        // Flip the first magic byte.
        let footer_start = bytes.len() - SSTABLE_FOOTER_SIZE;
        bytes[footer_start] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let err = SSTableReader::open(&path).unwrap_err();
        assert!(matches!(err, SSTableError::InvalidMagic { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupted_footer_crc_rejected() {
        let dir = temp_dir("footercrc");
        let path = dir.join("000001.ndb");
        let mut w = SSTableWriter::create(&path).unwrap();
        w.append(&dict(1, "X")).unwrap();
        w.finish().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside record_count, leaving the magic intact.
        let footer_start = bytes.len() - SSTABLE_FOOTER_SIZE;
        bytes[footer_start + 12] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let err = SSTableReader::open(&path).unwrap_err();
        assert!(matches!(err, SSTableError::FooterCrcMismatch { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupted_record_surfaces_during_iter() {
        let dir = temp_dir("rec_corrupt");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w = SSTableWriter::create(&path).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        // Find the start offset of the third record by scanning bytes — flip
        // a byte well past the size prefix.
        let mut bytes = std::fs::read(&path).unwrap();
        let mut offset: usize = 0;
        for _ in 0..2 {
            offset += peek_record_size(&bytes[offset..]).unwrap();
        }
        // offset now = start of record #3.
        bytes[offset + 10] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let r = SSTableReader::open(&path).unwrap();
        let mut ok = 0;
        let mut errored = false;
        for item in r.iter() {
            match item {
                Ok(_) => ok += 1,
                Err(SSTableError::Decode { .. }) => {
                    errored = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert!(errored);
        assert_eq!(ok, 2, "first two records survive");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_file_rejected() {
        let dir = temp_dir("trunc");
        let path = dir.join("000001.ndb");
        let mut w = SSTableWriter::create(&path).unwrap();
        w.append(&dict(1, "X")).unwrap();
        w.finish().unwrap();

        // Lop off 5 bytes from the end (inside the footer).
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        let len = f.metadata().unwrap().len();
        f.set_len(len - 5).unwrap();

        let err = SSTableReader::open(&path).unwrap_err();
        // Truncating mid-footer makes the file shorter than SSTABLE_FOOTER_SIZE
        // -> TooShort fires; if it landed differently we'd see InvalidMagic.
        assert!(
            matches!(
                err,
                SSTableError::TooShort { .. } | SSTableError::InvalidMagic { .. }
            ),
            "got {err:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sstable_key_ordering_matches_byte_compare() {
        // Two dictionary records with id 1 and id 1000 (BE-encoded) — byte
        // compare must match numeric order.
        let a = SSTableKey::for_record(&dict(1, ""));
        let b = SSTableKey::for_record(&dict(1000, ""));
        assert!(a < b);
        // Across kinds: a dictionary kind (0x04) sorts AFTER tombstone (0x03).
        let dict_key = SSTableKey::for_record(&dict(1, ""));
        let tomb_key = SSTableKey::for_record(&tombstone(uuid::Uuid::nil(), 1));
        assert!(tomb_key < dict_key);
    }

    // ---------------------------------------------------------------------
    // Block compression (v2 format).
    // ---------------------------------------------------------------------

    #[test]
    fn compressed_sstable_round_trips_and_finds() {
        let dir = temp_dir("compress_rt");
        let path = dir.join("000001.ndb");
        let records = sorted_corpus();
        let mut w =
            SSTableWriter::create_with_cipher_codec(&path, None, crate::compression::Codec::Lz4)
                .unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        let footer = w.finish().unwrap();
        assert_eq!(footer.format_version, SSTABLE_FORMAT_VERSION_COMPRESSED);
        assert_eq!(footer.record_count, records.len() as u64);

        let r = SSTableReader::open(&path).unwrap();
        assert_eq!(r.footer().format_version, 2);
        // Full iter round-trip.
        let back: Result<Vec<_>, _> = r.iter().map(|res| res.map(|(rec, _)| rec)).collect();
        assert_eq!(back.unwrap(), records);
        // find() locates every key (block index offsets are uncompressed, so
        // they resolve against the reconstructed stream).
        for rec in &records {
            let k = SSTableKey::for_record(rec);
            assert!(r.find(&k).unwrap().is_some(), "key {k:?} must be found");
        }
        // Absent key misses cleanly.
        assert!(
            r.find(&SSTableKey {
                kind: RecordKind::PropertyKey.as_byte(),
                primary: 999u32.to_be_bytes().to_vec(),
            })
            .unwrap()
            .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compression_shrinks_a_repetitive_sstable_and_reads_back() {
        let dir = temp_dir("compress_size");
        let mut ents: Vec<EntityId> = (0..400).map(|_| EntityId::now_v7()).collect();
        ents.sort_by_key(|e| *e.as_bytes());
        let records: Vec<Record> = ents
            .iter()
            .map(|e| {
                Record::Entity(EntityRecord {
                    entity_id: *e,
                    type_id: TypeId::new(1),
                    tx_id_assert: TxId::new(1),
                    tx_id_supersede: TxId::ACTIVE,
                    properties: vec![(
                        PropertyId::new(1),
                        Value::String("the same long repetitive value repeated".into()),
                    )],
                })
            })
            .collect();

        let p1 = dir.join("000001.ndb");
        let mut w = SSTableWriter::create(&p1).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();
        let v1_size = std::fs::metadata(&p1).unwrap().len();

        let p2 = dir.join("000002.ndb");
        let mut w =
            SSTableWriter::create_with_cipher_codec(&p2, None, crate::compression::Codec::Lz4)
                .unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();
        let v2_size = std::fs::metadata(&p2).unwrap().len();

        assert!(v2_size < v1_size, "compressed {v2_size} should be < plain {v1_size}");
        let rd = SSTableReader::open(&p2).unwrap();
        let back: Vec<_> = rd.iter().map(|r| r.unwrap().0).collect();
        assert_eq!(back, records);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_and_compressed_compose() {
        let dir = temp_dir("enc_compress");
        let path = dir.join("000001.ndb");
        let mut records = enc_records();
        records.sort_by(|a, b| SSTableKey::for_record(a).cmp(&SSTableKey::for_record(b)));
        let mut w = SSTableWriter::create_with_cipher_codec(
            &path,
            Some(test_cipher()),
            crate::compression::Codec::Lz4,
        )
        .unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        let footer = w.finish().unwrap();
        assert_eq!(footer.format_version, 2);

        let rd = SSTableReader::open_with_cipher(&path, Some(test_cipher())).unwrap();
        let back: Vec<_> = rd.iter().map(|r| r.unwrap().0).collect();
        assert_eq!(back, records);
        let mid = SSTableKey::for_record(&records[records.len() / 2]);
        assert!(rd.find(&mid).unwrap().is_some(), "find through enc+compress");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---------------------------------------------------------------------
    // Encryption — write+read round-trip + wrong-key + plain-vs-encrypted.
    // ---------------------------------------------------------------------

    fn test_cipher() -> Cipher {
        Cipher::from_raw_key(&[0x77u8; 32]).unwrap()
    }

    fn enc_records() -> Vec<Record> {
        vec![
            entity(EntityId::now_v7(), 10, 1),
            entity(EntityId::now_v7(), 20, 2),
            entity(EntityId::now_v7(), 30, 3),
        ]
    }

    #[test]
    fn encrypted_sstable_round_trip_iter() {
        let dir = temp_dir("enc_sst_round_trip");
        let path = dir.join("000001.ndb");
        let mut records = enc_records();
        records.sort_by(|a, b| SSTableKey::for_record(a).cmp(&SSTableKey::for_record(b)));

        let mut w = SSTableWriter::create_with_cipher(&path, Some(test_cipher())).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        let footer = w.finish().unwrap();
        assert_eq!(footer.record_count, records.len() as u64);

        let reader = SSTableReader::open_with_cipher(&path, Some(test_cipher())).unwrap();
        assert_eq!(reader.footer().record_count, records.len() as u64);
        let read_back: Vec<Record> = reader.iter().map(|r| r.unwrap().0).collect();
        assert_eq!(read_back, records);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_sstable_find_uses_block_index() {
        let dir = temp_dir("enc_sst_find");
        let path = dir.join("000001.ndb");
        let mut records: Vec<Record> = (0..32)
            .map(|i| entity(EntityId::now_v7(), 100 + i, u64::from(i)))
            .collect();
        records.sort_by(|a, b| SSTableKey::for_record(a).cmp(&SSTableKey::for_record(b)));

        let mut w = SSTableWriter::create_with_cipher(&path, Some(test_cipher())).unwrap();
        for r in &records {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        let reader = SSTableReader::open_with_cipher(&path, Some(test_cipher())).unwrap();
        assert!(reader.has_block_index(), "sidecar should still be written for encrypted SSTables");
        // Spot-check find — encryption should be invisible at the API.
        let mid_key = SSTableKey::for_record(&records[records.len() / 2]);
        let found = reader.find(&mid_key).unwrap().unwrap();
        assert_eq!(found, records[records.len() / 2]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_sstable_wrong_key_fails_open() {
        let dir = temp_dir("enc_sst_wrong_key");
        let path = dir.join("000001.ndb");

        let mut w = SSTableWriter::create_with_cipher(&path, Some(test_cipher())).unwrap();
        for r in &enc_records() {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        let wrong = Cipher::from_raw_key(&[0x99u8; 32]).unwrap();
        let result = SSTableReader::open_with_cipher(&path, Some(wrong));
        assert!(result.is_err(), "wrong key must error at open time");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_sstable_plain_reader_rejects() {
        // A plain SSTable reader should fail on an encrypted file —
        // the on-disk magic is the EncryptedFile magic, not the SSTable
        // footer magic, and the file isn't laid out as plaintext.
        let dir = temp_dir("enc_sst_plain_reader");
        let path = dir.join("000001.ndb");

        let mut w = SSTableWriter::create_with_cipher(&path, Some(test_cipher())).unwrap();
        for r in &enc_records() {
            w.append(r).unwrap();
        }
        w.finish().unwrap();

        let result = SSTableReader::open(&path);
        assert!(
            result.is_err(),
            "plaintext reader must reject encrypted SSTable"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
