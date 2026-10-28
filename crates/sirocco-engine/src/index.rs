//! Persistent block index for incremental sync.
//!
//! A block delta without an index has to read **both** sides: the source to know
//! what it contains, the destination to know what is there. The read of the source
//! is pure overhead if we already recorded, last time, what the source looked like.
//!
//! So after every successful copy we store the per-block hashes of the source. On
//! the next run, if the source's `(size, mtime)` still match, we can read *only the
//! destination*, compare each block against the recorded hash, and pull the source
//! bytes for the blocks that actually differ:
//!
//! ```text
//! indexed delta:   S/Rd + r·S/R + r·S/W
//! plain delta:     S/R  + S/Rd + r·S/W
//! full copy:       S/R  + S/W
//! ```
//!
//! For small `r` the indexed form is strictly the cheapest, and unlike the plain
//! delta it does not need the destination to be slow for the maths to work out.
//!
//! # Trust model
//!
//! The index is only consulted when the source's size and modification time match
//! exactly what was recorded. If a file is modified without changing either — a
//! deliberate `utimes` restore, or an in-place write that keeps the timestamp —
//! the index is stale and we would skip a changed block. That is the same
//! assumption rsync's `--checksum`-off mode makes, it is opt-in via `--index`, and
//! `--index=verify` closes the hole by re-hashing the source.

use std::io::Read;
use std::io::Write;
use std::io::{
    self,
};
use std::path::Path;
use std::path::PathBuf;

use sirocco_core::hash::xxh64;

const MAGIC: &[u8; 8] = b"SIROCCIX";
const VERSION: u32 = 2;
// magic, version, hash kind, block, size, mtime, count
const HEADER: usize = 8 + 4 + 4 + 8 + 8 + 16 + 8;

/// Block hashes for one file.
#[derive(Clone, Debug)]
pub struct BlockIndex {
    /// Which digest produced `hashes`. Mixing kinds would make every block look
    /// changed, so the kind travels with the data.
    pub kind: sirocco_core::hash::HashKind,
    /// Block size the hashes were taken at.
    pub block: u64,
    /// Size of the file the hashes describe (the destination).
    pub size: u64,
    /// Its modification time in ns.
    pub mtime_ns: u128,
    /// One digest per block, in file order.
    pub hashes: Vec<u64>,
}

impl BlockIndex {
    /// Build an index for a file of `size` bytes split into `block`-sized pieces.
    #[must_use]
    pub const fn new(
        kind: sirocco_core::hash::HashKind,
        block: u64,
        size: u64,
        mtime_ns: u128,
        hashes: Vec<u64>,
    ) -> Self {
        Self {
            kind,
            block,
            size,
            mtime_ns,
            hashes,
        }
    }

    /// Is this index still an accurate description of the file?
    #[must_use]
    pub fn matches(
        &self,
        size: u64,
        mtime_ns: u128,
        block: u64,
        kind: sirocco_core::hash::HashKind,
    ) -> bool {
        self.size == size
            && self.mtime_ns == mtime_ns
            && self.block == block
            && self.kind == kind
            && self.hashes.len() as u64 == size.div_ceil(block.max(1))
    }

    #[must_use]
    /// Serialise to the on-disk form.
    ///
    /// The header is `magic | version | kind | block | size | mtime | count`,
    /// little-endian, followed by the hashes. A reader that does not recognise the
    /// version rejects the file rather than misreading it.

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER + self.hashes.len() * 8);
        v.extend_from_slice(MAGIC);
        v.extend_from_slice(&VERSION.to_le_bytes());
        let kind = u32::from(self.kind == sirocco_core::hash::HashKind::Crc32c);
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(&self.block.to_le_bytes());
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(&self.mtime_ns.to_le_bytes());
        v.extend_from_slice(&(self.hashes.len() as u64).to_le_bytes());
        for h in &self.hashes {
            v.extend_from_slice(&h.to_le_bytes());
        }
        v
    }

    #[must_use]
    /// Parse the on-disk form. `None` for anything that does not match the
    /// current magic, version or length — a stale index is ignored, never trusted.

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < HEADER || &b[0..8] != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(b[8..12].try_into().ok()?);
        if version != VERSION {
            return None;
        }
        let kind = if u32::from_le_bytes(b[12..16].try_into().ok()?) == 1 {
            sirocco_core::hash::HashKind::Crc32c
        } else {
            sirocco_core::hash::HashKind::Xxh64
        };
        let block = u64::from_le_bytes(b[16..24].try_into().ok()?);
        let size = u64::from_le_bytes(b[24..32].try_into().ok()?);
        let mtime_ns = u128::from_le_bytes(b[32..48].try_into().ok()?);
        let count = u64::from_le_bytes(b[48..56].try_into().ok()?) as usize;
        if block == 0 || count > (1 << 32) {
            return None;
        }
        let need = HEADER.checked_add(count.checked_mul(8)?)?;
        if b.len() < need {
            return None;
        }
        let mut hashes = Vec::with_capacity(count);
        for i in 0..count {
            let o = HEADER + i * 8;
            hashes.push(u64::from_le_bytes(b[o..o + 8].try_into().ok()?));
        }
        Some(Self {
            kind,
            block,
            size,
            mtime_ns,
            hashes,
        })
    }
}

/// Where indexes live. Kept outside the destination tree so a copy never has to
/// explain an unexpected `.sirocco-index` directory to the user (or to `sync`).
pub struct IndexStore {
    root: PathBuf,
    enabled: bool,
}

