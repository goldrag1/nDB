//! At-rest encryption primitives for nDB (§13.4).
//!
//! Provides:
//!
//! - [`Cipher`] — AES-GCM-256 wrapper with a 32-byte symmetric key. The key
//!   typically comes from the operator's KMS (via the `NDB_ENC_KEY` env
//!   var in hex form, or a [`Cipher::from_raw_key`] constructor for
//!   tests).
//!
//! - [`EncryptedFile`] — a chunked-AEAD `Read + Write` wrapper that turns
//!   a stream of plaintext bytes into a stream of `[u32 chunk_len | u96
//!   nonce | ciphertext | u128 tag]` records on disk. Drop-in
//!   replacement for `File` in any code that needs at-rest encryption
//!   without learning a new I/O model.
//!
//! # v1 scope baked in here
//!
//! - **Algorithm:** AES-256-GCM only. No agility yet — adding new ciphers
//!   later requires bumping `ENCRYPTED_FILE_FORMAT_VERSION`.
//! - **Key sourcing:** caller's problem. The recommended path is the
//!   `NDB_ENC_KEY` env var (hex-encoded 32 bytes); production deployments
//!   are expected to source this from an external KMS via standard env-var
//!   injection.
//! - **Chunked AEAD over a stream.** Each chunk is up to 4 KiB plaintext;
//!   each chunk is authenticated independently with a fresh random nonce.
//!   This lets the reader fail fast on a single tampered chunk without
//!   needing to authenticate the entire file end-to-end.
//! - **File header is plaintext.** The magic + version + chunk-size are
//!   not encrypted — they're the protocol envelope that tells a reader
//!   how to interpret the rest. Tampering with them changes the chunk
//!   layout but doesn't yield ciphertext-level information.
//! - **Backward-compatible opening.** A file whose first 4 bytes do not
//!   match `ENCRYPTED_FILE_MAGIC` is treated as plain bytes; encrypted
//!   and unencrypted files coexist in the same database directory.
//!
//! # Integration status
//!
//! v1 ships the primitive + the chunked-file wrapper as a reusable
//! library. Wiring [`EncryptedFile`] into the `WAL` and `SSTable` I/O paths is
//! a focused follow-on. The shape is intentionally drop-in: any
//! `Read`/`Write` consumer can swap `File` for `EncryptedFile` without
//! API changes elsewhere.

use std::io::{self, Read, Write};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand_core::{OsRng, RngCore};
use thiserror::Error;

/// 4-byte magic at the start of every encrypted file. Spells "encrypted nDB" in
/// the byte sequence the operator sees when they hex-dump a file.
pub const ENCRYPTED_FILE_MAGIC: u32 = 0xE5DB_E5DB;

/// On-disk format version for encrypted files. Bumped when the chunk
/// layout, AEAD algorithm, or nonce strategy changes.
pub const ENCRYPTED_FILE_FORMAT_VERSION: u32 = 1;

/// Default plaintext chunk size, 4 KiB. Tuned for the page-cache sweet spot.
pub const DEFAULT_CHUNK_SIZE: u32 = 4096;

/// AES-256-GCM nonce length in bytes.
pub const NONCE_LEN: usize = 12;

/// AES-256-GCM authentication tag length in bytes.
pub const TAG_LEN: usize = 16;

/// AES-256 key length in bytes.
pub const KEY_LEN: usize = 32;

/// Header size in bytes: magic + version + chunk size + reserved.
const HEADER_LEN: usize = 16;

/// Per-chunk overhead on disk: 4-byte plaintext-len + 12-byte nonce +
/// 16-byte GCM tag.
#[cfg(test)]
const CHUNK_OVERHEAD: usize = 4 + NONCE_LEN + TAG_LEN;

/// Errors raised by the encryption layer.
#[derive(Debug, Error)]
pub enum EncryptionError {
    /// AEAD encryption / decryption failure (corruption, tampering, or
    /// wrong key).
    #[error("AEAD failure (likely tampering or wrong key)")]
    Aead,

