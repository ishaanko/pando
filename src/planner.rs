//! Fetch planning: turn a client's `have`/`want` commits into a list of stored packs.
//!
//! Nothing here looks at objects. Wants and haves are mapped to ref versions through
//! the metadata store, and the answer is a set of existing packs:
//!
//! - Trunk is a linear chain, so "what the client has" collapses to one number
//!   (`trunk_have`) and "what it wants" to another (`trunk_want`). Compaction keeps
//!   every trunk position in exactly one *piece* (a version's own pack, or one cover;
//!   see `compact.rs`), so the range between them is served by walking pieces: the
//!   piece holding `trunk_have + 1`, then the one after it, up to `trunk_want`. If the
//!   client's position falls inside a cover, the whole cover is sent. Compaction's
//!   merge rule keeps that overshoot below the distance the client is behind, and
//!   the walk to O(log Δ) packs.
//! - Any other ref version is one pack plus its trunk dependency.
//! - After trunk is rewritten, positions from earlier epochs are served as every chain
//!   version up to that position (see `meta.rs`), also by walking pieces. An earlier
//!   epoch's chain version that compaction merged is served that way too.
//!
//! Packs may contain objects the client already has. That is the trade: the same
//! bytes are served to every reader, so they can be cached like a CDN.

use crate::meta::{Meta, Piece, Version};
use crate::object::Oid;
use std::collections::{HashSet, VecDeque};

/// Compaction fan-out: a level-L cover spans `FANOUT^L` trunk versions.
pub const FANOUT: u64 = 4;
/// Highest compaction level. Its span (4^16, about 4 billion versions) is never
/// reached in practice, so covers have no fixed ceiling.
pub const MAX_LEVEL: u32 = 16;
/// How many commits to walk back from the haves, in total, looking for known ref
/// version tips.
const HAVE_WALK_LIMIT: usize = 4096;
/// Haves beyond this many are ignored (git clients send far fewer); the cost is only
/// a larger pack.
const MAX_HAVES: usize = 4096;

pub fn span(level: u32) -> u64 {
    FANOUT.pow(level)
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Packs to send, in dependency order (trunk first).
    pub packs: Vec<Oid>,
    /// Haves we recognized. Sent back as ACKs.
    pub common: Vec<Oid>,
    /// True once at least one have pinned down a ref version, i.e. we found a cut.
    pub cut_found: bool,
    pub trunk_have: u64,
    pub trunk_want: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("not our ref {0}")]
    NotOurRef(Oid),
    #[error("trunk version {0} is merged but no cover holds it")]
    Uncovered(u64),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

pub fn plan(meta: &Meta, wants: &[Oid], haves: &[Oid]) -> Result<Plan, PlanError> {
    let mut p = Plan::default();
    let mut had_versions = HashSet::new();
    let mut had_packs = HashSet::new();
    let have_set: HashSet<Oid> = haves.iter().copied().collect();
    let mut walked = HashSet::new();
    let mut considered = HashSet::new();
    for have in haves {
        if considered.len() >= MAX_HAVES {
            break;
        }
        if !considered.insert(*have) {
            continue;
        }
        let (known, tips) = tips_reachable_from(meta, have, &mut walked)?;
        if !known {
            continue;
        }
        p.common.push(*have);
        for v in tips {
            p.cut_found = true;
            if v.on_trunk {
                p.trunk_have = p.trunk_have.max(v.seq);
            } else {
                had_versions.insert(v.id);
                if v.pack_complete {
                    had_packs.extend(v.pack);
                }
            }
        }
    }

    // Side packs still to include, and the trunk ranges they need: `trunk_want` in
    // the current epoch, `old_want` for positions of earlier epochs.
    let mut work: Vec<Oid> = Vec::new();
    let mut old_want = 0;
    let need = |p: &mut Plan, old_want: &mut u64, trunk: u64| {
        if meta.in_epoch(trunk) {
            p.trunk_want = p.trunk_want.max(trunk);
        } else {
            *old_want = (*old_want).max(trunk);
        }
    };
    for want in wants {
        if have_set.contains(want) {
            continue;
        }
        let version = match best_version(meta.versions_with_tip(want)?) {
            Some(v) => v,
            None => {
                // Not a tip: an older commit, or any other object. Serve whichever
                // pack stored it.
                let row = meta.object(want)?.ok_or(PlanError::NotOurRef(*want))?;
                match row.trunk_seq {
                    Some(seq) if meta.in_epoch(seq) => p.trunk_want = p.trunk_want.max(seq),
                    // Its pack and that pack's dependencies.
                    _ => work.push(row.pack),
                }
                continue;
            }
        };
        if version.on_trunk {
            p.trunk_want = p.trunk_want.max(version.seq);
        } else if version.merged {
            // An earlier epoch's chain version whose pack a cover replaced: serve its
            // position (every chain version up to it, a superset).
            need(&mut p, &mut old_want, version.seq);
        } else if !had_versions.contains(&version.id) {
            need(&mut p, &mut old_want, version.deps.trunk);
            work.extend(version.pack);
            work.extend(version.deps.packs);
        }
    }

    // Follow dependencies to a fixed point: side packs can raise the trunk ranges, and
    // newly covered trunk versions can need side packs.
    let mut side = Vec::new();
    let mut seen = HashSet::new();
    let mut covered = p.trunk_have.max(meta.epoch_start - 1);
    let mut old_covered = 0;
    loop {
        while let Some(pack) = work.pop() {
            if had_packs.contains(&pack) || !seen.insert(pack) {
                continue;
            }
            side.push(pack);
            if let Some(deps) = meta.pack_deps(&pack)? {
                need(&mut p, &mut old_want, deps.trunk);
                work.extend(deps.packs);
            }
        }
        if p.trunk_want > covered {
            if covered < meta.epoch_start {
                // The epoch's first version may depend on earlier epochs.
                if let Some(first) = meta.trunk_version(meta.epoch_start)? {
                    need(&mut p, &mut old_want, first.deps.trunk);
                }
            }
            let (packs, deps) = cover_trunk(meta, covered, p.trunk_want)?;
            p.packs.extend(packs);
            work.extend(deps);
            covered = p.trunk_want;
        } else if old_want > old_covered {
            // Earlier epochs: everything up to the needed position, a superset.
            let (packs, deps) = cover_trunk(meta, old_covered, old_want)?;
            p.packs.extend(packs);
            work.extend(deps);
            old_covered = old_want;
        } else {
            break;
        }
    }
    p.packs.extend(side);
    let mut unique = HashSet::new();
    p.packs.retain(|pack| unique.insert(*pack));
    Ok(p)
}

