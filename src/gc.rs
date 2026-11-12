//! Garbage collection: delete packs that nothing can reach anymore (deleted branches,
//! history dropped by a force push, rejected pushes, and pieces compaction replaced),
//! and trim history a rewrite dropped out of the compaction cover that still holds it.
//!
//! Live data starts at the refs: each current version's pack and dependencies, the
//! current trunk epoch, and earlier-epoch trunk positions up to the highest one
//! anything live still depends on (`old_max`). Trunk ranges are live as the pieces
//! the planner serves them with (`planner::pieces`), so a cover that holds even one
//! live position is live: once compaction has merged a version, its cover holds the
//! only copy. A merged version's own pack is live only if something names it as a
//! dependency. Pending covers (being recorded by compaction) are live. Pack
//! dependencies are followed transitively, and may raise `old_max`.
//!
//! A pack found dead is deleted only once it has stayed dead for `grace`, because
//! fetches in flight and signed pack URLs may still use it. Until then it stays fully
//! indexed, so a push that references it simply makes it live again. When it goes,
//! index rows that pointed into it move to another live pack holding the same object,
//! or are dropped along with versions that can no longer be served. Compaction moves
//! trunk rows off the packs it replaces before they can die, so that search is only
//! needed for history that really became unreachable.
//!
//! Trimming. A cover of an earlier epoch can straddle `old_max` (start <= old_max <
//! end): it is live, but the positions above `old_max` were dropped by a rewrite (or
//! by a branch going away), and their objects would stay on disk and fetchable for as
//! long as the cover does. After collecting, gc replaces such a cover's pack with a
//! trimmed one, once all of these hold:
//!
//! - No pack, live or dead, depends on an earlier-epoch position above `old_max`.
//!   Every stored pack can come back (a dead one through a push that references it),
//!   and dead packs go after `grace`, so this holds from the run that deletes the last
//!   one: what is dropped can no longer be needed. Versions are never revived.
//! - Nothing names the cover's pack as a dependency: no live ref version or piece,
//!   and no pack, live or dead. A push that builds on an object homed in the cover
//!   records the whole pack (a branch based on dropped history keeps it reachable),
//!   which then stays whole while that lives; trimming would store it twice.
//! - No pending cover overlaps it (compaction may be splicing it into a larger one).
//! - Some index row homed in it is marked with a dropped position.
//!
//! The trimmed pack holds the cover's objects that its live positions reach, walking
//! from their tips through commits and trees and never leaving the cover: anything
//! they reach only through an object stored elsewhere is complete there, and
//! everything else (dropped history, and objects of other refs that rode along in a
//! trunk push and are no longer referenced) is left out. It is written by the
//! generated-fetch writer, so an object stored as a delta against one left out is
//! written whole, and the pack is self-contained like every stored pack.
//!
//! The pack is built outside the metadata lock. The swap then re-checks everything
//! under the lock (the cover still has the same pack, `old_max` and the epoch are
//! unchanged, nothing names it) and in one transaction points the cover at the new
//! pack, moves the kept objects' index rows to it, deletes the dropped versions, and
//! marks the old pack dead: nothing references it now. Rows of left-out objects stay
//! with it (a row must never point at a pack that lacks its object), so until it goes
//! after `grace` they are still served from it, and a push that references one revives
//! it whole. Then they go with it. Dropped history is therefore gone two grace periods
//! after the first run that sees the rewrite (each rounded up to the next run). A crash
//! before the swap leaves the repository as it was (and a staging file, removed
//! later), and the next run trims again; the swap itself is one transaction.

use crate::meta::{CHAIN, Meta, Piece};
use crate::object::{GITLINK_MODE, Kind, Oid, commit_parents, commit_tree, tree_entries};
use crate::odb::Odb;
use crate::pack::IdxEntry;
use crate::planner;
use crate::server::Repo;
use crate::store::BlobStore;
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub live: usize,
    /// Packs newly found dead (deleted after the grace period).
    pub marked: usize,
    pub deleted: usize,
    pub bytes: u64,
    /// Covers trimmed of history a rewrite dropped.
    pub trimmed: usize,
}

/// Collect garbage, then trim the cover that straddles `old_max` if one is due.
pub fn run(repo: &Repo, grace: Duration) -> anyhow::Result<Report> {
    let mut report = collect(repo, grace)?;
    report.trimmed = usize::from(trim(repo)?);
    Ok(report)
}

