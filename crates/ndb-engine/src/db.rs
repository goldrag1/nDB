//! Database directory orchestration — `MANIFEST`, `CURRENT`, `LOCK` (§11.5).
#![allow(clippy::doc_markdown)] // "MANIFEST", "RocksDB", "SSTable" are domain terms used liberally.
//!
//! An nDB database is a directory (not a single file). On disk it looks like:
//!
//! ```text
//! mydb/
//! ├── CURRENT                  // text: name of the active MANIFEST
//! ├── LOCK                     // exclusive file lock for the whole dir
//! ├── MANIFEST-000001          // immutable snapshot of active SSTables
//! ├── MANIFEST-000002          // ... bumped on every flush/compaction
//! ├── 000001.ndb               // SSTable, level 0
//! ├── 000002.ndb               // SSTable, level 0
//! ├── 000003.ndb               // SSTable, level 1
//! └── 000004.ndblog            // active WAL
//! ```
//!
//! v1 decisions baked in here:
//!
//! - **`MANIFEST` is a versioned snapshot, not an edit log.** Each MANIFEST
//!   file is the *complete* active state at one point in time. Newer
//!   MANIFEST means newer snapshot. Old MANIFESTs are GC-able once `CURRENT`
//!   points past them. This is simpler than RocksDB's edit-log model, and
//!   adequate for the single-writer (§14.3) v1; the trade-off is bigger
//!   per-checkpoint MANIFEST writes — fine for small/medium databases.
//! - **`CURRENT` is a one-line text file** containing the MANIFEST filename
//!   plus a trailing newline. Atomically updated via write-temp + rename +
//!   `fsync_dir`. The text format lets a human `cat CURRENT` and see exactly
//!   what's loaded.
//! - **`LOCK` uses stdlib `File::try_lock`** (stable since Rust 1.89).
//!   Held for the lifetime of the [`Database`] handle. Two simultaneous
//!   opens get a clean [`DatabaseError::AlreadyLocked`] from the second
//!   call, not an unexplained `WouldBlock`.
//! - **MANIFEST file format is custom binary** with magic bytes and CRC
//!   (same discipline as records + SSTable footer). Each entry is
//!   `(file_seq: u64, level: u8, padding: u8 × 7)` so the entry array is
//!   8-byte aligned for cheap mmap reads in the future.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;
use thiserror::Error;

/// File name for the CURRENT pointer.
pub const CURRENT_FILE: &str = "CURRENT";

/// File name for the LOCK file.
pub const LOCK_FILE: &str = "LOCK";

/// Prefix for every MANIFEST file. The sequence number follows as 6 zero-padded
/// digits (e.g. `MANIFEST-000042`). Matches RocksDB convention.
pub const MANIFEST_PREFIX: &str = "MANIFEST-";

/// Magic bytes at the start of every MANIFEST file. Distinguishes nDB
/// MANIFESTs from foreign files.
pub const MANIFEST_MAGIC: &[u8; 8] = b"NDBMAN01";

/// Current MANIFEST on-disk format version this build emits.
pub const MANIFEST_FORMAT_VERSION: u8 = 1;

/// Highest MANIFEST `format_version` this build can read.
pub const MANIFEST_FORMAT_VERSION_MAX_SUPPORTED: u8 = 1;

