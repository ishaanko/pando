//! Blob storage for packs. Packs are immutable and keyed by their trailer hash, so a
//! write is "stage, then rename (or link) into place", and readers never see partial
//! packs. This stands in for an object store; nothing else in Pando touches pack bytes.
//!
//! A stored pack is two files in `packs/`: `<hash>.pack` and its index sidecar
//! `<hash>.pidx`. It counts as stored ([`BlobStore::has`]) only when both exist.
//!
//! Durability: every file is fsynced while still in staging, so any name in `packs/`
//! that survives a power loss names complete contents. Publishing puts both names in
//! place and then fsyncs `packs/` once, so when it returns the pack survives power loss
//! and metadata may name it. A crash before that sync can leave either name without
//! the other; [`BlobStore::has`] then says the pack is not stored, and publishing it
//! again completes it. Deletes are not synced: a pack that reappears after a crash is
//! just garbage, and half of one is not stored.
//!
//! Pushes publish before taking the metadata lock (see `receive.rs`); deletes happen
//! only under it (gc). [`BlobStore::deletions`] lets a pusher tell, under the lock,
//! whether a delete could have removed what it published meanwhile.

use crate::object::Oid;
use crate::pack::{self, IdxEntry};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct BlobStore {
    root: PathBuf,
    staged: AtomicU64,
    /// Calls to [`BlobStore::delete`] so far.
    deleted: AtomicU64,
}

impl BlobStore {
    pub fn open(root: impl AsRef<Path>) -> io::Result<BlobStore> {
        let root = root.as_ref().to_path_buf();
        create_dir_all(&root.join("packs"))?;
        create_dir_all(&root.join("tmp"))?;
        Ok(BlobStore {
            root,
            staged: AtomicU64::new(0),
            deleted: AtomicU64::new(0),
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

    /// Whether a pack is stored: its file and its index both exist.
    pub fn has(&self, hash: &Oid) -> bool {
        self.idx_path(hash).exists() && self.pack_path(hash).exists()
    }

    /// Move a validated, already synced staged pack into place along with its sidecar
    /// index. Both are durable when this returns.
    pub fn publish(&self, staged: &Path, hash: &Oid, idx: &[IdxEntry]) -> io::Result<()> {
        self.put(staged, hash, idx, false)
    }

    /// Like [`BlobStore::publish`], but hard-link the staged file into place and leave
    /// it in staging, so the caller can publish it again if the pack is deleted before
    /// its metadata is committed.
    pub fn publish_linked(&self, staged: &Path, hash: &Oid, idx: &[IdxEntry]) -> io::Result<()> {
        self.put(staged, hash, idx, true)
    }

    fn put(&self, staged: &Path, hash: &Oid, idx: &[IdxEntry], link: bool) -> io::Result<()> {
        let packs = self.root.join("packs");
        if self.has(hash) {
            if !link {
                fs::remove_file(staged)?;
            }
            // Whoever put these names here may not have synced them yet.
            return sync_dir(&packs);
        }
        let (tmp, ()) = self.stage(|out| out.write_all(&pack::encode_idx(idx)))?;
        let dest = self.pack_path(hash);
        let placed = if link {
            match fs::hard_link(staged, &dest) {
                // The same bytes, stored by a concurrent push or left by a crash.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                r => r,
            }
        } else {
            fs::rename(staged, &dest)
        };
        if let Err(e) = placed.and_then(|()| fs::rename(&tmp, self.idx_path(hash))) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        sync_dir(&packs)
    }

    /// How many times [`BlobStore::delete`] has been called. Deletes run under the
    /// metadata lock, so if this is unchanged between before a publish and a point
    /// under the lock, nothing the publish wrote has been deleted.
    pub fn deletions(&self) -> u64 {
        self.deleted.load(Ordering::SeqCst)
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

    /// Delete a pack and its index. Readers that already opened it keep working. Call
    /// it only under the metadata lock (see [`BlobStore::deletions`]).
    pub fn delete(&self, hash: &Oid) -> io::Result<u64> {
        self.deleted.fetch_add(1, Ordering::SeqCst);
        let size = fs::metadata(self.pack_path(hash)).map_or(0, |m| m.len());
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
