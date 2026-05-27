//! Append-only write-ahead log (§9.1, §11.5 — `.ndblog` file).
//!
//! The WAL is the durability layer that sits between the in-memory memtable
//! (not yet implemented) and the flushed `SSTable`s. A transaction is
//! considered committed once its records are appended to the WAL and
//! `fsync`'d. On crash restart the engine replays the WAL into a fresh
//! memtable.
//!
//! v1 decisions, made here in the commit that introduces the file:
//!
//! - **Separate `.ndblog` file.** Memtable-as-WAL is rejected — the memtable
//!   is in-memory in v1, so durability requires its own log. (§11.4)
//! - **Buffered `std::fs::File`, not mmap.** mmap-based logging has subtle
//!   semantics around partial writes and signals that we don't need yet. The
//!   buffered-file path can saturate sequential SSDs perfectly well for the
//!   single-writer model (§14.3).
//! - **Record envelopes are the framing.** Each record already carries its
//!   own `record_size` and CRC32, so no separate WAL framing is needed.
//!   Recovery scans from offset 0 and validates each envelope; the first
//!   CRC mismatch or truncated read is treated as the boundary of the last
//!   durable record. Everything after is discarded.
//! - **Atomic batches via grouped `fsync`.** Multiple records belonging to
//!   one transaction are written sequentially in a single `append_batch` call
//!   followed by a single `fsync_data`. A crash either persists the full
//!   batch or none of it (modulo trailing-partial-record truncation, which
//!   the per-record CRC catches).

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::error::DecodeError;
use crate::record::{Record, peek_record_size};

/// Canonical file extension for WAL files (§11.5).
pub const WAL_EXTENSION: &str = "ndblog";

/// Size of the `record_size` prefix on every record (used by the reader to
/// peek the length without parsing).
const SIZE_PREFIX: usize = 4;

/// Append-only writer over a `.ndblog` file.
///
/// One [`WriteAheadLog`] owns the active log file for a database. Appends
/// are buffered; durability requires an explicit [`WriteAheadLog::sync`].
pub struct WriteAheadLog {
    path: PathBuf,
    file: BufWriter<File>,
    /// Number of bytes durably written + buffered (the next-write LSN).
    bytes_written: u64,
}

impl WriteAheadLog {
    /// Create a fresh `.ndblog` file, failing if it already exists. Used when
    /// the engine starts a brand-new WAL segment.
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
            bytes_written: 0,
        })
    }

    /// Open an existing `.ndblog` for append. The cursor seeks to the current
    /// file length, so subsequent appends extend the file. Caller is
    /// responsible for having previously truncated any partial trailing
    /// record (see [`WalReader::recover`]).
    pub fn open_append<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().write(true).append(false).open(&path)?;
        let end = file.seek(SeekFrom::End(0))?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
            bytes_written: end,
        })
    }

    /// Append one already-encoded record's bytes. Returns the LSN (the byte
    /// offset of the first byte of this record). Does *not* `fsync`; call
    /// [`sync`](Self::sync) after a batch.
    ///
    /// The caller is responsible for ensuring `record_bytes` is a complete
    /// record produced by one of the `*Record::encode` methods.
    pub fn append_raw(&mut self, record_bytes: &[u8]) -> io::Result<u64> {
        let lsn = self.bytes_written;
        self.file.write_all(record_bytes)?;
        self.bytes_written += record_bytes.len() as u64;
        Ok(lsn)
    }

    /// Append a parsed record. Equivalent to encoding into a temporary buffer
    /// and calling [`append_raw`](Self::append_raw); kept separate so the
    /// hot path of replaying pre-encoded records (e.g. compaction) avoids
    /// allocating.
    pub fn append(&mut self, record: &Record) -> io::Result<u64> {
        let mut buf = Vec::with_capacity(128);
        record
            .encode(&mut buf)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("encode failed: {e}")))?;
        self.append_raw(&buf)
    }

    /// Append every record in `records` sequentially. Returns the LSN of the
    /// first record in the batch. The records become atomically durable only
    /// after a subsequent [`sync`](Self::sync).
    pub fn append_batch(&mut self, records: &[Record]) -> io::Result<u64> {
        let first_lsn = self.bytes_written;
        let mut buf = Vec::with_capacity(128 * records.len().max(1));
        for r in records {
            buf.clear();
            r.encode(&mut buf).map_err(|e| {
                io::Error::new(ErrorKind::InvalidData, format!("encode failed: {e}"))
            })?;
            self.file.write_all(&buf)?;
            self.bytes_written += buf.len() as u64;
        }
        Ok(first_lsn)
    }

    /// Flush the buffered writer to the OS, then `fsync_data` the file
    /// descriptor. After this call returns Ok, every byte written so far is
    /// durable.
    ///
    /// Uses `sync_data` (not `sync_all`) — we don't need metadata-only fields
    /// like atime to be flushed; only the file *contents* matter for
    /// recovery. Saves a syscall on platforms where it's distinct.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()
    }

    /// Bytes durably or buffered in this WAL. Equal to the file length at the
    /// last `sync`, plus any unflushed buffered bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.bytes_written
    }

    /// Whether nothing has been appended yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes_written == 0
    }

    /// Path of the underlying WAL file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush + sync + drop. Equivalent to `sync()` followed by `drop(self)`.
    pub fn close(mut self) -> io::Result<()> {
        self.sync()
    }
}