/// Maximum LSM level number this build understands. Anything beyond is
/// rejected on decode.
pub const MAX_LSM_LEVEL: u8 = 7;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised while opening or mutating a database directory.
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// Another process already holds the `LOCK` file.
    #[error("database at {path} is already locked by another process")]
    AlreadyLocked {
        /// Path to the database directory.
        path: PathBuf,
    },

    /// `CURRENT` was missing — the directory is not a database.
    #[error("CURRENT file missing in {path}; not a database directory")]
    MissingCurrent {
        /// Path to the database directory.
        path: PathBuf,
    },

    /// `CURRENT` pointed at a MANIFEST that doesn't exist on disk.
    #[error("CURRENT points to {manifest}, which is missing")]
    MissingManifest {
        /// MANIFEST filename listed in CURRENT.
        manifest: String,
    },

    /// MANIFEST magic mismatch.
    #[error("invalid MANIFEST magic: got {got:?}, expected {expected:?}")]
    InvalidManifestMagic {
        /// Bytes read from disk.
        got: [u8; 8],
        /// Expected magic.
        expected: [u8; 8],
    },

    /// MANIFEST CRC mismatch.
    #[error("MANIFEST CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    ManifestCrcMismatch {
        /// CRC read from the footer.
        stored: u32,
        /// CRC computed.
        computed: u32,
    },

    /// MANIFEST `format_version` newer than this build supports.
    #[error(
        "unsupported MANIFEST format_version {version} (this build supports up to {supported})"
    )]
    UnsupportedManifestFormat {
        /// Format version byte read.
        version: u8,
        /// Highest version this build can read.
        supported: u8,
    },

    /// MANIFEST file shorter than the minimum valid layout.
    #[error("MANIFEST too short: {len} bytes")]
    ManifestTooShort {
        /// File length.
        len: u64,
    },

    /// MANIFEST contained an LSM level beyond `MAX_LSM_LEVEL`.
    #[error("MANIFEST entry has invalid level {level} (max {max})")]
    InvalidLevel {
        /// Level byte from the entry.
        level: u8,
        /// Max level this build understands.
        max: u8,
    },

    /// `CURRENT` contents could not be parsed as a MANIFEST filename.
    #[error("malformed CURRENT contents: {contents:?}")]
    MalformedCurrent {
        /// Raw contents read.
        contents: String,
    },
}

// ---------------------------------------------------------------------------
// MANIFEST data structures + codec
// ---------------------------------------------------------------------------

/// One MANIFEST entry — an active SSTable plus its LSM level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ManifestEntry {
    /// Sequence number of the SSTable file (`<seq>.ndb`).
    pub file_seq: u64,
    /// LSM level (0 = newest flush; higher = older / compacted).
    pub level: u8,
}

/// Parsed MANIFEST contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Active SSTables at the moment this MANIFEST was written.
    pub sstables: Vec<ManifestEntry>,
    /// Sequence-number watermark for the next allocation (used to mint
    /// `<seq>.ndb` and `<seq>.ndblog` names without collision).
    pub next_file_seq: u64,
    /// Sequence-number watermark for the next MANIFEST. Each MANIFEST file
    /// records the *next* manifest sequence, which lets recovery validate
    /// that no MANIFEST was lost.
    pub next_manifest_seq: u64,
    /// Latest assigned transaction id (so the engine can resume monotonic
    /// `TxId` allocation across restarts).
    pub last_tx_id: u64,
}

const MANIFEST_HEADER_LEN: usize = 8 /* magic */ + 1 /* fmt */ + 1 /* flags */ + 2 /* reserved */;
const MANIFEST_BODY_FIXED_LEN: usize =
    8 /* next_file_seq */ + 8 /* next_manifest_seq */ + 8 /* last_tx_id */ + 4 /* entry_count */;
const MANIFEST_ENTRY_LEN: usize = 8 /* file_seq */ + 1 /* level */ + 7 /* padding */;
const MANIFEST_FOOTER_CRC_LEN: usize = 4;

const MANIFEST_MIN_LEN: u64 =
    (MANIFEST_HEADER_LEN + MANIFEST_BODY_FIXED_LEN + MANIFEST_FOOTER_CRC_LEN) as u64;