impl IndexStore {
    #[must_use]
    /// Open (or lazily create) an index store rooted at `root`.

    pub const fn new(
        root: PathBuf,
        enabled: bool,
    ) -> Self {
        Self { root, enabled }
    }

    /// Default cache location: `$XDG_CACHE_HOME/sirocco/index` on Linux,
    /// `%LOCALAPPDATA%\sirocco\index` on Windows.
    #[must_use]
    pub fn default_root() -> PathBuf {
        #[cfg(windows)]
        {
            if let Some(p) = std::env::var_os("LOCALAPPDATA") {
                return PathBuf::from(p).join("sirocco").join("index");
            }
            std::env::temp_dir().join("sirocco-index")
        }
        #[cfg(not(windows))]
        {
            if let Some(p) = std::env::var_os("XDG_CACHE_HOME") {
                return PathBuf::from(p).join("sirocco").join("index");
            }
            if let Some(h) = std::env::var_os("HOME") {
                return PathBuf::from(h)
                    .join(".cache")
                    .join("sirocco")
                    .join("index");
            }
            std::env::temp_dir().join("sirocco-index")
        }
    }

    #[must_use]
    /// True when indexing is on at all.

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Index file for a destination file, sharded two levels deep so a tree with a
    /// million entries does not produce a million-entry directory.
    #[must_use]
    pub fn path_for(
        &self,
        dst: &Path,
    ) -> PathBuf {
        // Hash the raw OS string: `to_string_lossy().to_lowercase()` collides `A`
        // with `a` on Linux (two different files, one index) and mangles non-UTF-8
        // names. Windows is case-insensitive, so it folds case — and only it.
        let key = {
            #[cfg(windows)]
            {
                xxh64(dst.to_string_lossy().to_lowercase().as_bytes())
            }
            #[cfg(not(windows))]
            {
                use std::os::unix::ffi::OsStrExt;
                xxh64(dst.as_os_str().as_bytes())
            }
        };
        let hex = format!("{key:016x}");
        self.root
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.idx"))
    }

    #[must_use]
    /// Read the index for `dst`, if one exists and parses.

    pub fn load(
        &self,
        dst: &Path,
    ) -> Option<BlockIndex> {
        if !self.enabled {
            return None;
        }
        let p = self.path_for(dst);
        let mut f = std::fs::File::open(p).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        BlockIndex::from_bytes(&buf)
    }

    /// Write the index for `dst`.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the index file cannot be written. A failure
    /// here never fails the copy — the index is an optimisation.
    pub fn store(
        &self,
        dst: &Path,
        idx: &BlockIndex,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let p = self.path_for(dst);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write to a sibling and rename so a crash never leaves a half index that
        // would be parsed as valid.
        let tmp = p.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&idx.to_bytes())?;
        }
        std::fs::rename(&tmp, &p)
    }

    /// Drop the index for `dst`, because the destination no longer matches it.
    pub fn forget(
        &self,
        dst: &Path,
    ) {
        if self.enabled {
            let _ = std::fs::remove_file(self.path_for(dst));
        }
    }
}

/// Block hashes of a file, collected while it is copied.
#[must_use]
pub fn mtime_ns(t: Option<std::time::SystemTime>) -> u128 {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let idx = BlockIndex::new(
            sirocco_core::hash::HashKind::Xxh64,
            1 << 20,
            5 << 20,
            123456789,
            vec![1, 2, 3, 4, 5],
        );
        let b = idx.to_bytes();
        let back = BlockIndex::from_bytes(&b).unwrap();
        assert_eq!(back.block, idx.block);
        assert_eq!(back.size, idx.size);
        assert_eq!(back.mtime_ns, idx.mtime_ns);
        assert_eq!(back.kind, idx.kind);
        assert_eq!(back.hashes, idx.hashes);
    }

    #[test]
    fn rejects_garbage() {
        assert!(BlockIndex::from_bytes(b"").is_none());
        assert!(BlockIndex::from_bytes(b"NOTMAGIC").is_none());
        let mut b =
            BlockIndex::new(sirocco_core::hash::HashKind::Xxh64, 4096, 4096, 0, vec![7]).to_bytes();
        b.truncate(b.len() - 4);
        assert!(
            BlockIndex::from_bytes(&b).is_none(),
            "truncated index must not parse"
        );
    }

    #[test]
    fn matching_requires_all_three() {
        use sirocco_core::hash::HashKind;
        let idx = BlockIndex::new(HashKind::Xxh64, 4096, 8192, 1000, vec![1, 2]);
        assert!(idx.matches(8192, 1000, 4096, HashKind::Xxh64));
        assert!(!idx.matches(8193, 1000, 4096, HashKind::Xxh64));
        assert!(!idx.matches(8192, 1001, 4096, HashKind::Xxh64));
        assert!(!idx.matches(8192, 1000, 8192, HashKind::Xxh64));
        assert!(!idx.matches(8192, 1000, 4096, HashKind::Crc32c));
    }

    #[test]
    fn shards_by_path() {
        let s = IndexStore::new(PathBuf::from("/tmp/x"), true);
        let a = s.path_for(Path::new("/data/a.bin"));
        let b = s.path_for(Path::new("/data/b.bin"));
        assert_ne!(a, b);
        assert_eq!(a, s.path_for(Path::new("/data/a.bin")));
        assert!(a.starts_with("/tmp/x"));
    }
}
