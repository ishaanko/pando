//! Garbage collection: delete packs that nothing can reach anymore (deleted branches,
//! history dropped by a force push, rejected pushes, and pieces compaction replaced).
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

use crate::meta::{CHAIN, Meta};
use crate::object::Oid;
use crate::planner;
use crate::server::Repo;
use crate::store::BlobStore;
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub live: usize,
    /// Packs newly found dead (deleted after the grace period).
    pub marked: usize,
    pub deleted: usize,
    pub bytes: u64,
}

pub fn run(repo: &Repo, grace: Duration) -> anyhow::Result<Report> {
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
            let mut stmt = meta.conn.prepare("SELECT hash, dead_since FROM packs")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
                let (hash, since) = row?;
                if let Some(h) = Oid::from_hex(&hash) {
                    packs.push((h, since));
                }
            }
        }
        let tx = meta.conn.transaction()?;
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
            let mut stmt = meta.conn.prepare("SELECT hash FROM packs")?;
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
        };
        let mut work: Vec<Oid> = Vec::new();
        let need = |live: &mut Live, trunk: u64| {
            if trunk < epoch {
                live.old_max = live.old_max.max(trunk);
            }
        };
        for v in meta.refs(&[])? {
            if !v.on_trunk {
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
            work.extend(cover.pack);
            work.extend(cover.side);
        }
        let mut old_done = 0;
        loop {
            while let Some(p) = work.pop() {
                if live.packs.insert(p)
                    && let Some(deps) = meta.pack_deps(&p)?
                {
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
            work.extend(piece.pack);
            work.extend(piece.side);
        }
        Ok(())
    }

    fn trunk_position_live(&self, seq: u64) -> bool {
        seq == 0 || seq <= self.old_max || seq >= self.epoch
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
            .conn
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
    let tx = meta.conn.transaction()?;
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

    let tx = meta.conn.transaction()?;
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
