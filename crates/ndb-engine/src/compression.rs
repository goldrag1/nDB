//! Block compression for SSTable data blocks (opt-in, §11.5 v2).
#![allow(clippy::doc_markdown)]
//!
//! SSTable records are stored as a stream of size-prefixed entries. With
//! compression enabled the writer groups records into ~`BLOCK_SIZE` blocks and
//! compresses each one; the reader decompresses a block on demand, so the page
//! cache holds the smaller compressed bytes and only touched blocks are
//! decompressed — preserving nDB's bounded-RAM mmap story while cutting disk +
//! cache footprint.
//!
//! This module owns just the **per-block framing + codec**; the SSTable
//! writer/reader drive it. The framing is codec-tagged so a future codec (e.g.
//! zstd) slots in without a format break.
//!
//! On-disk block layout (little-endian):
//!
//! ```text
//! header (13 bytes)
//!   codec             u8     0 = stored (uncompressed), 1 = lz4
//!   uncompressed_len  u32
//!   compressed_len    u32    bytes of payload that follow
//!   crc32             u32    CRC32 of the payload bytes
//! payload             compressed_len bytes
//! ```
//!
//! A block whose payload would not shrink is written `Stored` (codec 0), so
//! compression never inflates a block beyond the 13-byte header.

use crc32fast::Hasher;
use thiserror::Error;

/// Fixed per-block header size in bytes.
pub const BLOCK_HEADER_SIZE: usize = 13;

/// Default target *uncompressed* block size: records are accumulated up to
/// this many bytes before a block is sealed and compressed.
pub const DEFAULT_BLOCK_BYTES: usize = 32 * 1024;

/// Compression codec for a block. The discriminant is the on-disk `codec`
/// byte; new codecs append without disturbing existing files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Codec {
    /// Stored uncompressed (payload == plaintext). Used when a block doesn't
    /// compress, and as the explicit "compression off" choice. Default.
    #[default]
    Stored = 0,
    /// LZ4 (via `lz4_flex`, pure-Rust safe mode).
    Lz4 = 1,
}

impl Codec {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Stored),
            1 => Some(Self::Lz4),
            _ => None,
        }
    }
}

/// Errors raised while decoding a compressed block.
#[derive(Debug, Error)]
pub enum CompressionError {
    /// Input shorter than a block header (or than the header claims).
    #[error("compressed block truncated: have {have} bytes, need {need}")]
    Truncated {
        /// Bytes available.
        have: usize,
        /// Bytes required.
        need: usize,
    },
    /// `codec` byte didn't match a known codec.
    #[error("unknown block codec byte {0}")]
    UnknownCodec(u8),
    /// CRC32 over the payload didn't match the header.
    #[error("block CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}")]
    CrcMismatch {
        /// CRC read from the header.
        stored: u32,
        /// CRC computed over the payload.
        computed: u32,
    },
    /// Decompression produced a different length than the header promised, or
    /// the codec rejected the payload.
    #[error("block decompress failed: {0}")]
    Decompress(String),
}