impl Drop for WriteAheadLog {
    fn drop(&mut self) {
        // Best-effort flush on drop; intentional final sync should go through
        // `close()` so the caller can observe errors.
        let _ = self.file.flush();
    }
}

// ---------------------------------------------------------------------------
// Reader — recovery + replay
// ---------------------------------------------------------------------------

/// Errors raised when reading a WAL file. Wraps `io::Error` and
/// `DecodeError`.
#[derive(Debug, Error)]
pub enum WalReadError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A record in the middle of the log failed to decode (envelope, CRC,
    /// sentinel, etc.). Mid-stream corruption is not recoverable in v1 — the
    /// caller must surface this to the operator.
    #[error("WAL record at offset {offset} failed to decode: {source}")]
    Decode {
        /// Byte offset of the offending record's first byte.
        offset: u64,
        /// Underlying decode error.
        #[source]
        source: DecodeError,
    },
}

/// Outcome of [`WalReader::recover`]: how the scan terminated and where the
/// next append should begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecovery {
    /// Records successfully read.
    pub records_read: usize,
    /// Byte offset of the first byte AFTER the last fully-decoded record.
    /// This is the safe-truncate point for an open-append session.
    pub durable_end: u64,
    /// Bytes present on disk past `durable_end`, indicating a partial
    /// trailing record (typically a crash mid-write).
    pub trailing_garbage: u64,
}

/// Streaming reader for a `.ndblog` file. Reads records one at a time so
/// large logs don't load entirely into memory.
pub struct WalReader {
    file: File,
    path: PathBuf,
    /// File length captured at open time, used to detect truncated trailing
    /// records.
    file_len: u64,
    /// Current read cursor.
    pos: u64,
}

