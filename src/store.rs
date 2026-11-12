//! Blob storage for packs. Packs are immutable and keyed by their trailer hash, so a
//! write is "stage, then rename into place", and readers never see partial packs.
//! This stands in for an object store; nothing else in Pando touches pack bytes.
//!
//! Durability: every file is fsynced before its rename, and `packs/` after it. A pack's
//! rename is durable before its index is written, and the index's before a write
//! returns, so once a write returns its pack survives power loss and metadata may name
//! it. Deletes are not synced: a pack that reappears after a crash is just garbage.

use crate::object::Oid;
use crate::pack::{self, IdxEntry};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct BlobStore {
    root: PathBuf,
    staged: AtomicU64,
}

impl BlobStore {
    pub fn open(root: impl AsRef<Path>) -> io::Result<BlobStore> {
        let root = root.as_ref().to_path_buf();
        create_dir_all(&root.join("packs"))?;
        create_dir_all(&root.join("tmp"))?;
        Ok(BlobStore {
            root,
            staged: AtomicU64::new(0),
        })
    }

    /// A fresh path to stream an incoming pack into before it is validated. Whoever
    /// writes it must `sync_all` it before handing it to [`BlobStore::publish`].
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

    /// Move a validated, already synced staged pack into place along with its sidecar
    /// index. The index is renamed last, so its presence means the pack is complete.
    /// Both are durable when this returns.
    pub fn publish(&self, staged: &Path, hash: &Oid, idx: &[IdxEntry]) -> io::Result<()> {
        if self.has(hash) {
            return fs::remove_file(staged);
        }
        self.install(staged, &self.pack_path(hash))?;
        self.write_idx(hash, idx)
    }

    fn write_idx(&self, hash: &Oid, idx: &[IdxEntry]) -> io::Result<()> {
        let (tmp, ()) = self.stage(|out| out.write_all(&pack::encode_idx(idx)))?;
        self.install(&tmp, &self.idx_path(hash))
    }

    /// Rename a synced staging file into `packs/` and make the rename durable.
    fn install(&self, tmp: &Path, dest: &Path) -> io::Result<()> {
        fs::rename(tmp, dest)?;
        sync_dir(&self.root.join("packs"))
    }

    /// Stream a file built here (a compaction cover, an index) into a fresh staging
    /// file through `write`, and sync it. A pack is then published with
    /// [`BlobStore::publish`]. The file is removed if writing fails.
    pub fn stage<T>(
        &self,
        write: impl FnOnce(&mut io::BufWriter<fs::File>) -> io::Result<T>,
    ) -> io::Result<(PathBuf, T)> {
        let path = self.staging_path();
        let result = (|| {
            let mut out = io::BufWriter::with_capacity(1 << 20, fs::File::create(&path)?);
            let value = write(&mut out)?;
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            Ok(value)
        })();
        match result {
            Ok(value) => Ok((path, value)),
            Err(e) => {
                let _ = fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// Size in bytes of a stored pack.
    pub fn pack_size(&self, hash: &Oid) -> io::Result<u64> {
        Ok(fs::metadata(self.pack_path(hash))?.len())
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
        remove_stale(&self.root.join("tmp"), age)
    }
}

/// Remove files in `dir` that were last written more than `age` ago: staging files
/// left by interrupted writes. A missing directory is fine.
pub fn remove_stale(dir: &Path, age: std::time::Duration) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
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

/// Flush a directory's entries (new names, renames) to disk.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// `fs::create_dir_all` that also makes every directory it creates survive power loss.
/// Repositories are created by their first push, so their directories must be as
/// durable as the pack and metadata inside them.
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    // The directories about to be created, deepest first.
    let new: Vec<&Path> = dir
        .ancestors()
        .take_while(|d| !d.as_os_str().is_empty() && !d.exists())
        .collect();
    let Some(top) = new.last() else {
        return Ok(());
    };
    fs::create_dir_all(dir)?;
    let parent = top.parent().filter(|p| !p.as_os_str().is_empty());
    for d in new
        .iter()
        .copied()
        .chain([parent.unwrap_or(Path::new("."))])
    {
        sync_dir(d)?;
    }
    Ok(())
}

/// Memory-map a file that nobody modifies while it is mapped.
pub fn map_file(path: &Path) -> io::Result<memmap2::Mmap> {
    let file = fs::File::open(path)?;
    // SAFETY: packs and staged files are written once and never modified in place.
    unsafe { memmap2::Mmap::map(&file) }
}