/// Mark dead packs, and delete those dead for `grace` (everything [`run`] does but
/// trimming).
pub fn collect(repo: &Repo, grace: Duration) -> anyhow::Result<Report> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let mut report = Report::default();
    let mut doomed: Vec<Oid> = Vec::new();
    let mut newly_dead: Vec<Oid> = Vec::new();
    {
        let mut meta = repo.meta.lock().unwrap();
        let live = Live::compute(&meta)?;
        report.live = live.packs.len();
        // Compaction merges earlier epochs up to here.
        meta.set_old_needed(live.old_max)?;

        let mut packs: Vec<(Oid, Option<i64>)> = Vec::new();
        {
            let mut stmt = meta.conn().prepare("SELECT hash, dead_since FROM packs")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
                let (hash, since) = row?;
                if let Some(h) = Oid::from_hex(&hash) {
                    packs.push((h, since));
                }
            }
        }
        let tx = meta.conn().transaction()?;
        for (hash, since) in &packs {
            match (live.packs.contains(hash), since) {
                (true, Some(_)) => {
                    tx.execute(
                        "UPDATE packs SET dead_since = NULL WHERE hash = ?1",
                        [hash.hex()],
                    )?;
                }
                (true, None) => {}
                (false, since) => {
                    let since = match since {
                        Some(s) => *s,
                        None => {
                            report.marked += 1;
                            newly_dead.push(*hash);
                            tx.execute(
                                "UPDATE packs SET dead_since = ?1 WHERE hash = ?2",
                                params![now, hash.hex()],
                            )?;
                            now
                        }
                    };
                    if now - since >= grace.as_secs() as i64 {
                        doomed.push(*hash);
                    }
                }
            }
        }
        tx.commit()?;
        // What is still reachable through other packs stays servable.
        rehome(&mut meta, &repo.store, &live, &newly_dead)?;
        if !doomed.is_empty() {
            forget(&mut meta, &repo.store, &live, &doomed)?;
        }
        // Files go while the lock is held: a push publishes its pack under the same
        // lock, so a pack pushed again right now cannot be deleted under it.
        for hash in &doomed {
            report.bytes += repo.store.delete(hash)?;
            report.deleted += 1;
        }
        // Pack files no metadata knows about (a push that failed after storing its
        // pack), once old enough.
        let known: HashSet<Oid> = {
            let mut stmt = meta.conn().prepare("SELECT hash FROM packs")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .filter_map(|h| h.ok().and_then(|h| Oid::from_hex(&h)))
                .collect()
        };
        for (hash, written) in repo.store.list()? {
            if !known.contains(&hash) && written.elapsed().is_ok_and(|e| e >= orphan_age(grace)) {
                report.bytes += repo.store.delete(&hash)?;
                report.deleted += 1;
            }
        }
        repo.store.clean_staging(orphan_age(grace))?;
        crate::lfs::clean_staging(&repo.dir, orphan_age(grace))?;
    }
    if report.deleted > 0 {
        crate::fetch_cache::clear(&repo.dir);
    }
    crate::metrics::GC_PACKS_DELETED.add(report.deleted as u64);
    crate::metrics::GC_BYTES_DELETED.add(report.bytes);
    if report.marked + report.deleted > 0 {
        tracing::info!(repo = %repo.name, ?report, "gc");
    }
    Ok(report)
}

/// How old an unindexed pack file or staging file must be before it is removed.
fn orphan_age(grace: Duration) -> Duration {
    grace.max(Duration::from_secs(600))
}

/// What is reachable from the refs.
struct Live {
    packs: HashSet<Oid>,
    /// Chain positions `1..=old_max` of earlier epochs are live.
    old_max: u64,
    epoch: u64,
    /// Live cover rows (pieces of live trunk ranges, and pending covers), by (level,
    /// start).
    covers: HashSet<(u32, u64)>,
    /// Packs live things name directly (as a ref version's pack, or a dependency), as
    /// opposed to serving them as a trunk piece.
    named: HashSet<Oid>,
}