    /// I/O failure during read or write.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// The file does not have the encrypted-file magic header.
    #[error("not an encrypted file (magic mismatch)")]
    NotEncrypted,

    /// The file's format version is unsupported.
    #[error("unsupported encrypted-file format version: {0}")]
    UnsupportedVersion(u32),

    /// Encryption marker file is shorter than the expected layout.
    #[error("encryption marker truncated: expected {expected} bytes, got {got}")]
    MarkerTruncated {
        /// Required marker length.
        expected: usize,
        /// Bytes actually read.
        got: usize,
    },

    /// Encryption marker file magic header doesn't match.
    #[error("encryption marker bad magic: 0x{got:08x}")]
    MarkerBadMagic {
        /// The bytes we observed at offset 0.
        got: u32,
    },

    /// Encryption marker's `format_version` field isn't one we recognise.
    #[error("encryption marker unsupported format version: {0}")]
    MarkerUnsupportedVersion(u32),

    /// Encryption marker's `algorithm` field isn't one we recognise.
    #[error("encryption marker unsupported algorithm id: {0}")]
    MarkerUnsupportedAlgorithm(u32),

    /// Hex decoding of `NDB_ENC_KEY` failed.
    #[error("invalid hex key: {0}")]
    KeyHex(String),

    /// Key length is wrong (must be 32 bytes / 64 hex chars).
    #[error("key must be {expected} bytes, got {got}")]
    KeyLength {
        /// Expected number of bytes (32).
        expected: usize,
        /// Number of bytes actually supplied.
        got: usize,
    },
}

// ---------------------------------------------------------------------------
// Cipher
// ---------------------------------------------------------------------------

/// AES-GCM-256 cipher. Cheap to clone; share across threads via `Arc`.
#[derive(Clone)]
pub struct Cipher {
    inner: Aes256Gcm,
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the key.
        f.debug_struct("Cipher").finish_non_exhaustive()
    }
}

impl Cipher {
    /// Build a cipher from a raw 32-byte key. Caller is responsible for
    /// sourcing the key bytes securely.
    pub fn from_raw_key(key: &[u8]) -> Result<Self, EncryptionError> {
        if key.len() != KEY_LEN {
            return Err(EncryptionError::KeyLength {
                expected: KEY_LEN,
                got: key.len(),
            });
        }
        let key = aes_gcm::Key::<Aes256Gcm>::from_slice(key);
        Ok(Self {
            inner: Aes256Gcm::new(key),
        })
    }

    /// Build a cipher from a hex-encoded 64-character key (32 bytes).
    pub fn from_hex(hex: &str) -> Result<Self, EncryptionError> {
        let raw = decode_hex(hex)?;
        Self::from_raw_key(&raw)
    }

    /// Look for `NDB_ENC_KEY` in the environment. Returns `Ok(None)` if
    /// the variable is unset (the operator has opted out of engine-level
    /// encryption — filesystem encryption is presumed at the OS layer).
    /// Returns `Ok(Some(cipher))` when the env var is present and well
    /// formed.
    pub fn from_env() -> Result<Option<Self>, EncryptionError> {
        match std::env::var("NDB_ENC_KEY") {
            Ok(v) if !v.is_empty() => Ok(Some(Self::from_hex(&v)?)),
            _ => Ok(None),
        }
    }

