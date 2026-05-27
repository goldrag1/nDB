//! Low-level little-endian read/write primitives used by the record + value
//! encoders. All multi-byte integers in nDB on-disk format are little-endian
//! (§11.2), so this module is the one place that fact is encoded; everything
//! else routes through `Cursor` and the `write_*` helpers.
#![allow(missing_docs)]

use crate::error::DecodeError;

/// Bounded cursor over an immutable byte slice. Tracks position and emits
/// `Truncated` errors on short reads.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    #[inline]
    pub fn remaining_bytes(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    #[inline]
    pub fn advance(&mut self, n: usize) {
        debug_assert!(self.pos + n <= self.buf.len(), "advance past end of buffer");
        self.pos += n;
    }

    fn need(&self, n: usize) -> Result<(), DecodeError> {
        if self.remaining() < n {
            Err(DecodeError::Truncated {
                offset: self.pos,
                needed: n - self.remaining(),
            })
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn read_u8(&mut self) -> Result<u8, DecodeError> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.need(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    pub fn read_slice(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    #[inline]
    pub fn read_u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }
    #[inline]
    pub fn read_u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }
    #[inline]
    pub fn read_u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }
    #[inline]
    pub fn read_i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.read_array::<8>()?))
    }
    #[inline]
    pub fn read_i128(&mut self) -> Result<i128, DecodeError> {
        Ok(i128::from_le_bytes(self.read_array::<16>()?))
    }
    #[inline]
    pub fn read_f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_le_bytes(self.read_array::<4>()?))
    }
    #[inline]
    pub fn read_f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_le_bytes(self.read_array::<8>()?))
    }
}

// --- write helpers ---------------------------------------------------------

#[inline]
pub fn write_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}
#[inline]
pub fn write_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn write_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn write_i128(buf: &mut Vec<u8>, v: i128) {
    buf.extend_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn write_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn write_f64(buf: &mut Vec<u8>, v: f64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