impl WalReader {
    /// Open `path` for read-only streaming.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            file,
            path,
            file_len,
            pos: 0,
        })
    }

    /// Path of the underlying file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File length at open time (in bytes).
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Current cursor position.
    #[must_use]
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Read the next record from the WAL. Returns:
    ///
    /// - `Ok(Some((record, lsn)))` — a record decoded successfully; `lsn` is
    ///   the byte offset where the record started.
    /// - `Ok(None)` — clean EOF (cursor is exactly at `file_len`).
    /// - `Err(_)` — mid-stream corruption (CRC, envelope, sentinel) or I/O.
    ///
    /// A truncated trailing record (cursor lands < `file_len` but the
    /// remaining bytes don't form a complete record) is reported as
    /// `Ok(None)` so the recovery flow can treat it as a partial write.
    pub fn next_record(&mut self) -> Result<Option<(Record, u64)>, WalReadError> {
        if self.pos >= self.file_len {
            return Ok(None);
        }
        let remaining = self.file_len - self.pos;
        if remaining < SIZE_PREFIX as u64 {
            // Trailing garbage shorter than a size field — partial write.
            // Leave pos UNCHANGED so the recovery sees this as the boundary
            // between durable and torn data.
            return Ok(None);
        }

        // The previous call may have left the file cursor mid-stream after a
        // successful decode; an explicit seek keeps the read positionally
        // correct without depending on call order.
        self.file.seek(SeekFrom::Start(self.pos))?;

        let mut size_buf = [0u8; SIZE_PREFIX];
        self.file.read_exact(&mut size_buf)?;
        let claimed = u64::from(u32::from_le_bytes(size_buf));
        if claimed == 0 {
            // Zero-sized record would loop forever; surface as a corruption
            // error rather than silently treating the rest of the file as
            // garbage.
            return Err(WalReadError::Decode {
                offset: self.pos,
                source: DecodeError::RecordSizeTooSmall {
                    claimed: 0,
                    minimum: crate::record::ENVELOPE_OVERHEAD,
                },
            });
        }
        if claimed > remaining {
            // Truncated trailing record — typical crash. Pos UNCHANGED so
            // recovery's `durable_end` lands exactly on the boundary between
            // the last good record and the torn bytes.
            return Ok(None);
        }

        // `claimed` is bounded above by `remaining` (a `u64` already addressable
        // in the file system layer), so the conversion to `usize` cannot lose
        // information on any 64-bit platform we target. Falls back to an error
        // for the unlikely 32-bit case rather than panicking.
        let claimed_usize = usize::try_from(claimed).map_err(|_| {
            io::Error::new(ErrorKind::InvalidData, "WAL record too large for usize")
        })?;
        let mut full = vec![0u8; claimed_usize];
        full[..SIZE_PREFIX].copy_from_slice(&size_buf);
        self.file.read_exact(&mut full[SIZE_PREFIX..])?;

        let lsn = self.pos;
        let (record, consumed) = Record::decode(&full).map_err(|e| WalReadError::Decode {
            offset: lsn,
            source: e,
        })?;
        debug_assert_eq!(consumed, claimed_usize);
        self.pos += claimed;
        Ok(Some((record, lsn)))
    }

    /// Replay the entire WAL into a `Vec`. Convenience for small logs / tests;
    /// production code should iterate with [`next_record`](Self::next_record).
    pub fn replay_all(&mut self) -> Result<(Vec<(Record, u64)>, WalRecovery), WalReadError> {
        let mut out = Vec::new();
        while let Some(pair) = self.next_record()? {
            out.push(pair);
        }
        let durable_end = self.pos;
        let trailing_garbage = self.file_len.saturating_sub(durable_end);
        Ok((
            out.clone(),
            WalRecovery {
                records_read: out.len(),
                durable_end,
                trailing_garbage,
            },
        ))
    }

    /// Scan to determine the safe truncate point without retaining decoded
    /// records. Used at startup to size the next-append cursor.
    pub fn recover(&mut self) -> Result<WalRecovery, WalReadError> {
        let mut count = 0;
        while self.next_record()?.is_some() {
            count += 1;
        }
        Ok(WalRecovery {
            records_read: count,
            durable_end: self.pos,
            trailing_garbage: self.file_len.saturating_sub(self.pos),
        })
    }
}

/// Truncate a WAL file to a known-safe length (typically the `durable_end`
/// from [`WalRecovery`]). Idempotent.
pub fn truncate_to(path: &Path, len: u64) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)
}