    /// Encrypt `plaintext` with a freshly-generated random nonce. Output
    /// layout: `nonce (12 bytes) || ciphertext+tag`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ct = self
            .inner
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| EncryptionError::Aead)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt the `[nonce || ciphertext+tag]` blob produced by
    /// [`Cipher::encrypt`]. Tampering, truncation, or wrong-key yields
    /// `EncryptionError::Aead`.
    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(EncryptionError::Aead);
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        self.inner
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| EncryptionError::Aead)
    }

    /// Deterministic 16-byte key fingerprint — AES-GCM the fixed
    /// plaintext `b"ndb-fingerprint!"` with an all-zero nonce + empty
    /// AAD; take the 16-byte authentication tag.
    ///
    /// **Important:** uses a static nonce, which would be insecure for
    /// general AEAD encryption (nonce reuse leaks plaintext XORs). It is
    /// safe HERE because the plaintext is also static and never carries
    /// secret material — the purpose is purely "did the operator load
    /// the same key as last time?"
    ///
    /// Different keys produce different fingerprints with overwhelming
    /// probability (collision space ≈ 2^128). The fingerprint is stored
    /// in the encryption marker file; on engine open it's compared
    /// against the running cipher's fingerprint to refuse wrong-key
    /// opens before any actual decryption fails.
    #[must_use]
    pub fn fingerprint(&self) -> [u8; FINGERPRINT_LEN] {
        const PLAINTEXT: &[u8; 16] = b"ndb-fingerprint!";
        let nonce = [0u8; NONCE_LEN];
        let ct = self
            .inner
            .encrypt(Nonce::from_slice(&nonce), &PLAINTEXT[..])
            .expect("AES-GCM encrypt over fixed input cannot fail");
        // ct = ciphertext (16 bytes) || tag (16 bytes). Take the tag —
        // it commits to both plaintext and key, so it's a clean fingerprint.
        let mut out = [0u8; FINGERPRINT_LEN];
        out.copy_from_slice(&ct[ct.len() - FINGERPRINT_LEN..]);
        out
    }
}

/// Length of [`Cipher::fingerprint`] output. 16 bytes (AES-GCM tag size).
pub const FINGERPRINT_LEN: usize = 16;