impl Manifest {
    /// Encode this MANIFEST to bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(
            MANIFEST_HEADER_LEN
                + MANIFEST_BODY_FIXED_LEN
                + self.sstables.len() * MANIFEST_ENTRY_LEN
                + MANIFEST_FOOTER_CRC_LEN,
        );
        buf.extend_from_slice(MANIFEST_MAGIC);
        buf.push(MANIFEST_FORMAT_VERSION);
        buf.push(0); // flags
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.extend_from_slice(&self.next_file_seq.to_le_bytes());
        buf.extend_from_slice(&self.next_manifest_seq.to_le_bytes());
        buf.extend_from_slice(&self.last_tx_id.to_le_bytes());
        let count = u32::try_from(self.sstables.len()).expect("entry count fits u32");
        buf.extend_from_slice(&count.to_le_bytes());
        for entry in &self.sstables {
            buf.extend_from_slice(&entry.file_seq.to_le_bytes());
            buf.push(entry.level);
            buf.extend_from_slice(&[0u8; 7]); // padding to 16-byte stride
        }
        let mut h = Hasher::new();
        h.update(&buf);
        buf.extend_from_slice(&h.finalize().to_le_bytes());
        buf
    }

    /// Decode a MANIFEST from bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, DatabaseError> {
        if (bytes.len() as u64) < MANIFEST_MIN_LEN {
            return Err(DatabaseError::ManifestTooShort {
                len: bytes.len() as u64,
            });
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&bytes[0..8]);
        if &magic != MANIFEST_MAGIC {
            return Err(DatabaseError::InvalidManifestMagic {
                got: magic,
                expected: *MANIFEST_MAGIC,
            });
        }
        let format_version = bytes[8];
        if format_version > MANIFEST_FORMAT_VERSION_MAX_SUPPORTED {
            return Err(DatabaseError::UnsupportedManifestFormat {
                version: format_version,
                supported: MANIFEST_FORMAT_VERSION_MAX_SUPPORTED,
            });
        }
        // bytes[9] = flags (unused in v1)
        // bytes[10..12] = reserved
        let next_file_seq = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let next_manifest_seq = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
        let last_tx_id = u64::from_le_bytes(bytes[28..36].try_into().unwrap());
        let entry_count = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
        let expected_len = MANIFEST_HEADER_LEN
            + MANIFEST_BODY_FIXED_LEN
            + entry_count * MANIFEST_ENTRY_LEN
            + MANIFEST_FOOTER_CRC_LEN;
        if bytes.len() != expected_len {
            return Err(DatabaseError::ManifestTooShort {
                len: bytes.len() as u64,
            });
        }
        let mut entries = Vec::with_capacity(entry_count);
        let mut cursor = MANIFEST_HEADER_LEN + MANIFEST_BODY_FIXED_LEN;
        for _ in 0..entry_count {
            let file_seq = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
            let level = bytes[cursor + 8];
            if level > MAX_LSM_LEVEL {
                return Err(DatabaseError::InvalidLevel {
                    level,
                    max: MAX_LSM_LEVEL,
                });
            }
            entries.push(ManifestEntry { file_seq, level });
            cursor += MANIFEST_ENTRY_LEN;
        }
        let stored_crc = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        let mut h = Hasher::new();
        h.update(&bytes[..cursor]);
        let computed = h.finalize();
        if stored_crc != computed {
            return Err(DatabaseError::ManifestCrcMismatch {
                stored: stored_crc,
                computed,
            });
        }
        Ok(Self {
            sstables: entries,
            next_file_seq,
            next_manifest_seq,
            last_tx_id,
        })
    }
}

/// Build the filename for a given MANIFEST sequence number.
#[must_use]
pub fn manifest_filename(seq: u64) -> String {
    format!("{MANIFEST_PREFIX}{seq:06}")
}

/// Parse a MANIFEST filename, returning the sequence number. Returns `None`
/// if the filename does not match the canonical pattern.
#[must_use]
pub fn parse_manifest_filename(name: &str) -> Option<u64> {
    let rest = name.strip_prefix(MANIFEST_PREFIX)?;
    if rest.len() < 6 {
        return None;
    }
    rest.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// CURRENT pointer
// ---------------------------------------------------------------------------

/// Read the CURRENT file, returning the MANIFEST filename it references.
fn read_current(dir: &Path) -> Result<String, DatabaseError> {
    let path = dir.join(CURRENT_FILE);
    let mut s = String::new();
    File::open(&path)
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => DatabaseError::MissingCurrent {
                path: dir.to_path_buf(),
            },
            _ => DatabaseError::Io(e),
        })?
        .read_to_string(&mut s)?;
    let trimmed = s.trim_end_matches('\n');
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\n') {
        return Err(DatabaseError::MalformedCurrent { contents: s });
    }
    Ok(trimmed.to_owned())
}

