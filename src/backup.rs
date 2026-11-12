//! Backups. `pando backup` takes a consistent copy of a data directory while the
//! server keeps running, `pando restore` puts one in place, and `pando verify` checks
//! that every pack the metadata knows is present (and with `--full`, intact), and that
//! everything the refs need is: every trunk position is held by a stored piece (its
//! version's pack, or the compaction cover that merged it), as gc and the planner
//! see it.
//!
//! Each repository's metadata is snapshotted with SQLite's `VACUUM INTO`. Packs never
//! change once written, so they are hard-linked (or copied across filesystems) after
//! the snapshot. A pack that gc deletes in between was already unreachable in the
//! snapshot, so it is skipped and forgotten in the copy, as gc forgot it. LFS objects
//! are immutable too and are linked the same way; one uploaded during the backup may
//! or may not make it in.
//!
//! Both commands fsync everything they wrote before reporting success.

use crate::meta::Meta;
use crate::object::{Format, Oid};
use crate::server::repo_names;
use crate::store::{BlobStore, map_file, sync_dir};
use std::fs;
use std::path::Path;

#[derive(Debug, Default)]
pub struct Summary {
    pub repos: usize,
    pub packs: usize,
    pub skipped: usize,
    pub lfs_objects: usize,
}

/// Copy `data` into `out`, which must not exist yet or be empty.
pub fn backup(data: &Path, out: &Path) -> anyhow::Result<Summary> {
    ensure_empty(out)?;
    let mut summary = Summary::default();
    for name in repo_names(data)? {
        let src = data.join("repos").join(format!("{name}.git"));
        let dst = out.join("repos").join(format!("{name}.git"));
        fs::create_dir_all(dst.join("blobs/packs"))?;
        fs::create_dir_all(dst.join("blobs/tmp"))?;
        let conn = rusqlite::Connection::open(src.join("meta.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute(
            "VACUUM INTO ?1",
            [dst.join("meta.db").to_string_lossy().as_ref()],
        )?;
        let snapshot = rusqlite::Connection::open(dst.join("meta.db"))?;
        let packs: Vec<String> = snapshot
            .prepare("SELECT hash FROM packs")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(snapshot);
        let mut skipped = Vec::new();
        for hash in packs {
            let mut copied = true;
            for ext in ["pack", "pidx"] {
                let file = format!("blobs/packs/{hash}.{ext}");
                if !link_or_copy(&src.join(&file), &dst.join(&file))? {
                    copied = false;
                }
            }
            if copied {
                summary.packs += 1;
            } else {
                skipped.extend(Oid::from_hex(&hash));
            }
        }
        if !skipped.is_empty() {
            // gc deleted these after the snapshot: forget them in the copy too.
            for hash in &skipped {
                for ext in ["pack", "pidx"] {
                    let _ = fs::remove_file(dst.join(format!("blobs/packs/{hash}.{ext}")));
                }
            }
            // The snapshot exists, so it keeps its own object format.
            let mut meta = Meta::open(dst.join("meta.db"), "refs/heads/main", Format::Sha1)?;
            let store = BlobStore::open(dst.join("blobs"))?;
            crate::gc::drop_packs(&mut meta, &store, &skipped)
                .map_err(|e| anyhow::anyhow!("{name}: {e:#}"))?;
            summary.skipped += skipped.len();
        }
        for (oid, from) in crate::lfs::stored_objects(&src)? {
            let to = crate::lfs::object_path(&dst, &oid);
            fs::create_dir_all(to.parent().unwrap())?;
            summary.lfs_objects += usize::from(link_or_copy(&from, &to)?);
        }
        copy_dir(&src.join("hooks"), &dst.join("hooks"))?;
        summary.repos += 1;
    }
    copy_dir(&data.join("hooks"), &out.join("hooks"))?;
    let key = data.join("ssh_host_ed25519_key");
    if key.exists() {
        fs::copy(&key, out.join("ssh_host_ed25519_key"))?;
    }
    sync_tree(out)?;
    let problems = verify(out, false)?;
    anyhow::ensure!(
        problems.is_empty(),
        "backup is incomplete:\n{}",
        problems.join("\n")
    );
    Ok(summary)
}

/// Put a backup in place as a new data directory, then verify it in full.
pub fn restore(from: &Path, data: &Path) -> anyhow::Result<Summary> {
    ensure_empty(data)?;
    let names = repo_names(from)?;
    anyhow::ensure!(
        !names.is_empty() || from.join("repos").exists(),
        "{} is not a pando backup",
        from.display()
    );
    let mut summary = Summary::default();
    copy_tree(from, data, &mut summary)?;
    sync_tree(data)?;
    summary.repos = names.len();
    let problems = verify(data, true)?;
    anyhow::ensure!(
        problems.is_empty(),
        "restored data has problems:\n{}",
        problems.join("\n")
    );
    Ok(summary)
}

/// Problems found in a data directory: packs the metadata knows that are missing or
/// (with `full`) corrupt, packs the refs need that are not stored, refs whose tips
/// are not stored, and stray or (with `full`) corrupt LFS objects.
pub fn verify(data: &Path, full: bool) -> anyhow::Result<Vec<String>> {
    let mut problems = Vec::new();
    for name in repo_names(data)? {
        let dir = data.join("repos").join(format!("{name}.git"));
        let mut meta = Meta::open(dir.join("meta.db"), "refs/heads/main", Format::Sha1)?;
        let store = BlobStore::open(dir.join("blobs"))?;
        let mut known: Vec<Oid> = meta
            .conn()
            .prepare("SELECT hash FROM packs")?
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|h| h.ok().and_then(|h| Oid::from_hex(&h)))
            .collect();
        known.sort();
        for hash in known {
            if hash.format() != meta.format {
                problems.push(format!("{name}: pack {hash} is not {}", meta.format.name()));
            }
            if !store.has(&hash) || !store.pack_path(&hash).exists() {
                problems.push(format!("{name}: pack {hash} is missing"));
                continue;
            }
            if full && let Err(e) = check_pack(&store, &hash) {
                problems.push(format!("{name}: pack {hash}: {e}"));
            }
        }
        match crate::gc::needed_packs(&meta) {
            Ok(needed) => {
                let mut needed: Vec<Oid> = needed.into_iter().collect();
                needed.sort();
                for hash in needed {
                    if !store.has(&hash) || !store.pack_path(&hash).exists() {
                        problems.push(format!("{name}: needed pack {hash} is missing"));
                    }
                }
            }
            Err(e) => problems.push(format!("{name}: {e:#}")),
        }
        for v in meta.refs(&[])? {
            if meta.object(&v.tip)?.is_none() {
                problems.push(format!("{name}: {} points at unknown {}", v.refname, v.tip));
            }
        }
        for p in crate::lfs::check(&dir, full)? {
            problems.push(format!("{name}: {p}"));
        }
    }
    Ok(problems)
}