impl Live {
    fn compute(meta: &Meta) -> anyhow::Result<Live> {
        let epoch = meta.epoch_start;
        let head = meta.trunk_head()?.map_or(0, |v| v.seq);
        let mut live = Live {
            packs: HashSet::new(),
            old_max: 0,
            epoch,
            covers: HashSet::new(),
            named: HashSet::new(),
        };
        let mut work: Vec<Oid> = Vec::new();
        let need = |live: &mut Live, trunk: u64| {
            if trunk < epoch {
                live.old_max = live.old_max.max(trunk);
            }
        };
        for v in meta.refs(&[])? {
            if !v.on_trunk {
                live.named.extend(v.pack);
                live.named.extend(v.deps.packs.iter().copied());
                work.extend(v.pack);
                work.extend(v.deps.packs);
                need(&mut live, v.deps.trunk);
            }
        }
        // The current epoch, and what its first version needed from earlier ones.
        live.add_pieces(meta, epoch - 1, head, &mut work)?;
        need(&mut live, meta.trunk_dep_below(epoch, head, epoch)?);
        for cover in meta.pending_covers()? {
            live.covers.insert((cover.level, cover.start));
            live.named.extend(cover.side.iter().copied());
            work.extend(cover.pack);
            work.extend(cover.side);
        }
        let mut old_done = 0;
        loop {
            while let Some(p) = work.pop() {
                if live.packs.insert(p)
                    && let Some(deps) = meta.pack_deps(&p)?
                {
                    live.named.extend(deps.packs.iter().copied());
                    work.extend(deps.packs);
                    need(&mut live, deps.trunk);
                }
            }
            if live.old_max <= old_done {
                break;
            }
            // Earlier epochs: every chain position up to the needed one, as served.
            live.add_pieces(meta, old_done, live.old_max, &mut work)?;
            old_done = live.old_max;
        }
        Ok(live)
    }

    /// Mark the pieces holding positions `(have, want]` live.
    fn add_pieces(
        &mut self,
        meta: &Meta,
        have: u64,
        want: u64,
        work: &mut Vec<Oid>,
    ) -> anyhow::Result<()> {
        for piece in planner::pieces(meta, have, want)? {
            if piece.level > 0 {
                self.covers.insert((piece.level, piece.start));
            }
            self.named.extend(piece.side.iter().copied());
            work.extend(piece.pack);
            work.extend(piece.side);
        }
        Ok(())
    }

    fn trunk_position_live(&self, seq: u64) -> bool {
        seq == 0 || seq <= self.old_max || seq >= self.epoch
    }
}

/// Trim the cover that straddles `old_max`, if one is due (see the module docs).
/// Returns whether one was trimmed.
pub fn trim(repo: &Repo) -> anyhow::Result<bool> {
    let Some(trim) = Trim::plan(&repo.meta.lock().unwrap())? else {
        return Ok(false);
    };
    let built = trim.build(&repo.store)?;
    trim.swap(repo, built)
}

/// A cover to trim: the piece holding `old_max`, and the live positions in it.
#[derive(Debug, PartialEq, Eq)]
pub struct Trim {
    cover: Piece,
    /// `old_max`: positions above it are dropped.
    cut: u64,
    epoch: u64,
    /// Tips of the cover's chain versions up to `cut`, by position.
    tips: Vec<(u64, Oid)>,
}

/// A trimmed pack waiting in staging, and what it keeps.
pub struct Built {
    /// `None` when every object is kept (only index rows need fixing).
    staged: Option<(PathBuf, Oid, Vec<IdxEntry>)>,
    /// Kept objects, each with the first live position that reaches it.
    kept: HashMap<Oid, u64>,
}

impl Trim {
    /// The cover to trim now, if any. Reads metadata only.
    pub fn plan(meta: &Meta) -> anyhow::Result<Option<Trim>> {
        let epoch = meta.epoch_start;
        if epoch == 1 {
            return Ok(None);
        }
        let live = Live::compute(meta)?;
        let cut = live.old_max;
        if cut == 0 {
            return Ok(None);
        }
        let Some(cover) = planner::piece_at(meta, cut)? else {
            return Ok(None);
        };
        let Some(pack) = cover.pack else {
            return Ok(None);
        };
        // Positions `epoch..` are live too, so the cover must end before them.
        if cover.level == 0 || cover.end <= cut || cover.end >= epoch {
            return Ok(None);
        }
        // Anything that can still be revived needs nothing above `cut`.
        let revivable = meta.max_trunk_dep_below(epoch)?;
        let named = live.named.contains(&pack) || meta.pack_is_named(&pack)?;
        let pending = meta
            .pending_covers()?
            .iter()
            .any(|p| p.start <= cover.end && cover.start <= p.end);
        if revivable > cut || named || pending {
            return Ok(None);
        }
        if !meta.homes_positions_between(&pack, cut, epoch)? {
            return Ok(None);
        }
        let mut tips = Vec::new();
        for seq in cover.start..=cut {
            if let Some(v) = meta.trunk_version(seq)? {
                tips.push((seq, v.tip));
            }
        }
        Ok(Some(Trim {
            cover,
            cut,
            epoch,
            tips,
        }))
    }