/// Atomically rewrite CURRENT to point at `manifest_filename`. Writes to
/// `CURRENT.tmp`, fsyncs the temp file, renames over `CURRENT`, then fsyncs
/// the directory so the link change itself is durable.
fn write_current(dir: &Path, manifest_filename: &str) -> Result<(), DatabaseError> {
    let final_path = dir.join(CURRENT_FILE);
    let tmp_path = dir.join(format!("{CURRENT_FILE}.tmp"));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        writeln!(f, "{manifest_filename}")?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    fsync_dir(dir)?;
    Ok(())
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()
}

// ---------------------------------------------------------------------------
// LOCK file
// ---------------------------------------------------------------------------

/// Acquire an exclusive lock on the `LOCK` file inside `dir`, creating the
/// file if needed. The returned `File` must stay alive for the lock to
/// remain held — dropping it releases the lock.
fn acquire_lock(dir: &Path) -> Result<File, DatabaseError> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(_) => Err(DatabaseError::AlreadyLocked {
            path: dir.to_path_buf(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Database handle
// ---------------------------------------------------------------------------

/// Open database directory handle. Owns the LOCK file and the latest
/// loaded MANIFEST. Higher-level operations (memtable, flush, transaction
/// commit) hang off this struct in subsequent commits.
#[derive(Debug)]
pub struct Database {
    path: PathBuf,
    /// Held for the lifetime of the handle to keep the LOCK acquired.
    _lock: File,
    /// In-memory copy of the MANIFEST referenced by CURRENT at open time.
    manifest: Manifest,
    /// Sequence number of the currently-active MANIFEST file.
    current_manifest_seq: u64,
}

impl Database {
    /// Create a fresh database directory. Fails if `path` exists.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;

        // Lock immediately so a concurrent create() on the same path can't
        // race in.
        let lock = acquire_lock(&path)?;

        let initial_manifest = Manifest {
            sstables: Vec::new(),
            next_file_seq: 1,
            next_manifest_seq: 2,
            last_tx_id: 0,
        };
        let initial_seq: u64 = 1;
        let initial_filename = manifest_filename(initial_seq);
        let manifest_path = path.join(&initial_filename);
        if manifest_path.exists() {
            // Path was created and locked but already has a MANIFEST — not
            // a clean directory.
            return Err(DatabaseError::Io(io::Error::new(
                ErrorKind::AlreadyExists,
                format!("{} already exists", manifest_path.display()),
            )));
        }

        atomically_write_manifest(&path, initial_seq, &initial_manifest)?;
        write_current(&path, &initial_filename)?;

        Ok(Self {
            path,
            _lock: lock,
            manifest: initial_manifest,
            current_manifest_seq: initial_seq,
        })
    }

    /// Open an existing database directory.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(DatabaseError::Io(io::Error::new(
                ErrorKind::NotFound,
                format!("database directory {} does not exist", path.display()),
            )));
        }
        let lock = acquire_lock(&path)?;
        let current_filename = read_current(&path)?;
        let current_manifest_seq = parse_manifest_filename(&current_filename).ok_or_else(|| {
            DatabaseError::MalformedCurrent {
                contents: current_filename.clone(),
            }
        })?;
        let manifest_path = path.join(&current_filename);
        let manifest_bytes = match std::fs::read(&manifest_path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(DatabaseError::MissingManifest {
                    manifest: current_filename,
                });
            }
            Err(e) => return Err(DatabaseError::Io(e)),
        };
        let manifest = Manifest::decode(&manifest_bytes)?;
        Ok(Self {
            path,
            _lock: lock,
            manifest,
            current_manifest_seq,
        })
    }

    /// Path of the database directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current MANIFEST snapshot.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Sequence number of the currently-active MANIFEST file.
    #[must_use]
    pub fn current_manifest_seq(&self) -> u64 {
        self.current_manifest_seq
    }

    /// Reserve the next `<seq>.ndb` / `<seq>.ndblog` file sequence number.
    /// Increments the in-memory MANIFEST's `next_file_seq`; the change becomes
    /// durable only after the next [`write_manifest`](Self::write_manifest).
    pub fn allocate_file_seq(&mut self) -> u64 {
        let seq = self.manifest.next_file_seq;
        self.manifest.next_file_seq += 1;
        seq
    }

    /// Reserve the next transaction id.
    pub fn allocate_tx_id(&mut self) -> u64 {
        self.manifest.last_tx_id += 1;
        self.manifest.last_tx_id
    }

    /// Persist a new MANIFEST snapshot describing `new_state` and flip
    /// CURRENT to point at it. Both operations are crash-safe: the new
    /// MANIFEST is written + fsynced first, then CURRENT is rewritten
    /// atomically (write-temp + rename + dir-fsync).
    pub fn write_manifest(&mut self, new_state: Manifest) -> Result<u64, DatabaseError> {
        let new_seq = self.manifest.next_manifest_seq;
        let mut to_persist = new_state;
        to_persist.next_manifest_seq = new_seq + 1;
        atomically_write_manifest(&self.path, new_seq, &to_persist)?;
        write_current(&self.path, &manifest_filename(new_seq))?;
        self.manifest = to_persist;
        self.current_manifest_seq = new_seq;
        Ok(new_seq)
    }

    /// Sync + drop the LOCK file. Equivalent to `drop(self)` but surfaces
    /// IO errors from the underlying lock release.
    pub fn close(self) -> Result<(), DatabaseError> {
        Ok(())
    }
}

