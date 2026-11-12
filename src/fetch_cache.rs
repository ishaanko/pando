//! The fetch cache: generated answers (see `generate.rs`) kept on disk, so that
//! identical shallow or filtered clones, like CI jobs cloning one tip with `--depth 1`,
//! cost one history walk instead of one each.
//!
//! An entry is a pack plus the `shallow`/`unshallow` lines that go with it, stored at
//! `<repo>.git/fetch-cache/<key>.answer`, where the key hashes everything the answer
//! depends on (`generate::Answer::prepare` builds it). Entries are written to
//! `<key>.tmp` and renamed into place, so only complete files are ever visible;
//! leftover temporaries are removed at startup. Entries are synced before the rename,
//! and one whose trailer does not check out is treated as missing and deleted.
//!
//! Concurrent requests for one key are coalesced: the first generates while the
//! others wait for its entry. Across all repositories the cache stays under a size
//! limit, evicting the least recently served entries after each fill. Backups skip
//! it, and gc clears a repository's entries when it deletes packs.

use crate::object::Oid;
use crate::server::repo_names;
use memmap2::Mmap;
use std::collections::HashSet;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};

/// The cache directory inside a repository directory.
const DIR: &str = "fetch-cache";
/// Ends every entry, after the pack and the shallow lines.
const MAGIC: &[u8; 8] = b"PANDOFC1";

pub struct FetchCache {
    data: PathBuf,
    /// Size limit in bytes across all repositories; 0 turns the cache off.
    max_bytes: u64,
    /// Entries being generated right now, by path.
    filling: Mutex<HashSet<PathBuf>>,
    filled: Condvar,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// A cached answer, mapped into memory. Entries are never modified in place, so the
/// mapping stays valid even if the file is evicted meanwhile.
pub struct Entry {
    map: Mmap,
    pack_len: usize,
    pub shallow: Vec<Oid>,
    pub unshallow: Vec<Oid>,
}

pub enum Lookup<'a> {
    Hit(Entry),
    /// Nobody has this answer yet: generate it and [`Fill::store`] it. Others asking
    /// for it wait until the `Fill` is stored or dropped.
    Fill(Fill<'a>),
}

pub struct Fill<'a> {
    cache: &'a FetchCache,
    path: PathBuf,
}

impl FetchCache {
    /// The cache for every repository under `data`. Clears temporaries left by a
    /// previous process, and every entry if the cache is off.
    pub fn open(data: &Path, max_bytes: u64) -> io::Result<FetchCache> {
        for dir in cache_dirs(data)? {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if max_bytes == 0 || path.extension().is_some_and(|e| e == "tmp") {
                    let _ = fs::remove_file(path);
                }
            }
        }
        Ok(FetchCache {
            data: data.to_path_buf(),
            max_bytes,
            filling: Mutex::new(HashSet::new()),
            filled: Condvar::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    pub fn enabled(&self) -> bool {
        self.max_bytes > 0
    }

    /// Hits and misses (answers generated into the cache) since startup.
    pub fn counts(&self) -> (u64, u64) {
        (self.hits.load(Relaxed), self.misses.load(Relaxed))
    }

    /// The entry for `key` in the repository at `repo_dir`, or the job of filling it.
    /// Blocks while another request is filling it.
    pub fn lookup(&self, repo_dir: &Path, key: &Oid) -> Lookup<'_> {
        let path = repo_dir.join(DIR).join(format!("{key}.answer"));
        let mut filling = self.filling.lock().unwrap();
        loop {
            if let Some(entry) = Entry::open(&path) {
                // Served recently: the last to be evicted.
                if let Ok(f) = fs::File::open(&path) {
                    let _ = f.set_modified(std::time::SystemTime::now());
                }
                self.hits.fetch_add(1, Relaxed);
                crate::metrics::FETCH_CACHE_HITS.inc();
                return Lookup::Hit(entry);
            }
            if filling.insert(path.clone()) {
                self.misses.fetch_add(1, Relaxed);
                crate::metrics::FETCH_CACHE_MISSES.inc();
                return Lookup::Fill(Fill { cache: self, path });
            }
            filling = self.filled.wait(filling).unwrap();
        }
    }

    /// Delete the least recently served entries until the cache fits, sparing `keep`.
    fn evict(&self, keep: &Path) -> io::Result<()> {
        let mut entries = Vec::new();
        let mut total = 0;
        for dir in cache_dirs(&self.data)? {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "answer") {
                    let m = entry.metadata()?;
                    total += m.len();
                    entries.push((m.modified()?, m.len(), path));
                }
            }
        }
        entries.sort();
        for (_, len, path) in entries {
            if total <= self.max_bytes {
                break;
            }
            if path != keep && fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
        Ok(())
    }
}

