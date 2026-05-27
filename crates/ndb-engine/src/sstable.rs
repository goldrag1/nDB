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

/// Highest SSTable `format_version` this build can read.
pub const SSTABLE_FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;

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
#[derive(Debug)]
pub struct SSTableWriter {
    final_path: PathBuf,
    tmp_path: PathBuf,
    file: BufWriter<File>,
    record_count: u64,
    bytes_written: u64,
    last_key: Option<SSTableKey>,
}

impl SSTableWriter {
    /// Open a temp file alongside `final_path` and prepare to receive records.
    pub fn create<P: AsRef<Path>>(final_path: P) -> Result<Self, SSTableError> {
        let final_path = final_path.as_ref().to_path_buf();
        let tmp_path = tmp_sibling(&final_path);
        // O_TRUNC to clean up a crashed prior write attempt.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        Ok(Self {
            final_path,
            tmp_path,
            file: BufWriter::new(file),
            record_count: 0,
            bytes_written: 0,
            last_key: None,
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
        let mut buf = Vec::with_capacity(128);
        record
            .encode(&mut buf)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("encode failed: {e}")))?;
        self.file.write_all(&buf)?;
        self.bytes_written += buf.len() as u64;
        self.record_count += 1;
        self.last_key = Some(key);
        Ok(())
    }

    /// Append already-encoded record bytes. Skips the sort-order check
    /// (caller already encoded the record, so they own the contract).
    pub fn append_raw(&mut self, bytes: &[u8]) -> Result<(), SSTableError> {
        self.file.write_all(bytes)?;
        self.bytes_written += bytes.len() as u64;
        self.record_count += 1;
        Ok(())
    }

    /// Write the footer, fsync the temp file, rename onto `final_path`, then
    /// fsync the parent directory.
    pub fn finish(mut self) -> Result<SSTableFooter, SSTableError> {
        let footer = SSTableFooter {
            record_count: self.record_count,
            data_size: self.bytes_written,
            format_version: SSTABLE_FORMAT_VERSION,
            flags: 0,
        };
        let footer_bytes = encode_footer(&footer);
        self.file.write_all(&footer_bytes)?;
        self.file.flush()?;
        let f = self
            .file
            .into_inner()
            .map_err(|e| io::Error::other(format!("BufWriter into_inner failed: {e}")))?;
        f.sync_data()?;
        std::fs::rename(&self.tmp_path, &self.final_path)?;
        // fsync the parent directory so the rename itself is durable.
        if let Some(parent) = self.final_path.parent() {
            fsync_dir(parent)?;
        }
        Ok(footer)
    }

    /// Abort the build: close the temp file and remove it. Idempotent.
    pub fn abort(self) -> io::Result<()> {
        drop(self.file);
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
#[derive(Debug)]
pub struct SSTableReader {
    path: PathBuf,
    /// Memory-mapped view of the file. Used for all reads — sequential
    /// iteration and (future) random-access block lookups. The map covers
    /// the entire file including the footer. v1 reads are sequential;
    /// mmap pays off most for the eventual block-index path because it
    /// lets the kernel page in only what we touch.
    mmap: memmap2::Mmap,
    /// Keeps the underlying file descriptor alive for the lifetime of the
    /// mmap. We never write through the FD — SSTable files are
    /// write-temp-then-rename and read-only after `open()`.
    _file: File,
    file_len: u64,
    footer: SSTableFooter,
}

impl SSTableReader {
    /// Open `path`, validate the footer, mmap the file, and return a
    /// handle ready for iteration / lookup.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, SSTableError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        let needed = SSTABLE_FOOTER_SIZE as u64;
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
        let footer_off = usize::try_from(file_len - needed)
            .map_err(|_| SSTableError::TooShort { len: file_len, needed: usize::MAX as u64 })?;
        let mut footer_bytes = [0u8; SSTABLE_FOOTER_SIZE];
        footer_bytes.copy_from_slice(&mmap[footer_off..footer_off + SSTABLE_FOOTER_SIZE]);
        let footer = decode_footer(&footer_bytes)?;
        if footer.data_size + needed != file_len {
            // data_size in the footer disagrees with the file length — clear
            // sign of truncation or extra trailing bytes.
            return Err(SSTableError::TooShort {
                len: file_len,
                needed: footer.data_size + needed,
            });
        }
        Ok(Self {
            path,
            mmap,
            _file: file,
            file_len,
            footer,
        })
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
    pub fn iter(&mut self) -> SSTableIter<'_> {
        SSTableIter {
            data: &self.mmap[..usize::try_from(self.footer.data_size).unwrap_or(usize::MAX)],
            pos: 0,
            done: false,
        }
    }

    /// Linear-scan lookup. Returns the first record whose [`SSTableKey`]
    /// matches `target`. v1 implementation is O(N); a block-index sidecar
    /// will give O(log N) in a follow-on commit.
    pub fn find(&mut self, target: &SSTableKey) -> Result<Option<Record>, SSTableError> {
        for item in self.iter() {
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

        let mut r = SSTableReader::open(&path).unwrap();
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

        let mut r = SSTableReader::open(&path).unwrap();
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

        let mut r = SSTableReader::open(&path).unwrap();
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
}