    /// Find what the live positions reach inside the cover and write it to staging.
    /// Reads the cover's pack only; takes no lock.
    pub fn build(&self, store: &BlobStore) -> anyhow::Result<Built> {
        let pack = self.cover.pack.expect("planned with a pack");
        let format = pack.format();
        let odb = Odb::open(store, format, &[pack])?;
        let mut kept: HashMap<Oid, u64> = HashMap::new();
        for &(seq, tip) in &self.tips {
            let mut stack = vec![tip];
            while let Some(oid) = stack.pop() {
                if !odb.contains(&oid) || kept.contains_key(&oid) {
                    continue;
                }
                kept.insert(oid, seq);
                let (kind, data) = odb
                    .read(&oid)?
                    .ok_or_else(|| anyhow::anyhow!("missing object {oid}"))?;
                match kind {
                    Kind::Commit => {
                        stack.extend(commit_parents(&data));
                        stack.extend(commit_tree(&data));
                    }
                    Kind::Tree => {
                        let entries = tree_entries(format, &data)
                            .ok_or_else(|| anyhow::anyhow!("malformed tree {oid}"))?;
                        for (mode, child) in entries {
                            if mode & 0o170000 == 0o040000 {
                                stack.push(child);
                            } else if mode != GITLINK_MODE && odb.contains(&child) {
                                kept.entry(child).or_insert(seq);
                            }
                        }
                    }
                    Kind::Blob | Kind::Tag => {}
                }
            }
        }
        let idx = store.read_idx(&pack)?;
        let objects: Vec<Oid> = idx
            .iter()
            .map(|e| e.oid)
            .filter(|o| kept.contains_key(o))
            .collect();
        let staged = if objects.len() == idx.len() {
            None
        } else {
            let (path, (hash, idx)) = store.stage(|out| {
                crate::generate::write_pack(&odb, &objects, out).map_err(std::io::Error::other)
            })?;
            Some((path, hash, idx))
        };
        Ok(Built { staged, kept })
    }