/// A pack's trailer matches its contents and its name, and its index matches it.
/// The name says which hash the trailer is.
fn check_pack(store: &BlobStore, hash: &Oid) -> anyhow::Result<()> {
    let map = map_file(&store.pack_path(hash))?;
    let width = hash.format().raw_len();
    anyhow::ensure!(map.len() >= 12 + width, "truncated");
    let (body, trailer) = map.split_at(map.len() - width);
    let mut sha = hash.format().hasher();
    sha.update(body);
    anyhow::ensure!(
        sha.finish() == *hash && trailer == hash.as_bytes(),
        "checksum mismatch"
    );
    let count = u32::from_be_bytes(body[8..12].try_into()?) as usize;
    let idx = store.read_idx(hash)?;
    anyhow::ensure!(
        idx.len() == count,
        "index lists {} of {count} objects",
        idx.len()
    );
    Ok(())
}

/// Fsync every file and directory under `dir`, and `dir`'s own entry in its parent.
fn sync_tree(dir: &Path) -> anyhow::Result<()> {
    fn walk(dir: &Path) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk(&entry.path())?;
            } else {
                fs::File::open(entry.path())?.sync_all()?;
            }
        }
        sync_dir(dir)
    }
    walk(dir)?;
    let parent = dir.parent().filter(|p| !p.as_os_str().is_empty());
    Ok(sync_dir(parent.unwrap_or(Path::new(".")))?)
}

fn ensure_empty(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        anyhow::ensure!(
            fs::read_dir(dir)?.next().is_none(),
            "{} exists and is not empty",
            dir.display()
        );
    }
    fs::create_dir_all(dir)?;
    Ok(())
}

/// Hard-link `src` to `dst`, or copy it. Returns false if `src` does not exist.
fn link_or_copy(src: &Path, dst: &Path) -> anyhow::Result<bool> {
    if !src.exists() {
        return Ok(false);
    }
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst)?;
    }
    Ok(true)
}

fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if src.is_dir() {
        copy_tree(src, dst, &mut Summary::default())?;
    }
    Ok(())
}

/// Copy a directory tree; immutable packs and LFS objects are hard-linked when possible.
fn copy_tree(src: &Path, dst: &Path, summary: &mut Summary) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to, summary)?;
        } else if from.extension().is_some_and(|e| e == "pack" || e == "pidx")
            || crate::lfs::valid_oid(&entry.file_name().to_string_lossy())
        {
            link_or_copy(&from, &to)?;
            summary.packs += usize::from(from.extension().is_some_and(|e| e == "pack"));
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
