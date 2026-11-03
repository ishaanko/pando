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
        Ok(BlobStore { root, staged: AtomicU64::new(0) })
    }

    /// A fresh path to stream an incoming pack into before it is validated.
    pub fn staging_path(&self) -> PathBuf {
        let n = self.staged.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!("incoming-{}-{n}", std::process::id()))
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

    pub fn read_idx(&self, hash: &Oid) -> io::Result<Vec<IdxEntry>> {
        pack::decode_idx(&fs::read(self.idx_path(hash))?)
    }
}