    /// Publish the trimmed pack and make it the cover's, if the plan still holds.
    /// Returns false (discarding the build) if it does not.
    pub fn swap(&self, repo: &Repo, built: Built) -> anyhow::Result<bool> {
        // Under the lock, like a push, so gc cannot delete a pack with the same hash
        // between its publication and its row.
        let mut meta = repo.meta.lock().unwrap();
        if Trim::plan(&meta)?.as_ref() != Some(self) {
            if let Some((path, ..)) = &built.staged {
                let _ = std::fs::remove_file(path);
            }
            return Ok(false);
        }
        let new = match &built.staged {
            Some((path, hash, idx)) => {
                repo.store.publish(path, hash, idx)?;
                *hash
            }
            None => self.cover.pack.expect("planned with a pack"),
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        meta.trim_cover(&self.cover, &new, self.cut, &built.kept, now)?;
        tracing::info!(
            repo = %repo.name,
            level = self.cover.level,
            start = self.cover.start,
            cut = self.cut,
            kept = built.kept.len(),
            pack = %new,
            "trimmed cover"
        );
        Ok(true)
    }
}

/// The highest earlier-epoch trunk position anything still depends on.
pub fn old_needed(meta: &Meta) -> anyhow::Result<u64> {
    Ok(Live::compute(meta)?.old_max)
}

/// Packs the refs need, the way gc sees them. `pando verify` checks these are stored.
pub fn needed_packs(meta: &Meta) -> anyhow::Result<HashSet<Oid>> {
    Ok(Live::compute(meta)?.packs)
}

/// Drop `doomed` packs from the metadata, re-homing objects that live packs also hold.
/// Remove packs from the metadata of a copy (a backup) that does not have them.
/// Fails if any of them is still needed.
pub fn drop_packs(meta: &mut Meta, store: &BlobStore, packs: &[Oid]) -> anyhow::Result<()> {
    let live = Live::compute(meta)?;
    if let Some(p) = packs.iter().find(|p| live.packs.contains(p)) {
        anyhow::bail!("pack {p} is still needed");
    }
    forget(meta, store, &live, packs)
}

/// Point index rows away from `packs` (dead ones) to live packs that hold the same
/// objects, so anything still reachable keeps being served. Returns the objects no
/// live pack holds.
fn rehome(
    meta: &mut Meta,
    store: &BlobStore,
    live: &Live,
    packs: &[Oid],
) -> anyhow::Result<HashSet<Oid>> {
    let mut orphans: HashSet<Oid> = HashSet::new();
    {
        let mut stmt = meta
            .conn()
            .prepare_cached("SELECT oid FROM objects WHERE pack = ?1")?;
        for p in packs {
            for oid in stmt.query_map([p.hex()], |r| r.get::<_, String>(0))? {
                orphans.extend(Oid::from_hex(&oid?));
            }
        }
    }
    let mut home: HashMap<Oid, Oid> = HashMap::new();
    if !orphans.is_empty() {
        for pack in &live.packs {
            let Ok(idx) = store.read_idx(pack) else {
                continue;
            };
            for e in idx {
                if orphans.contains(&e.oid) {
                    home.entry(e.oid).or_insert(*pack);
                }
            }
            if home.len() == orphans.len() {
                break;
            }
        }
    }
    let tx = meta.conn().transaction()?;
    for (oid, pack) in &home {
        tx.execute(
            "UPDATE objects SET pack = ?1, trunk_seq = NULL WHERE oid = ?2",
            params![pack.hex(), oid.hex()],
        )?;
        // Commits remember the first pack that stored them.
        tx.execute(
            "UPDATE commits SET pack = ?1 WHERE oid = ?2",
            params![pack.hex(), oid.hex()],
        )?;
    }
    tx.commit()?;
    orphans.retain(|o| !home.contains_key(o));
    Ok(orphans)
}

fn forget(meta: &mut Meta, store: &BlobStore, live: &Live, doomed: &[Oid]) -> anyhow::Result<()> {
    let dead: HashSet<Oid> = doomed.iter().copied().collect();
    let hexes: Vec<String> = doomed.iter().map(Oid::hex).collect();
    let orphans = rehome(meta, store, live, doomed)?;

    let tx = meta.conn().transaction()?;
    for oid in &orphans {
        tx.execute("DELETE FROM objects WHERE oid = ?1", [oid.hex()])?;
        tx.execute("DELETE FROM tags WHERE oid = ?1", [oid.hex()])?;
    }
    // Commits whose first pack is going: wherever the object index says they live
    // now, if anywhere.
    for h in &hexes {
        let oids: Vec<String> = tx
            .prepare_cached("SELECT oid FROM commits WHERE pack = ?1")?
            .query_map([h], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for oid in oids {
            let pack: Option<String> = tx
                .query_row("SELECT pack FROM objects WHERE oid = ?1", [&oid], |r| {
                    r.get(0)
                })
                .optional()?;
            match pack {
                Some(p) => tx.execute(
                    "UPDATE commits SET pack = ?1 WHERE oid = ?2",
                    params![p, oid],
                )?,
                None => tx.execute("DELETE FROM commits WHERE oid = ?1", [&oid])?,
            };
        }
    }
    // Versions that can no longer be served (current ref versions are always live).
    let current: HashSet<i64> = tx
        .prepare("SELECT version FROM refs")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let gone = |h: &str| Oid::from_hex(h).is_some_and(|o| dead.contains(&o));
    let unservable: Vec<i64> = tx
        .prepare("SELECT id, ref, seq, pack, trunk_dep, side_deps, merged FROM versions")?
        .query_map([], |r| {
            let (id, vref, seq): (i64, String, u64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            let (pack, trunk_dep, side): (Option<String>, u64, String) =
                (r.get(3)?, r.get(4)?, r.get(5)?);
            let merged: bool = r.get(6)?;
            // A merged version is served by its cover, which is live with its position.
            let servable = (merged
                || !pack.as_deref().is_some_and(gone) && !side.split_whitespace().any(gone))
                && live.trunk_position_live(trunk_dep)
                && (vref != CHAIN || live.trunk_position_live(seq));
            Ok((id, servable))
        })?
        .filter(|row| !matches!(row, Ok((id, servable)) if *servable || current.contains(id)))
        .map(|row| row.map(|(id, _)| id))
        .collect::<rusqlite::Result<_>>()?;
    for id in unservable {
        tx.execute("DELETE FROM versions WHERE id = ?1", [id])?;
    }
    let covers: Vec<(u32, u64)> = tx
        .prepare("SELECT level, start FROM covers")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for c in covers {
        if !live.covers.contains(&c) {
            tx.execute(
                "DELETE FROM covers WHERE level = ?1 AND start = ?2",
                params![c.0, c.1],
            )?;
        }
    }
    for h in &hexes {
        tx.execute("DELETE FROM packs WHERE hash = ?1", [h])?;
    }
    tx.commit()?;
    Ok(())
}