fn decode_hex(s: &str) -> Result<Vec<u8>, EncryptionError> {
    if !s.len().is_multiple_of(2) {
        return Err(EncryptionError::KeyHex(format!(
            "odd hex length ({} chars)",
            s.len()
        )));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let hi = hex_nybble(chunk[0])?;
        let lo = hex_nybble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nybble(b: u8) -> Result<u8, EncryptionError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(EncryptionError::KeyHex(format!(
            "non-hex character 0x{other:02x}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// EncryptedFile
// ---------------------------------------------------------------------------

/// Mode of operation for [`EncryptedFile`].
enum Mode {
    Writing {
        buffer: Vec<u8>,
        chunk_size: usize,
    },
    Reading {
        remaining: Vec<u8>,
        /// Chunk size declared by the file header. Currently unused at
        /// read time (the per-chunk `plaintext_len` field is the source of
        /// truth for each chunk's size) but kept so we can validate the
        /// header / bound future seek logic.
        _chunk_size: usize,
        exhausted: bool,
    },
}

/// A `Read + Write` wrapper that transparently AEAD-encrypts chunked
/// blocks. Pair with any inner `Read + Write` (typically a `File`):
///
/// ```ignore
/// let cipher = Cipher::from_env()?.unwrap();
/// let file = std::fs::File::create("/path/to/db/000001.ndblog")?;
/// let mut enc = EncryptedFile::create(file, cipher.clone(), DEFAULT_CHUNK_SIZE)?;
/// enc.write_all(record_bytes)?;
/// enc.finish()?; // emits the trailing partial chunk
/// ```
///
/// Writer holds an in-memory buffer of up to `chunk_size` plaintext bytes.
/// When the buffer fills, a chunk is flushed. The reader is symmetric:
/// it reads one whole on-disk chunk, decrypts, and yields the plaintext
/// to callers via the `Read` trait.
///
/// Partial reads / writes are supported transparently — the wrapper
/// hides chunk boundaries from the caller.
pub struct EncryptedFile<F> {
    inner: F,
    cipher: Cipher,
    mode: Mode,
}

impl<F> std::fmt::Debug for EncryptedFile<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedFile")
            .field(
                "mode",
                &match self.mode {
                    Mode::Writing { .. } => "writing",
                    Mode::Reading { .. } => "reading",
                },
            )
            .finish_non_exhaustive()
    }
}

impl<F: Write> EncryptedFile<F> {
    /// Open a new encrypted file for writing. Writes the plaintext header
    /// (`magic + version + chunk_size + reserved`) immediately.
    pub fn create(mut inner: F, cipher: Cipher, chunk_size: u32) -> Result<Self, EncryptionError> {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&ENCRYPTED_FILE_MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&ENCRYPTED_FILE_FORMAT_VERSION.to_le_bytes());
        header[8..12].copy_from_slice(&chunk_size.to_le_bytes());
        // reserved = 0
        inner.write_all(&header)?;
        Ok(Self {
            inner,
            cipher,
            mode: Mode::Writing {
                buffer: Vec::with_capacity(chunk_size as usize),
                chunk_size: chunk_size as usize,
            },
        })
    }

    /// Flush any in-progress chunk and finalise the file. Must be called
    /// before drop to commit the trailing partial chunk.
    pub fn finish(mut self) -> Result<F, EncryptionError> {
        self.flush_chunk()?;
        Ok(self.inner)
    }

    /// Seal the in-progress chunk (if any) WITHOUT consuming the file.
    /// Used by the WAL writer's `sync()` path — the in-memory plaintext
    /// buffer would otherwise be lost on crash. Idempotent: no-op when
    /// the buffer is empty or when called on a reader.
    pub fn flush_pending(&mut self) -> Result<(), EncryptionError> {
        self.flush_chunk()
    }

    /// Mutable access to the underlying writer. Used so the WAL sync
    /// path can call `sync_data()` on the wrapped `File` after sealing
    /// the in-progress chunk.
    pub fn inner_mut(&mut self) -> &mut F {
        &mut self.inner
    }

    fn flush_chunk(&mut self) -> Result<(), EncryptionError> {
        let Mode::Writing { buffer, .. } = &mut self.mode else {
            return Ok(());
        };
        if buffer.is_empty() {
            return Ok(());
        }
        let plaintext_len = u32::try_from(buffer.len()).expect("chunk fits in u32");
        let blob = self.cipher.encrypt(buffer)?;
        self.inner.write_all(&plaintext_len.to_le_bytes())?;
        self.inner.write_all(&blob)?;
        buffer.clear();
        Ok(())
    }
}

impl<F: Read> EncryptedFile<F> {
    /// Open an existing encrypted file for reading. Reads the plaintext
    /// header up-front and validates magic + version.
    pub fn open(mut inner: F, cipher: Cipher) -> Result<Self, EncryptionError> {
        let mut header = [0u8; HEADER_LEN];
        inner.read_exact(&mut header)?;
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != ENCRYPTED_FILE_MAGIC {
            return Err(EncryptionError::NotEncrypted);
        }
        let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if version != ENCRYPTED_FILE_FORMAT_VERSION {
            return Err(EncryptionError::UnsupportedVersion(version));
        }
        let chunk_size = u32::from_le_bytes(header[8..12].try_into().unwrap());
        Ok(Self {
            inner,
            cipher,
            mode: Mode::Reading {
                remaining: Vec::new(),
                _chunk_size: chunk_size as usize,
                exhausted: false,
            },
        })
    }

    /// Try to detect whether a file is encrypted by sniffing the first
    /// four bytes. Caller is expected to seek back to position 0 if they
    /// want to subsequently `open`. Returns `Ok(true)` if magic matches,
    /// `Ok(false)` if it does not, error on I/O failure.
    pub fn sniff_magic<R: Read>(mut r: R) -> io::Result<bool> {
        let mut buf = [0u8; 4];
        let n = r.read(&mut buf)?;
        if n < 4 {
            return Ok(false);
        }
        let magic = u32::from_le_bytes(buf);
        Ok(magic == ENCRYPTED_FILE_MAGIC)
    }
}

impl<F: Write> Write for EncryptedFile<F> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let initial = buf.len();
        let chunk_size = match &self.mode {
            Mode::Writing { chunk_size, .. } => *chunk_size,
            Mode::Reading { .. } => {
                return Err(io::Error::other("write on read-mode EncryptedFile"));
            }
        };
        while !buf.is_empty() {
            let Mode::Writing { buffer, .. } = &mut self.mode else {
                unreachable!();
            };
            let space = chunk_size - buffer.len();
            let take = space.min(buf.len());
            buffer.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            if buffer.len() == chunk_size {
                self.flush_chunk()
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
        }
        Ok(initial)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Note: callers wanting durable partial-chunk writes should call
        // `finish()` instead. `flush()` just forwards to the inner stream.
        if let Mode::Writing { .. } = &self.mode {
            self.inner.flush()?;
        }
        Ok(())
    }
}

impl<F: Read> Read for EncryptedFile<F> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        loop {
            let Mode::Reading {
                remaining,
                exhausted,
                ..
            } = &mut self.mode
            else {
                return Err(io::Error::other("read on write-mode EncryptedFile"));
            };
            if !remaining.is_empty() {
                let take = remaining.len().min(dst.len());
                dst[..take].copy_from_slice(&remaining[..take]);
                remaining.drain(..take);
                return Ok(take);
            }
            if *exhausted {
                return Ok(0);
            }
            // Pull the next chunk off the wire.
            self.read_one_chunk()?;
        }
    }
}