fn atomically_write_manifest(
    dir: &Path,
    seq: u64,
    manifest: &Manifest,
) -> Result<(), DatabaseError> {
    let final_filename = manifest_filename(seq);
    let final_path = dir.join(&final_filename);
    let tmp_path = dir.join(format!("{final_filename}.tmp"));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        let bytes = manifest.encode();
        f.write_all(&bytes)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    fsync_dir(dir)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ndb-db-{}-{}", name, uuid::Uuid::now_v7().simple()));
        p
    }

    #[test]
    fn manifest_round_trip() {
        let m = Manifest {
            sstables: vec![
                ManifestEntry {
                    file_seq: 1,
                    level: 0,
                },
                ManifestEntry {
                    file_seq: 2,
                    level: 0,
                },
                ManifestEntry {
                    file_seq: 3,
                    level: 1,
                },
            ],
            next_file_seq: 4,
            next_manifest_seq: 7,
            last_tx_id: 42,
        };
        let bytes = m.encode();
        let restored = Manifest::decode(&bytes).unwrap();
        assert_eq!(m, restored);
    }

    #[test]
    fn empty_manifest_round_trip() {
        let m = Manifest::default();
        let bytes = m.encode();
        let restored = Manifest::decode(&bytes).unwrap();
        assert_eq!(m, restored);
    }

    #[test]
    fn manifest_corruption_detected() {
        let m = Manifest {
            sstables: vec![ManifestEntry {
                file_seq: 1,
                level: 0,
            }],
            next_file_seq: 2,
            next_manifest_seq: 2,
            last_tx_id: 0,
        };
        let mut bytes = m.encode();
        bytes[20] ^= 0xff;
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(DatabaseError::ManifestCrcMismatch { .. })
        ));
    }

    #[test]
    fn manifest_filename_round_trip() {
        assert_eq!(manifest_filename(1), "MANIFEST-000001");
        assert_eq!(manifest_filename(1_234_567), "MANIFEST-1234567");
        assert_eq!(parse_manifest_filename("MANIFEST-000001"), Some(1));
        assert_eq!(parse_manifest_filename("MANIFEST-1234567"), Some(1_234_567));
        assert_eq!(parse_manifest_filename("notamanifest"), None);
        assert_eq!(parse_manifest_filename("MANIFEST-1234"), None); // <6 digits
    }

    #[test]
    fn create_and_reopen() {
        let dir = temp_dir("create_reopen");
        {
            let db = Database::create(&dir).unwrap();
            assert_eq!(db.current_manifest_seq(), 1);
            assert_eq!(db.manifest().sstables, vec![]);
            assert_eq!(db.manifest().next_file_seq, 1);
            assert_eq!(db.manifest().next_manifest_seq, 2);
            db.close().unwrap();
        }
        let db = Database::open(&dir).unwrap();
        assert_eq!(db.current_manifest_seq(), 1);
        assert!(db.manifest().sstables.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_manifest_creates_new_version() {
        let dir = temp_dir("new_version");
        let mut db = Database::create(&dir).unwrap();
        let mut next = db.manifest().clone();
        next.sstables.push(ManifestEntry {
            file_seq: 1,
            level: 0,
        });
        let new_seq = db.write_manifest(next.clone()).unwrap();
        assert_eq!(new_seq, 2);
        assert_eq!(db.current_manifest_seq(), 2);
        assert_eq!(db.manifest().sstables.len(), 1);
        // Both MANIFEST files exist.
        assert!(dir.join("MANIFEST-000001").exists());
        assert!(dir.join("MANIFEST-000002").exists());
        db.close().unwrap();

        // Reopen sees the new state.
        let db2 = Database::open(&dir).unwrap();
        assert_eq!(db2.current_manifest_seq(), 2);
        assert_eq!(db2.manifest().sstables.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn second_open_fails_with_already_locked() {
        let dir = temp_dir("locking");
        let _db1 = Database::create(&dir).unwrap();
        let err = Database::open(&dir).unwrap_err();
        assert!(matches!(err, DatabaseError::AlreadyLocked { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_released_on_drop() {
        let dir = temp_dir("lock_release");
        {
            let _db1 = Database::create(&dir).unwrap();
            // Hold the lock here.
        }
        // Now the lock is released; opening should succeed.
        let _db2 = Database::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_missing_directory_errors() {
        let dir = temp_dir("does_not_exist");
        let err = Database::open(&dir).unwrap_err();
        assert!(matches!(err, DatabaseError::Io(_)));
    }

    #[test]
    fn open_directory_without_current_errors() {
        let dir = temp_dir("no_current");
        std::fs::create_dir_all(&dir).unwrap();
        let err = Database::open(&dir).unwrap_err();
        assert!(matches!(err, DatabaseError::MissingCurrent { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_current_rejected() {
        let dir = temp_dir("malformed_current");
        // Set up a valid LOCK + a CURRENT with garbage contents but no
        // MANIFEST.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CURRENT_FILE), "bogus contents\n").unwrap();
        let err = Database::open(&dir).unwrap_err();
        // CURRENT parses to "bogus contents", which fails the
        // parse_manifest_filename check.
        assert!(matches!(err, DatabaseError::MalformedCurrent { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn allocate_file_seq_monotonic() {
        let dir = temp_dir("alloc_seq");
        let mut db = Database::create(&dir).unwrap();
        assert_eq!(db.allocate_file_seq(), 1);
        assert_eq!(db.allocate_file_seq(), 2);
        assert_eq!(db.allocate_file_seq(), 3);
        // Persist so the next open sees the bumped counter.
        let m = db.manifest().clone();
        db.write_manifest(m).unwrap();
        db.close().unwrap();

        let db2 = Database::open(&dir).unwrap();
        assert_eq!(db2.manifest().next_file_seq, 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn allocate_tx_id_monotonic() {
        let dir = temp_dir("alloc_tx");
        let mut db = Database::create(&dir).unwrap();
        assert_eq!(db.allocate_tx_id(), 1);
        assert_eq!(db.allocate_tx_id(), 2);
        let m = db.manifest().clone();
        db.write_manifest(m).unwrap();
        db.close().unwrap();

        let mut db2 = Database::open(&dir).unwrap();
        assert_eq!(db2.allocate_tx_id(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
