//! Fetch planning: turn a client's `have`/`want` commits into a list of stored packs.
//!
//! Nothing here looks at objects. Wants and haves are mapped to ref versions through
//! the metadata store, and the answer is a set of existing packs:
//!
//! - Trunk is a linear chain, so "what the client has" collapses to one number
//!   (`trunk_have`) and "what it wants" to another (`trunk_want`). The range between
//!   them is covered greedily with compaction covers (L4 down to L0), which is the
//!   skip-list walk that keeps a catch-up fetch to O(log Δ) packs.
//! - Any other ref version is one pack plus its trunk dependency.
//!
//! Packs may contain objects the client already has. That is the trade: the same
//! bytes are served to every reader, so they can be cached like a CDN.

use crate::meta::{Meta, Version};
use crate::object::Oid;
use std::collections::{HashSet, VecDeque};

/// Compaction fan-out: a level-L cover spans `FANOUT^L` trunk versions.
pub const FANOUT: u64 = 4;
/// Highest compaction level.
pub const MAX_LEVEL: u32 = 4;
/// How far to walk back from a `have` looking for a known ref version tip.
const HAVE_WALK_LIMIT: usize = 256;

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
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

pub fn plan(meta: &Meta, wants: &[Oid], haves: &[Oid]) -> Result<Plan, PlanError> {
    let mut p = Plan::default();
    let mut had_versions = HashSet::new();
    let mut had_packs = HashSet::new();
    let have_set: HashSet<Oid> = haves.iter().copied().collect();

    for have in haves {
        let (known, tips) = tips_reachable_from(meta, have)?;
        if !known {
            continue;
        }
        p.common.push(*have);
        for v in tips {
            p.cut_found = true;
            if meta.is_trunk(&v.refname) {
                p.trunk_have = p.trunk_have.max(v.seq);
            } else {
                had_versions.insert(v.id);
                if v.pack_complete {
                    had_packs.extend(v.pack);
                }
            }
        }
    }

    // Side packs still to include, and trunk coverage so far.
    let mut work: Vec<Oid> = Vec::new();
    for want in wants {
        if have_set.contains(want) {
            continue;
        }
        let version = match best_version(meta, meta.versions_with_tip(want)?) {
            Some(v) => v,
            None => {
                // Not a tip: an older commit. Serve whichever version stored it.
                let row = meta.commit(want)?.ok_or(PlanError::NotOurRef(*want))?;
                if let Some(seq) = row.trunk_seq {
                    p.trunk_want = p.trunk_want.max(seq);
                    continue;
                }
                let v = meta
                    .version(row.version)?
                    .ok_or(PlanError::NotOurRef(*want))?;
                // Even a trunk version's pack is needed explicitly here: the commit is
                // in it but not reachable from trunk.
                p.trunk_want = p.trunk_want.max(v.deps.trunk);
                work.extend(v.pack);
                work.extend(v.deps.packs);
                continue;
            }
        };
        if meta.is_trunk(&version.refname) {
            p.trunk_want = p.trunk_want.max(version.seq);
        } else if !had_versions.contains(&version.id) {
            p.trunk_want = p.trunk_want.max(version.deps.trunk);
            work.extend(version.pack);
            work.extend(version.deps.packs);
        }
    }

    // Follow dependencies to a fixed point: side packs can raise the trunk range, and
    // newly covered trunk versions can need side packs.
    let mut side = Vec::new();
    let mut seen = HashSet::new();
    let mut covered = p.trunk_have;
    loop {
        while let Some(pack) = work.pop() {
            if had_packs.contains(&pack) || !seen.insert(pack) {
                continue;
            }
            side.push(pack);
            if let Some(deps) = meta.pack_deps(&pack)? {
                p.trunk_want = p.trunk_want.max(deps.trunk);
                work.extend(deps.packs);
            }
        }
        if p.trunk_want <= covered {
            break;
        }
        let (packs, deps) = cover_trunk(meta, covered, p.trunk_want)?;
        p.packs.extend(packs);
        work.extend(deps);
        covered = p.trunk_want;
    }
    for pack in side {
        if !p.packs.contains(&pack) {
            p.packs.push(pack);
        }
    }
    Ok(p)
}

/// Prefer a trunk version (no extra pack), otherwise the newest version.
fn best_version(meta: &Meta, mut vs: Vec<Version>) -> Option<Version> {
    if let Some(i) = vs.iter().position(|v| meta.is_trunk(&v.refname)) {
        return Some(vs.swap_remove(i));
    }
    vs.pop()
}

/// Is `have` a commit we know, and which ref version tips does the client therefore
/// have? Walks parents a bounded distance; reaching a trunk tip stops that path,
/// since trunk tips imply every earlier trunk version.
fn tips_reachable_from(meta: &Meta, have: &Oid) -> Result<(bool, Vec<Version>), PlanError> {
    let mut tips = Vec::new();
    let mut known = false;
    let mut seen = HashSet::from([*have]);
    let mut queue = VecDeque::from([*have]);
    while let Some(oid) = queue.pop_front() {
        let versions = meta.versions_with_tip(&oid)?;
        let hit_trunk = versions.iter().any(|v| meta.is_trunk(&v.refname));
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

/// Packs covering trunk versions `(have, want]`, using the largest aligned cover that
/// fits at each step, plus the non-trunk packs those versions depend on.
pub fn cover_trunk(meta: &Meta, have: u64, want: u64) -> Result<(Vec<Oid>, Vec<Oid>), PlanError> {
    let mut packs = Vec::new();
    let mut deps = Vec::new();
    let mut at = have; // versions 1..=at are covered
    'outer: while at < want {
        for level in (1..=MAX_LEVEL).rev() {
            let s = span(level);
            if at.is_multiple_of(s)
                && at + s <= want
                && let Some(cover) = meta.cover(level, at + 1)?
            {
                packs.extend(cover.pack);
                deps.extend(cover.packs);
                at += s;
                continue 'outer;
            }
        }
        let v = meta
            .trunk_version(at + 1)?
            .ok_or(PlanError::NotOurRef(Oid::ZERO))?;
        packs.extend(v.pack);
        deps.extend(v.deps.packs);
        at += 1;
    }
    Ok((packs, deps))
}