/// Prefer a current trunk version (no extra pack), otherwise the newest version.
fn best_version(mut vs: Vec<Version>) -> Option<Version> {
    if let Some(i) = vs.iter().position(|v| v.on_trunk) {
        return Some(vs.swap_remove(i));
    }
    vs.pop()
}

/// Is `have` a commit we know, and which ref version tips does the client therefore
/// have? Walks parents within a budget shared by all haves (`walked`); reaching a
/// trunk tip stops that path, since trunk tips imply every earlier trunk version of
/// their epoch.
fn tips_reachable_from(
    meta: &Meta,
    have: &Oid,
    walked: &mut HashSet<Oid>,
) -> Result<(bool, Vec<Version>), PlanError> {
    let mut tips = Vec::new();
    let mut known = false;
    let seen = walked;
    seen.insert(*have);
    let mut queue = VecDeque::from([*have]);
    while let Some(oid) = queue.pop_front() {
        let versions = meta.versions_with_tip(&oid)?;
        let hit_trunk = versions.iter().any(|v| v.on_trunk);
        known |= !versions.is_empty();
        tips.extend(versions);
        let Some(row) = meta.commit(&oid)? else {
            continue;
        };
        known = true;
        if hit_trunk {
            continue;
        }
        for parent in row.parents {
            if seen.len() < HAVE_WALK_LIMIT && seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok((known, tips))
}

/// Packs holding trunk positions `(have, want]` (see [`pieces`]), plus the non-trunk
/// packs those positions depend on.
pub fn cover_trunk(meta: &Meta, have: u64, want: u64) -> Result<(Vec<Oid>, Vec<Oid>), PlanError> {
    let mut packs = Vec::new();
    let mut deps = Vec::new();
    for piece in pieces(meta, have, want)? {
        packs.extend(piece.pack);
        deps.extend(piece.side);
    }
    Ok((packs, deps))
}

/// The pieces holding trunk positions `(have, want]`, in order. The first may start
/// at or before `have`, and the last may end after `want`.
pub fn pieces(meta: &Meta, have: u64, want: u64) -> Result<Vec<Piece>, PlanError> {
    let mut out = Vec::new();
    let mut at = have; // positions 1..=at are covered
    while at < want {
        match piece_at(meta, at + 1)? {
            Some(piece) => {
                at = piece.end;
                out.push(piece);
            }
            // gc removes earlier-epoch versions that nothing needed. Such a gap lies
            // above every position anything still depends on, so it holds nothing
            // this range needs.
            None if at + 1 < meta.epoch_start => {
                at = meta.next_trunk_seq(at + 1)?.map_or(want, |next| next - 1);
            }
            None => return Err(PlanError::NotOurRef(Oid::ZERO)),
        }
    }
    Ok(out)
}

/// The piece holding chain position `seq`: its version if not merged, otherwise the
/// cover that merged it. `None` if gc removed the version.
pub fn piece_at(meta: &Meta, seq: u64) -> Result<Option<Piece>, PlanError> {
    let Some(v) = meta.trunk_version(seq)? else {
        return Ok(None);
    };
    if !v.merged {
        return Ok(Some(Piece {
            level: 0,
            start: seq,
            end: seq,
            pack: v.pack,
            side: v.deps.packs,
        }));
    }
    match meta.cover_containing(seq)? {
        Some(cover) => Ok(Some(cover)),
        None => Err(PlanError::Uncovered(seq)),
    }
}
