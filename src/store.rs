//! Blob storage for packs. Packs are immutable and keyed by their trailer hash, so a
//! write is "stage, then rename into place", and readers never see partial packs.
//! This stands in for an object store; nothing else in Pando touches pack bytes.

use crate::object::Oid;
use crate::pack::{self, IdxEntry};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct BlobStore {
    root: PathBuf,
    staged: AtomicU64,
}

impl BlobStore {
    pub fn open(root: impl AsRef<Path>) -> io::Result<BlobStore> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("packs"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(BlobStore {
            root,
            staged: AtomicU64::new(0),
        })
    }

    /// A fresh path to stream an incoming pack into before it is validated.
    pub fn staging_path(&self) -> PathBuf {
        let n = self.staged.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("tmp")
            .join(format!("incoming-{}-{n}", std::process::id()))
    }

    pub fn pack_path(&self, hash: &Oid) -> PathBuf {
        self.root.join("packs").join(format!("{hash}.pack"))
    }

    fn idx_path(&self, hash: &Oid) -> PathBuf {
        self.root.join("packs").join(format!("{hash}.pidx"))
    }

    pub fn has(&self, hash: &Oid) -> bool {
        self.idx_path(hash).exists()
    }

    /// Move a validated staged pack into place along with its sidecar index. The index
    /// is renamed last, so its presence means the pack is complete.
    pub fn publish(&self, staged: &Path, hash: &Oid, idx: &[IdxEntry]) -> io::Result<()> {
        if self.has(hash) {
            return fs::remove_file(staged);
        }
        fs::rename(staged, self.pack_path(hash))?;
        self.write_idx(hash, idx)
    }

    /// Store a pack built in memory (compaction output).
    pub fn put(&self, hash: &Oid, bytes: &[u8], idx: &[IdxEntry]) -> io::Result<()> {
        if self.has(hash) {
            return Ok(());
        }
        let tmp = self.staging_path();
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, self.pack_path(hash))?;
        self.write_idx(hash, idx)
    }

    fn write_idx(&self, hash: &Oid, idx: &[IdxEntry]) -> io::Result<()> {
        let tmp = self.staging_path();
        fs::write(&tmp, pack::encode_idx(idx))?;
        fs::rename(&tmp, self.idx_path(hash))
    }

    pub fn read_pack(&self, hash: &Oid) -> io::Result<Vec<u8>> {
        fs::read(self.pack_path(hash))
    }

    /// Map a stored pack into memory. Packs are immutable, so the mapping is stable.
    pub fn map_pack(&self, hash: &Oid) -> io::Result<memmap2::Mmap> {
        map_file(&self.pack_path(hash))
    }

    pub fn read_idx(&self, hash: &Oid) -> io::Result<Vec<IdxEntry>> {
        pack::decode_idx(&fs::read(self.idx_path(hash))?)
    }
}

impl BlobStore {
    /// Every stored pack, with when it was written.
    pub fn list(&self) -> io::Result<Vec<(Oid, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("packs"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(hash) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".pack"))
                .and_then(Oid::from_hex)
            else {
                continue;
            };
            out.push((hash, entry.metadata()?.modified()?));
        }
        Ok(out)
    }

    /// Delete a pack and its index. Readers that already opened it keep working.
    pub fn delete(&self, hash: &Oid) -> io::Result<u64> {
        let size = fs::metadata(self.pack_path(hash)).map_or(0, |m| m.len());
        // Index first: its presence is what marks a pack as complete.
        for path in [self.idx_path(hash), self.pack_path(hash)] {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(size)
    }

    /// Remove staging files older than `age` (left by interrupted pushes).
    pub fn clean_staging(&self, age: std::time::Duration) -> io::Result<()> {
        for entry in fs::read_dir(self.root.join("tmp"))? {
            let entry = entry?;
            let old = entry
                .metadata()?
                .modified()?
                .elapsed()
                .is_ok_and(|e| e >= age);
            if old {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

/// Memory-map a file that nobody modifies while it is mapped.
pub fn map_file(path: &Path) -> io::Result<memmap2::Mmap> {
    let file = fs::File::open(path)?;
    // SAFETY: packs and staged files are written once and never modified in place.
    unsafe { memmap2::Mmap::map(&file) }
}