/// Compress `plaintext` into a self-describing block (header + payload), using
/// `codec`. Falls back to [`Codec::Stored`] for this block when compression
/// would not shrink it, so the result is never more than
/// `plaintext.len() + BLOCK_HEADER_SIZE` bytes.
#[must_use]
pub fn encode_block(plaintext: &[u8], codec: Codec) -> Vec<u8> {
    let uncompressed_len = plaintext.len();
    let (chosen, payload): (Codec, std::borrow::Cow<'_, [u8]>) = match codec {
        Codec::Stored => (Codec::Stored, std::borrow::Cow::Borrowed(plaintext)),
        Codec::Lz4 => {
            let compressed = lz4_flex::compress(plaintext);
            if compressed.len() < uncompressed_len {
                (Codec::Lz4, std::borrow::Cow::Owned(compressed))
            } else {
                // No win (or expansion) — store raw.
                (Codec::Stored, std::borrow::Cow::Borrowed(plaintext))
            }
        }
    };

    let mut crc = Hasher::new();
    crc.update(&payload);
    let crc = crc.finalize();

    let mut out = Vec::with_capacity(BLOCK_HEADER_SIZE + payload.len());
    out.push(chosen as u8);
    out.extend_from_slice(
        &u32::try_from(uncompressed_len)
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Total on-disk size of the block starting at `input[0]`, read from its
/// header without decompressing. Used to walk blocks / size the reader's
/// cursor.
pub fn block_len(input: &[u8]) -> Result<usize, CompressionError> {
    if input.len() < BLOCK_HEADER_SIZE {
        return Err(CompressionError::Truncated {
            have: input.len(),
            need: BLOCK_HEADER_SIZE,
        });
    }
    let compressed_len = u32::from_le_bytes(input[5..9].try_into().unwrap()) as usize;
    Ok(BLOCK_HEADER_SIZE + compressed_len)
}

/// Decode the block at the start of `input`, returning the decompressed
/// plaintext and the number of input bytes the block occupied (header +
/// payload). The CRC over the payload is verified before decompression.
pub fn decode_block(input: &[u8]) -> Result<(Vec<u8>, usize), CompressionError> {
    if input.len() < BLOCK_HEADER_SIZE {
        return Err(CompressionError::Truncated {
            have: input.len(),
            need: BLOCK_HEADER_SIZE,
        });
    }
    let codec = Codec::from_byte(input[0]).ok_or(CompressionError::UnknownCodec(input[0]))?;
    let uncompressed_len = u32::from_le_bytes(input[1..5].try_into().unwrap()) as usize;
    let compressed_len = u32::from_le_bytes(input[5..9].try_into().unwrap()) as usize;
    let stored_crc = u32::from_le_bytes(input[9..13].try_into().unwrap());

    let total = BLOCK_HEADER_SIZE + compressed_len;
    if input.len() < total {
        return Err(CompressionError::Truncated {
            have: input.len(),
            need: total,
        });
    }
    let payload = &input[BLOCK_HEADER_SIZE..total];

    let mut crc = Hasher::new();
    crc.update(payload);
    let computed = crc.finalize();
    if computed != stored_crc {
        return Err(CompressionError::CrcMismatch {
            stored: stored_crc,
            computed,
        });
    }

    let plaintext = match codec {
        Codec::Stored => {
            if payload.len() != uncompressed_len {
                return Err(CompressionError::Decompress(format!(
                    "stored block length {} != header {}",
                    payload.len(),
                    uncompressed_len
                )));
            }
            payload.to_vec()
        }
        Codec::Lz4 => lz4_flex::decompress(payload, uncompressed_len)
            .map_err(|e| CompressionError::Decompress(e.to_string()))?,
    };
    if plaintext.len() != uncompressed_len {
        return Err(CompressionError::Decompress(format!(
            "decompressed length {} != header {}",
            plaintext.len(),
            uncompressed_len
        )));
    }
    Ok((plaintext, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8], codec: Codec) {
        let block = encode_block(data, codec);
        assert_eq!(block_len(&block).unwrap(), block.len());
        let (back, consumed) = decode_block(&block).unwrap();
        assert_eq!(consumed, block.len());
        assert_eq!(back, data);
    }

    #[test]
    fn roundtrip_compressible_and_incompressible_and_empty() {
        // Compressible (repetitive).
        roundtrip(&vec![0xABu8; 50_000], Codec::Lz4);
        // Incompressible (varied) — should fall back to Stored internally.
        let varied: Vec<u8> = (0..40_000u32).flat_map(|i| i.to_le_bytes()).collect();
        roundtrip(&varied, Codec::Lz4);
        // Empty.
        roundtrip(&[], Codec::Lz4);
        // Stored codec.
        roundtrip(b"hello world", Codec::Stored);
    }

    #[test]
    fn compressible_block_actually_shrinks() {
        let data = vec![0x7Fu8; 100_000];
        let block = encode_block(&data, Codec::Lz4);
        assert!(
            block.len() < data.len() / 2,
            "repetitive data should compress well"
        );
        assert_eq!(block[0], Codec::Lz4 as u8);
    }

    #[test]
    fn incompressible_block_falls_back_to_stored_no_inflation() {
        let varied: Vec<u8> = (0..20_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let block = encode_block(&varied, Codec::Lz4);
        assert_eq!(
            block[0],
            Codec::Stored as u8,
            "should store raw when no win"
        );
        assert_eq!(block.len(), varied.len() + BLOCK_HEADER_SIZE);
    }

    #[test]
    fn crc_corruption_rejected() {
        let mut block = encode_block(&vec![1u8; 1000], Codec::Lz4);
        let last = block.len() - 1;
        block[last] ^= 0xff;
        assert!(matches!(
            decode_block(&block),
            Err(CompressionError::Decompress(_) | CompressionError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn header_corruption_rejected() {
        let mut block = encode_block(b"payload bytes here", Codec::Stored);
        block[0] = 99; // unknown codec
        assert!(matches!(
            decode_block(&block),
            Err(CompressionError::UnknownCodec(99))
        ));
    }

    #[test]
    fn truncation_rejected() {
        let block = encode_block(&vec![5u8; 2000], Codec::Lz4);
        assert!(matches!(
            decode_block(&block[..BLOCK_HEADER_SIZE - 1]),
            Err(CompressionError::Truncated { .. })
        ));
        assert!(matches!(
            decode_block(&block[..block.len() - 3]),
            Err(CompressionError::Truncated { .. })
        ));
    }

    #[test]
    fn two_blocks_walk_by_consumed_len() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&encode_block(b"first block data", Codec::Lz4));
        buf.extend_from_slice(&encode_block(b"second block data here", Codec::Lz4));
        let (b0, c0) = decode_block(&buf).unwrap();
        assert_eq!(b0, b"first block data");
        let (b1, c1) = decode_block(&buf[c0..]).unwrap();
        assert_eq!(b1, b"second block data here");
        assert_eq!(c0 + c1, buf.len());
    }
}