impl<F: Read> EncryptedFile<F> {
    fn read_one_chunk(&mut self) -> io::Result<()> {
        let Mode::Reading {
            remaining,
            exhausted,
            ..
        } = &mut self.mode
        else {
            return Err(io::Error::other("internal: read on writer"));
        };
        let mut len_buf = [0u8; 4];
        match self.inner.read(&mut len_buf)? {
            0 => {
                *exhausted = true;
                return Ok(());
            }
            n if n < 4 => {
                self.inner.read_exact(&mut len_buf[n..])?;
            }
            _ => {}
        }
        let plaintext_len = u32::from_le_bytes(len_buf) as usize;
        // Each chunk on disk: nonce + ciphertext_len + tag where
        // ciphertext_len = plaintext_len (AES-GCM ciphertext == plaintext
        // length; tag is separate).
        let blob_len = NONCE_LEN + plaintext_len + TAG_LEN;
        let mut blob = vec![0u8; blob_len];
        self.inner.read_exact(&mut blob)?;
        let plain = self
            .cipher
            .decrypt(&blob)
            .map_err(|e| io::Error::other(e.to_string()))?;
        *remaining = plain;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EncryptionMarker — `<db>/.encryption` file on disk
// ---------------------------------------------------------------------------

/// 4-byte magic at the start of `.encryption`: spells "NDEM" (nDB Encrypted
/// Marker).
pub const ENCRYPTION_MARKER_MAGIC: u32 = 0x4D45_444E;

/// On-disk format version of the marker file.
pub const ENCRYPTION_MARKER_FORMAT_VERSION: u32 = 1;

/// Algorithm id stored in the marker. Currently only AES-GCM-256 = 1.
pub const ENCRYPTION_ALGO_AES_GCM_256: u32 = 1;

/// Filename used inside the database directory.
pub const ENCRYPTION_MARKER_FILENAME: &str = ".encryption";

/// Transient marker written by `Engine::reencrypt` before the rewrite
/// loop starts and removed once the migration completes. Its presence
/// on `Engine::open` signals an interrupted migration; the engine
/// refuses to open silently in that case.
pub const ENCRYPTION_MIGRATION_FILENAME: &str = ".encryption.next";

/// On-disk layout: `magic(4)` + `version(4)` + `algo(4)` + `chunk_size(4)`
/// + `fingerprint(16)` + `reserved(32)` = 64 bytes.
const MARKER_LEN: usize = 64;

/// Engine encryption marker, stored at `<db>/.encryption`.
///
/// Presence of this file means the database WAS encrypted on its last
/// flush. The cipher loaded at open must produce a matching fingerprint
/// (compared byte-for-byte against [`Self::fingerprint`]); otherwise the
/// engine refuses to open with an "encryption-key-mismatch" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptionMarker {
    /// On-disk format version. Bumped when this layout evolves.
    pub format_version: u32,
    /// Algorithm id — see [`ENCRYPTION_ALGO_AES_GCM_256`].
    pub algorithm: u32,
    /// Chunk size (plaintext bytes per AEAD chunk) — must match the
    /// chunk size used by `EncryptedFile` for this database.
    pub chunk_size: u32,
    /// Key fingerprint — see [`Cipher::fingerprint`].
    pub fingerprint: [u8; FINGERPRINT_LEN],
}

impl EncryptionMarker {
    /// Build a marker for the given cipher + chunk size.
    #[must_use]
    pub fn new(cipher: &Cipher, chunk_size: u32) -> Self {
        Self {
            format_version: ENCRYPTION_MARKER_FORMAT_VERSION,
            algorithm: ENCRYPTION_ALGO_AES_GCM_256,
            chunk_size,
            fingerprint: cipher.fingerprint(),
        }
    }

