//! A minimal, dependency-free archive for shipping a base backup over the
//! wire (`GET /v1/backup` → follower bootstrap).
//!
//! [`Engine::backup_to`](crate::Engine::backup_to) produces a **flat**
//! directory of files (SSTables + sidecars + manifests + `CURRENT` + the
//! active WAL — no subdirectories). This packs that directory into one byte
//! blob and unpacks it on the follower. We avoid a `tar` dependency (and its
//! path-traversal surface) with a tiny self-describing format:
//!
//! ```text
//!   magic "NDBA"  version:u8=1  file_count:u32
//!   repeated file_count times:
//!     name_len:u16  name:[u8]   data_len:u64  data:[u8]
//! ```
//!
//! Names are flat (no `/`, no `..`, non-empty) — enforced on both pack and
//! unpack, so a hostile archive can't escape the destination directory.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"NDBA";
const VERSION: u8 = 1;

/// Reject anything that isn't a plain flat filename.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && name != "."
        && name != ".."
        && !name.contains('\0')
}

/// Pack every regular file directly under `dir` into one archive blob.
///
/// # Errors
/// On any IO error reading the directory or its files, or if a filename is
/// not representable as UTF-8 / not a safe flat name.
pub fn pack_dir(dir: &Path) -> io::Result<Vec<u8>> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let os = entry.file_name();
        let name = os.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 filename in backup")
        })?;
        if !is_safe_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe filename in backup: {name}"),
            ));
        }
        files.push((name.to_string(), fs::read(entry.path())?));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic order

    let mut out = Vec::new();
    out.write_all(MAGIC)?;
    out.push(VERSION);
    out.write_all(&u32::try_from(files.len()).unwrap_or(u32::MAX).to_le_bytes())?;
    for (name, data) in &files {
        let nb = name.as_bytes();
        out.write_all(&u16::try_from(nb.len()).unwrap_or(u16::MAX).to_le_bytes())?;
        out.write_all(nb)?;
        out.write_all(&(data.len() as u64).to_le_bytes())?;
        out.write_all(data)?;
    }
    Ok(out)
}

/// Unpack an archive produced by [`pack_dir`] into `dest` (created if absent).
///
/// # Errors
/// On a malformed/truncated archive, an unsafe filename, or any IO error.
pub fn unpack_into(bytes: &[u8], dest: &Path) -> io::Result<()> {
    let mut c = io::Cursor::new(bytes);
    let mut magic = [0u8; 4];
    c.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad archive magic",
        ));
    }
    let mut v = [0u8; 1];
    c.read_exact(&mut v)?;
    if v[0] != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported archive version {}", v[0]),
        ));
    }
    let mut n4 = [0u8; 4];
    c.read_exact(&mut n4)?;
    let count = u32::from_le_bytes(n4);

    fs::create_dir_all(dest)?;
    for _ in 0..count {
        let mut nl = [0u8; 2];
        c.read_exact(&mut nl)?;
        let mut name = vec![0u8; u16::from_le_bytes(nl) as usize];
        c.read_exact(&mut name)?;
        let name = String::from_utf8(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 name"))?;
        if !is_safe_name(&name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe filename in archive: {name}"),
            ));
        }
        let mut dl = [0u8; 8];
        c.read_exact(&mut dl)?;
        let mut data = vec![0u8; usize::try_from(u64::from_le_bytes(dl)).unwrap_or(usize::MAX)];
        c.read_exact(&mut data)?;
        let mut f = fs::File::create(dest.join(&name))?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    if let Ok(d) = fs::File::open(dest) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trip() {
        let src = std::env::temp_dir().join(format!("ndba-src-{}", uuid::Uuid::now_v7().simple()));
        let dst = std::env::temp_dir().join(format!("ndba-dst-{}", uuid::Uuid::now_v7().simple()));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("000001.ndb"), b"sstable-bytes").unwrap();
        fs::write(src.join("000001.idx"), b"index").unwrap();
        fs::write(src.join("CURRENT"), b"000001").unwrap();
        // A subdirectory must be ignored (backup_to only copies flat files).
        fs::create_dir_all(src.join("subdir")).unwrap();

        let blob = pack_dir(&src).unwrap();
        unpack_into(&blob, &dst).unwrap();

        assert_eq!(fs::read(dst.join("000001.ndb")).unwrap(), b"sstable-bytes");
        assert_eq!(fs::read(dst.join("000001.idx")).unwrap(), b"index");
        assert_eq!(fs::read(dst.join("CURRENT")).unwrap(), b"000001");
        assert!(!dst.join("subdir").exists(), "subdirs are not archived");

        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[test]
    fn rejects_bad_magic() {
        let dst = std::env::temp_dir().join(format!("ndba-bad-{}", uuid::Uuid::now_v7().simple()));
        let err = unpack_into(b"XXXX\x01\x00\x00\x00\x00", &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