/// Peek the size of the record at the head of a slice without parsing — thin
/// wrapper over [`crate::record::peek_record_size`] for callers that already
/// `use` the wal module.
pub fn peek_size(input: &[u8]) -> Result<usize, DecodeError> {
    peek_record_size(input)
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
            "ndb-wal-{}-{}",
            name,
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_records() -> Vec<Record> {
        vec![
            Record::TypeName(TypeNameRecord {
                id: TypeId::new(1),
                name: "Customer".into(),
            }),
            Record::PropertyKey(PropertyKeyRecord {
                id: PropertyId::new(2),
                name: "email".into(),
            }),
            Record::Entity(EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(1),
                tx_id_assert: TxId::new(10),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(
                    PropertyId::new(2),
                    Value::String("alice@example.com".into()),
                )],
            }),
            Record::RoleName(RoleNameRecord {
                id: RoleId::new(3),
                name: "approver".into(),
            }),
            Record::HyperEdge(HyperEdgeRecord {
                hyperedge_id: HyperedgeId::now_v7(),
                type_id: TypeId::new(5),
                tx_id_assert: TxId::new(11),
                tx_id_supersede: TxId::ACTIVE,
                roles: vec![(RoleId::new(3), EntityId::now_v7())],
                properties: vec![],
            }),
            Record::Tombstone(TombstoneRecord {
                target_id: uuid::Uuid::now_v7(),
                tx_id_supersede: TxId::new(12),
            }),
        ]
    }

    #[test]
    fn create_append_replay_round_trip() {
        let dir = temp_dir("create_append_replay");
        let path = dir.join("000001.ndblog");
        let records = sample_records();

        let mut wal = WriteAheadLog::create(&path).unwrap();
        assert!(wal.is_empty());
        let mut lsns = Vec::new();
        for r in &records {
            lsns.push(wal.append(r).unwrap());
        }
        wal.sync().unwrap();
        wal.close().unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let (replayed, recovery) = reader.replay_all().unwrap();
        assert_eq!(recovery.records_read, records.len());
        assert_eq!(recovery.trailing_garbage, 0);
        let restored: Vec<_> = replayed.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(restored, records);
        let actual_lsns: Vec<_> = replayed.iter().map(|(_, lsn)| *lsn).collect();
        assert_eq!(actual_lsns, lsns);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_batch_writes_records_contiguously() {
        let dir = temp_dir("append_batch");
        let path = dir.join("000001.ndblog");
        let records = sample_records();

        let mut wal = WriteAheadLog::create(&path).unwrap();
        let first_lsn = wal.append_batch(&records).unwrap();
        wal.sync().unwrap();
        assert_eq!(first_lsn, 0);

        let mut reader = WalReader::open(&path).unwrap();
        let (replayed, recovery) = reader.replay_all().unwrap();
        assert_eq!(recovery.records_read, records.len());
        let restored: Vec<_> = replayed.into_iter().map(|(r, _)| r).collect();
        assert_eq!(restored, records);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_trailing_record_is_treated_as_partial_write() {
        let dir = temp_dir("truncated_tail");
        let path = dir.join("000001.ndblog");
        let records = sample_records();

        let mut wal = WriteAheadLog::create(&path).unwrap();
        wal.append_batch(&records).unwrap();
        wal.close().unwrap();

        // Lop off the last 7 bytes — partway through the final record.
        let full_len = std::fs::metadata(&path).unwrap().len();
        truncate_to(&path, full_len - 7).unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let recovery = reader.recover().unwrap();
        assert_eq!(recovery.records_read, records.len() - 1);
        assert!(recovery.trailing_garbage > 0);
        // durable_end points to the boundary between the last good record
        // and the trailing garbage.
        assert!(recovery.durable_end < full_len);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_size_prefix_is_partial_write() {
        let dir = temp_dir("truncated_size_prefix");
        let path = dir.join("000001.ndblog");
        let records = sample_records();

        let mut wal = WriteAheadLog::create(&path).unwrap();
        wal.append_batch(&records).unwrap();
        wal.close().unwrap();

        // Append 2 bytes of garbage that don't even form a u32 size prefix.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0xab, 0xcd]).unwrap();
        f.sync_data().unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let recovery = reader.recover().unwrap();
        // All sample records still decode; the 2 trailing bytes are partial.
        assert_eq!(recovery.records_read, records.len());
        assert_eq!(recovery.trailing_garbage, 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mid_stream_corruption_surfaces_as_error() {
        let dir = temp_dir("mid_corruption");
        let path = dir.join("000001.ndblog");
        let records = sample_records();

        let mut wal = WriteAheadLog::create(&path).unwrap();
        // Append records one at a time so we know each record's start LSN.
        let mut lsns = Vec::new();
        for r in &records {
            lsns.push(wal.append(r).unwrap());
        }
        wal.close().unwrap();

        // Corrupt a byte deep inside the third record's payload — past the
        // 4-byte size header so we don't accidentally claim a different
        // record_size (which the reader treats as truncation, not corruption).
        let mut bytes = std::fs::read(&path).unwrap();
        let target = usize::try_from(lsns[2]).unwrap() + 8;
        bytes[target] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let mut errored = false;
        let mut read_count = 0;
        loop {
            match reader.next_record() {
                Ok(Some(_)) => read_count += 1,
                Ok(None) => break,
                Err(WalReadError::Decode { .. }) => {
                    errored = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert!(
            errored,
            "mid-stream corruption must surface as a Decode error"
        );
        assert!(read_count < records.len());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_append_continues_existing_log() {
        let dir = temp_dir("open_append");
        let path = dir.join("000001.ndblog");

        let mut wal = WriteAheadLog::create(&path).unwrap();
        wal.append(&Record::TypeName(TypeNameRecord {
            id: TypeId::new(1),
            name: "A".into(),
        }))
        .unwrap();
        wal.close().unwrap();

        let mut wal = WriteAheadLog::open_append(&path).unwrap();
        wal.append(&Record::TypeName(TypeNameRecord {
            id: TypeId::new(2),
            name: "B".into(),
        }))
        .unwrap();
        wal.close().unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let (replayed, _) = reader.replay_all().unwrap();
        assert_eq!(replayed.len(), 2);
        if let (Record::TypeName(a), Record::TypeName(b)) = (&replayed[0].0, &replayed[1].0) {
            assert_eq!(a.id, TypeId::new(1));
            assert_eq!(a.name, "A");
            assert_eq!(b.id, TypeId::new(2));
            assert_eq!(b.name, "B");
        } else {
            panic!("unexpected record kinds: {replayed:?}");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncate_then_open_append_resumes_at_safe_boundary() {
        let dir = temp_dir("truncate_resume");
        let path = dir.join("000001.ndblog");
        let records = sample_records();

        let mut wal = WriteAheadLog::create(&path).unwrap();
        wal.append_batch(&records).unwrap();
        wal.close().unwrap();
        // Simulate a crash mid-write of an extra record by appending garbage
        // then recovering.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xaa, 0xbb, 0xcc]).unwrap();
            f.sync_data().unwrap();
        }
        let mut reader = WalReader::open(&path).unwrap();
        let recovery = reader.recover().unwrap();
        truncate_to(&path, recovery.durable_end).unwrap();

        // Resume — file must accept new appends and the next reader sees the
        // original records plus the new one.
        let mut wal = WriteAheadLog::open_append(&path).unwrap();
        let new = Record::TypeName(TypeNameRecord {
            id: TypeId::new(999),
            name: "Z".into(),
        });
        wal.append(&new).unwrap();
        wal.close().unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let (replayed, recovery) = reader.replay_all().unwrap();
        assert_eq!(recovery.records_read, records.len() + 1);
        assert_eq!(recovery.trailing_garbage, 0);
        let last = replayed.last().unwrap();
        assert_eq!(last.0, new);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_wal_recovers_cleanly() {
        let dir = temp_dir("empty_wal");
        let path = dir.join("000001.ndblog");
        WriteAheadLog::create(&path).unwrap().close().unwrap();
        let mut reader = WalReader::open(&path).unwrap();
        let recovery = reader.recover().unwrap();
        assert_eq!(recovery.records_read, 0);
        assert_eq!(recovery.durable_end, 0);
        assert_eq!(recovery.trailing_garbage, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