impl Fill<'_> {
    /// Write an answer into the cache (`pack` writes the pack) and return it as an
    /// entry. Waiting requests then find it.
    pub fn store(
        self,
        shallow: &[Oid],
        unshallow: &[Oid],
        pack: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
    ) -> anyhow::Result<Entry> {
        // Not create_dir_all: a repository deleted or renamed meanwhile stays gone.
        match fs::create_dir(self.path.parent().unwrap()) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
            _ => {}
        }
        let tmp = self.path.with_extension("tmp");
        let written = (|| -> anyhow::Result<()> {
            let mut file = BufWriter::new(fs::File::create(&tmp)?);
            pack(&mut file)?;
            for oid in shallow.iter().chain(unshallow) {
                file.write_all(&oid.0)?;
            }
            file.write_all(&(shallow.len() as u32).to_be_bytes())?;
            file.write_all(&(unshallow.len() as u32).to_be_bytes())?;
            file.write_all(MAGIC)?;
            // Synced, so a power loss cannot leave an entry with a valid trailer and
            // a hole in the middle.
            file.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            Ok(fs::rename(&tmp, &self.path)?)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        let entry =
            Entry::open(&self.path).ok_or_else(|| anyhow::anyhow!("fetch cache entry vanished"))?;
        let (cache, path) = (self.cache, self.path.clone());
        drop(self);
        if let Err(e) = cache.evict(&path) {
            tracing::warn!("fetch cache eviction failed: {e}");
        }
        Ok(entry)
    }
}

impl Drop for Fill<'_> {
    fn drop(&mut self) {
        self.cache.filling.lock().unwrap().remove(&self.path);
        self.cache.filled.notify_all();
    }
}

impl Entry {
    /// A complete entry, or `None` (deleting a damaged one).
    fn open(path: &Path) -> Option<Entry> {
        let map = crate::store::map_file(path).ok()?;
        let entry = Entry::parse(map);
        if entry.is_none() {
            let _ = fs::remove_file(path);
        }
        entry
    }

    fn parse(map: Mmap) -> Option<Entry> {
        let tail = map.len().checked_sub(16)?;
        if &map[tail + 8..] != MAGIC {
            return None;
        }
        let count = |at: usize| u32::from_be_bytes(map[at..at + 4].try_into().unwrap()) as usize;
        let (ns, nu) = (count(tail), count(tail + 4));
        let pack_len = tail.checked_sub((ns + nu) * 20)?;
        if pack_len < 32 || &map[..4] != b"PACK" {
            return None;
        }
        let oids = |from: usize, n: usize| -> Vec<Oid> {
            map[from..from + n * 20]
                .chunks(20)
                .map(|c| Oid(c.try_into().unwrap()))
                .collect()
        };
        Some(Entry {
            shallow: oids(pack_len, ns),
            unshallow: oids(pack_len + ns * 20, nu),
            pack_len,
            map,
        })
    }

    pub fn pack(&self) -> &[u8] {
        &self.map[..self.pack_len]
    }
}

/// Delete every cached answer of the repository at `repo_dir`. Gc calls this after
/// deleting packs, so commits it dropped do not linger on disk in cached copies.
pub fn clear(repo_dir: &Path) {
    if let Ok(entries) = fs::read_dir(repo_dir.join(DIR)) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "answer") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// The cache directories of every repository that has one.
fn cache_dirs(data: &Path) -> io::Result<Vec<PathBuf>> {
    let names = repo_names(data).map_err(io::Error::other)?;
    Ok(names
        .into_iter()
        .map(|n| data.join("repos").join(format!("{n}.git")).join(DIR))
        .filter(|d| d.is_dir())
        .collect())
}