    /// Encode to the on-disk byte layout. Always 64 bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; MARKER_LEN] {
        let mut out = [0u8; MARKER_LEN];
        out[0..4].copy_from_slice(&ENCRYPTION_MARKER_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        out[8..12].copy_from_slice(&self.algorithm.to_le_bytes());
        out[12..16].copy_from_slice(&self.chunk_size.to_le_bytes());
        out[16..32].copy_from_slice(&self.fingerprint);
        // bytes [32..64] reserved zero
        out
    }

    /// Decode from the on-disk byte layout. Strict — refuses anything
    /// other than the documented shape.
    pub fn decode(bytes: &[u8]) -> Result<Self, EncryptionError> {
        if bytes.len() < MARKER_LEN {
            return Err(EncryptionError::MarkerTruncated {
                expected: MARKER_LEN,
                got: bytes.len(),
            });
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != ENCRYPTION_MARKER_MAGIC {
            return Err(EncryptionError::MarkerBadMagic { got: magic });
        }
        let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if format_version != ENCRYPTION_MARKER_FORMAT_VERSION {
            return Err(EncryptionError::MarkerUnsupportedVersion(format_version));
        }
        let algorithm = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if algorithm != ENCRYPTION_ALGO_AES_GCM_256 {
            return Err(EncryptionError::MarkerUnsupportedAlgorithm(algorithm));
        }
        let chunk_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let mut fingerprint = [0u8; FINGERPRINT_LEN];
        fingerprint.copy_from_slice(&bytes[16..32]);
        Ok(Self {
            format_version,
            algorithm,
            chunk_size,
            fingerprint,
        })
    }

    /// Verify a candidate cipher matches this marker's fingerprint.
    /// Constant-time comparison — fingerprint mismatch reveals nothing
    /// about the actual key.
    #[must_use]
    pub fn matches(&self, cipher: &Cipher) -> bool {
        let actual = cipher.fingerprint();
        // Byte-for-byte equal-time compare. 16 bytes is short enough
        // that the optimizer wouldn't introduce a branch anyway, but the
        // explicit fold makes the intent clear.
        let mut diff: u8 = 0;
        for (a, b) in self.fingerprint.iter().zip(actual.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn cipher() -> Cipher {
        Cipher::from_raw_key(&[0x42u8; KEY_LEN]).unwrap()
    }

    #[test]
    fn cipher_encrypt_decrypt_round_trip() {
        let c = cipher();
        let plaintext = b"hello, hyperedge world";
        let blob = c.encrypt(plaintext).unwrap();
        let back = c.decrypt(&blob).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn cipher_two_encrypts_use_different_nonces() {
        let c = cipher();
        let a = c.encrypt(b"same plaintext").unwrap();
        let b = c.encrypt(b"same plaintext").unwrap();
        assert_ne!(a, b, "nonce must be fresh per call");
    }

    #[test]
    fn cipher_tamper_detected() {
        let c = cipher();
        let mut blob = c.encrypt(b"the secret").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01; // flip a bit in the tag
        assert!(c.decrypt(&blob).is_err());
    }

    #[test]
    fn cipher_wrong_key_detected() {
        let c1 = Cipher::from_raw_key(&[1u8; KEY_LEN]).unwrap();
        let c2 = Cipher::from_raw_key(&[2u8; KEY_LEN]).unwrap();
        let blob = c1.encrypt(b"hidden").unwrap();
        assert!(c2.decrypt(&blob).is_err());
    }

    #[test]
    fn cipher_from_hex_round_trip() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let c = Cipher::from_hex(hex).unwrap();
        let blob = c.encrypt(b"x").unwrap();
        assert_eq!(c.decrypt(&blob).unwrap(), b"x");
    }

    #[test]
    fn cipher_from_hex_rejects_wrong_length() {
        // 30 bytes (60 hex chars), not 32.
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddee";
        let err = Cipher::from_hex(hex).unwrap_err();
        assert!(matches!(err, EncryptionError::KeyLength { .. }));
    }

    #[test]
    fn cipher_from_hex_rejects_non_hex() {
        let hex = "zzzz".repeat(16);
        let err = Cipher::from_hex(&hex).unwrap_err();
        assert!(matches!(err, EncryptionError::KeyHex(_)));
    }

    #[test]
    fn encrypted_file_round_trip_small_write() {
        let buf: Vec<u8> = Vec::new();
        let writer = EncryptedFile::create(buf, cipher(), DEFAULT_CHUNK_SIZE).unwrap();
        let mut w = writer;
        w.write_all(b"hello world").unwrap();
        let on_disk = w.finish().unwrap();
        assert!(
            on_disk.len() > HEADER_LEN + CHUNK_OVERHEAD,
            "encrypted form must be larger than plaintext: {}",
            on_disk.len()
        );

        let cursor = Cursor::new(on_disk);
        let mut r = EncryptedFile::open(cursor, cipher()).unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn encrypted_file_round_trip_multi_chunk() {
        // Write more than one chunk worth of plaintext.
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i & 0xff) as u8).collect();
        let mut w = EncryptedFile::create(Vec::new(), cipher(), 1024).unwrap();
        w.write_all(&payload).unwrap();
        let bytes = w.finish().unwrap();

        let mut r = EncryptedFile::open(Cursor::new(bytes), cipher()).unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, payload);
    }

    #[test]
    fn encrypted_file_partial_writes_and_reads() {
        let mut w = EncryptedFile::create(Vec::new(), cipher(), 32).unwrap();
        for byte in b"abcdefghijklmnopqrstuvwxyz0123456789" {
            w.write_all(&[*byte]).unwrap();
        }
        let bytes = w.finish().unwrap();
        let mut r = EncryptedFile::open(Cursor::new(bytes), cipher()).unwrap();
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while r.read(&mut byte).unwrap() != 0 {
            out.push(byte[0]);
        }
        assert_eq!(out, b"abcdefghijklmnopqrstuvwxyz0123456789");
    }

    #[test]
    fn encrypted_file_open_rejects_plain_file() {
        let plain = Cursor::new(b"\x00\x01\x02\x03not encrypted".to_vec());
        let err = EncryptedFile::open(plain, cipher()).unwrap_err();
        assert!(matches!(err, EncryptionError::NotEncrypted));
    }

    #[test]
    fn encrypted_file_open_rejects_wrong_version() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&ENCRYPTED_FILE_MAGIC.to_le_bytes());
        buf.extend_from_slice(&999u32.to_le_bytes()); // unsupported version
        buf.extend_from_slice(&DEFAULT_CHUNK_SIZE.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);
        let err = EncryptedFile::open(Cursor::new(buf), cipher()).unwrap_err();
        assert!(matches!(err, EncryptionError::UnsupportedVersion(999)));
    }

    #[test]
    fn encrypted_file_open_rejects_wrong_key() {
        let mut w = EncryptedFile::create(Vec::new(), cipher(), 64).unwrap();
        w.write_all(b"some payload").unwrap();
        let bytes = w.finish().unwrap();

        let wrong = Cipher::from_raw_key(&[0xAAu8; KEY_LEN]).unwrap();
        let mut r = EncryptedFile::open(Cursor::new(bytes), wrong).unwrap();
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).unwrap_err();
        assert!(err.to_string().contains("AEAD"));
    }

    #[test]
    fn sniff_magic_detects_encrypted_files() {
        let mut w = EncryptedFile::create(Vec::new(), cipher(), 64).unwrap();
        w.write_all(b"x").unwrap();
        let bytes = w.finish().unwrap();
        let detected = EncryptedFile::<&[u8]>::sniff_magic(Cursor::new(bytes)).unwrap();
        assert!(detected);

        let plain: &[u8] = b"\x00\x00\x00\x00plain";
        let detected = EncryptedFile::<&[u8]>::sniff_magic(Cursor::new(plain)).unwrap();
        assert!(!detected);
    }

    #[test]
    fn fingerprint_is_deterministic_and_distinguishes_keys() {
        let a = Cipher::from_raw_key(&[0x11u8; KEY_LEN]).unwrap();
        let b = Cipher::from_raw_key(&[0x11u8; KEY_LEN]).unwrap();
        let c = Cipher::from_raw_key(&[0x22u8; KEY_LEN]).unwrap();
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "same key → same fingerprint"
        );
        assert_ne!(
            a.fingerprint(),
            c.fingerprint(),
            "different key → different fingerprint"
        );
    }

    #[test]
    fn marker_round_trip_and_match_check() {
        let key = Cipher::from_raw_key(&[0x33u8; KEY_LEN]).unwrap();
        let marker = EncryptionMarker::new(&key, DEFAULT_CHUNK_SIZE);
        let bytes = marker.encode();
        assert_eq!(bytes.len(), 64);
        let decoded = EncryptionMarker::decode(&bytes).unwrap();
        assert_eq!(decoded, marker);
        assert!(decoded.matches(&key));

        let wrong = Cipher::from_raw_key(&[0xAAu8; KEY_LEN]).unwrap();
        assert!(!decoded.matches(&wrong));
    }

    #[test]
    fn marker_decode_rejects_bad_magic() {
        let mut bytes = [0u8; 64];
        bytes[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let err = EncryptionMarker::decode(&bytes).unwrap_err();
        assert!(matches!(err, EncryptionError::MarkerBadMagic { .. }));
    }

    #[test]
    fn marker_decode_rejects_truncated_input() {
        let err = EncryptionMarker::decode(&[0u8; 8]).unwrap_err();
        assert!(matches!(err, EncryptionError::MarkerTruncated { .. }));
    }

    #[test]
    fn marker_decode_rejects_unsupported_version() {
        let mut bytes = [0u8; 64];
        bytes[0..4].copy_from_slice(&ENCRYPTION_MARKER_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&ENCRYPTION_ALGO_AES_GCM_256.to_le_bytes());
        let err = EncryptionMarker::decode(&bytes).unwrap_err();
        assert!(matches!(
            err,
            EncryptionError::MarkerUnsupportedVersion(999)
        ));
    }

    #[test]
    fn marker_decode_rejects_unsupported_algorithm() {
        let mut bytes = [0u8; 64];
        bytes[0..4].copy_from_slice(&ENCRYPTION_MARKER_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&ENCRYPTION_MARKER_FORMAT_VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&42u32.to_le_bytes());
        let err = EncryptionMarker::decode(&bytes).unwrap_err();
        assert!(matches!(
            err,
            EncryptionError::MarkerUnsupportedAlgorithm(42)
        ));
    }
}
