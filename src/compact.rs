//! Background compaction: trunk storage as an LSM-style partition.
//!
//! Every trunk position is held by exactly one *piece*: its version's own pack (level
//! 0), or one cover. A level-L cover is one pack holding the `FANOUT^L` positions of
//! an aligned range `k * FANOUT^L + 1 ..= (k + 1) * FANOUT^L`, spliced from the pieces
//! inside it. Recording a cover replaces those pieces: the covers below it are
//! deleted, the versions in it are marked merged (their rows stay; they still map
//! haves and wants to positions), and the index rows of trunk objects homed in a
//! replaced pack move to the cover. So trunk history is on disk about once, and a
//! fresh clone is O(log N) packs.
//!
//! Merge rule: the level-L cover of a range is built only once trunk head is at least
//! one more level-L span past the range's end. A client whose position falls inside
//! a level-L cover is then more than `FANOUT^L` versions behind, and gets the whole
//! cover: fewer than twice the versions it lacks. The most recent spans stay in finer
//! pieces, at most `2 * FANOUT - 1` per level. When several levels are due at once
//! (after an upgrade, or a long pause), the highest is built directly from whatever
//! pieces it contains (at most [`MAX_PARTS`]).
//!
//! A cover whose parts total more than the size cap is not built, so a huge repository
//! is not rewritten end to end again and again; its range stays in smaller pieces,
//! which keep merging below the cap.
//!
//! Epochs: covers of the current epoch start inside it. Earlier epochs are only ever
//! served whole, from position 1 up to the highest position something still needs
//! (gc's `old_max`), so that range is merged as far as it goes, without the lag. Covers
//! there may span several old epochs, and never go past `old_max` as found when the
//! merge starts (gc stores it; compaction finds it afresh before merging there), so
//! history a rewrite dropped is not merged into anything new. A cover built before a
//! rewrite (or before a ref holding `old_max` up went away) can still straddle it; gc
//! trims those (see `gc.rs`).
//!
//! Compaction never deletes packs: the replaced ones become unreachable, and gc
//! deletes them after its grace period (fetches in flight and signed pack URLs may
//! still use them), unless something depends on one directly. Index rows of objects
//! that trunk does not reach (pushed in the same pack as trunk, for another ref) stay
//! with their pack, which is their smallest home and what their dependents name. One
//! compaction runs at a time per repository.
//!
//! Steps, with metadata locked only for short reads and writes:
//! 1. Build: map the parts and splice them into a staging file (no lock).
//! 2. Record (lock): check the parts are still the pieces of their ranges (gc may
//!    have trimmed one meanwhile; if so, discard and retry next run), publish the
//!    pack and store the cover as pending. gc keeps pending covers, does not trim
//!    under them, and the planner does not see them.
//! 3. Re-point (lock per chunk): move trunk index rows from the parts to the cover.
//! 4. Finish (lock): one transaction makes the cover the piece for its range. A
//!    crash before this leaves it pending, and the next run finishes it.

use crate::meta::{Meta, Piece};
use crate::object::Oid;
use crate::pack;
use crate::planner::{self, MAX_LEVEL, span};
use crate::store::BlobStore;
use std::collections::HashSet;
use std::sync::Mutex;

/// Covers whose parts total more than this are not built.
pub const MAX_COVER_BYTES: u64 = 4 << 30;
/// Most parts spliced at once (each is memory-mapped while building).
const MAX_PARTS: usize = 256;
/// Index rows moved per lock acquisition.
const REPOINT_CHUNK: usize = 10_000;
/// Pieces looked up per lock acquisition.
const WALK_CHUNK: u64 = 1024;

/// Build every cover that is due, after finishing any a crash left pending. Returns
/// how many were recorded or finished.
pub fn run(meta: &Mutex<Meta>, store: &BlobStore) -> anyhow::Result<usize> {
    run_with_limit(meta, store, MAX_COVER_BYTES)
}

/// [`run`] with a different size cap.
pub fn run_with_limit(
    meta: &Mutex<Meta>,
    store: &BlobStore,
    max_bytes: u64,
) -> anyhow::Result<usize> {
    let pending = meta.lock().unwrap().pending_covers()?;
    let mut written = pending.len();
    for cover in pending {
        let parts = planner::pieces(&meta.lock().unwrap(), cover.start - 1, cover.end)?;
        let members: Vec<Oid> = parts
            .iter()
            .filter(|p| p.level < cover.level && p.start >= cover.start && p.end <= cover.end)
            .filter_map(|p| p.pack)
            .collect();
        finish(meta, &cover, &members)?;
    }

    let (head, epoch) = {
        let m = meta.lock().unwrap();
        (m.trunk_head()?.map_or(0, |v| v.seq), m.epoch_start)
    };
    // The current epoch, lazily: the most recent span at each level stays in finer
    // pieces, for clients a little behind.
    let pieces = walk(meta, epoch - 1, head)?;
    written += merge_due(meta, store, max_bytes, pieces, epoch, |end, s| {
        end.saturating_add(s) <= head
    })?;
    // Earlier epochs are only ever served whole, from position 1 up to the highest
    // one something still needs: merge as far as that allows. What gc last stored
    // says whether anything is due; a ref deleted since may have lowered it, so the
    // merge goes only as far as what is needed now.
    if epoch > 1 {
        let hint = old_needed(meta, false)?;
        let pieces = walk(meta, 0, hint)?;
        let fits = |end: u64, _| end <= hint;
        if (0..pieces.len()).any(|i| due(&pieces, i, 1, fits, &HashSet::new()).is_some()) {
            let old = old_needed(meta, true)?;
            let pieces = walk(meta, 0, old)?;
            written += merge_due(meta, store, max_bytes, pieces, 1, |end, _| end <= old)?;
        }
    }
    Ok(written)
}

/// The highest earlier-epoch position anything still depends on: as gc last found it
/// unless `fresh` (or none is stored for this epoch), otherwise found and stored now.
fn old_needed(meta: &Mutex<Meta>, fresh: bool) -> anyhow::Result<u64> {
    let mut m = meta.lock().unwrap();
    if !fresh && let Some(old) = m.old_needed()? {
        return Ok(old);
    }
    let old = crate::gc::old_needed(&m)?;
    m.set_old_needed(old)?;
    Ok(old)
}

/// Build every due cover over `pieces` (in order, positions from `lo` on), where
/// `fits(end, span)` says whether a cover ending at `end` is due.
fn merge_due(
    meta: &Mutex<Meta>,
    store: &BlobStore,
    max_bytes: u64,
    mut pieces: Vec<Piece>,
    lo: u64,
    fits: impl Fn(u64, u64) -> bool,
) -> anyhow::Result<usize> {
    let mut refused = HashSet::new();
    let mut written = 0;
    let mut i = 0;
    while i < pieces.len() {
        let Some((level, start, range)) = due(&pieces, i, lo, &fits, &refused) else {
            i += 1;
            continue;
        };
        let parts = &pieces[range.clone()];
        let mut packs: Vec<Oid> = Vec::new();
        for pack in parts.iter().filter_map(|p| p.pack) {
            if !packs.contains(&pack) {
                packs.push(pack);
            }
        }
        let bytes = packs
            .iter()
            .map(|p| store.pack_size(p))
            .sum::<std::io::Result<u64>>()?;
        if bytes > max_bytes {
            tracing::info!(level, start, bytes, "cover over the size cap, not built");
            refused.insert((level, start));
            continue; // lower levels for the same piece
        }
        let Some(cover) = merge(meta, store, level, start, parts, &packs, lo)? else {
            // The pieces changed while it was built (gc trimmed one): next run.
            tracing::info!(
                level,
                start,
                "pieces changed while building, cover not recorded"
            );
            break;
        };
        tracing::info!(level, start, end = cover.end, pack = ?cover.pack, "compacted");
        pieces.splice(range.clone(), [cover]);
        written += 1;
        i = range.start; // higher levels may be due now that there are fewer parts
    }
    Ok(written)
}

/// The pieces holding positions `(have, want]`, looked up a chunk at a time.
fn walk(meta: &Mutex<Meta>, have: u64, want: u64) -> anyhow::Result<Vec<Piece>> {
    let mut out: Vec<Piece> = Vec::new();
    let mut at = have;
    while at < want {
        let to = want.min(at + WALK_CHUNK);
        let pieces = planner::pieces(&meta.lock().unwrap(), at, to)?;
        at = pieces.last().map_or(to, |p| p.end.max(to));
        out.extend(pieces);
    }
    Ok(out)
}

/// The highest due cover containing `pieces[i]`: starting at or after `lo`, due by
/// `fits`, not refused, and with few enough parts. Returns its level, start, and the
/// range of `pieces` inside it.
fn due(
    pieces: &[Piece],
    i: usize,
    lo: u64,
    fits: impl Fn(u64, u64) -> bool,
    refused: &HashSet<(u32, u64)>,
) -> Option<(u32, u64, std::ops::Range<usize>)> {
    let piece = &pieces[i];
    for level in (piece.level + 1..=MAX_LEVEL).rev() {
        let s = span(level);
        let start = (piece.start - 1) / s * s + 1;
        let end = start + s - 1;
        if start < lo || !fits(end, s) || refused.contains(&(level, start)) {
            continue;
        }
        let a = pieces.partition_point(|p| p.end < start);
        let b = pieces.partition_point(|p| p.start <= end);
        if b - a <= MAX_PARTS {
            return Some((level, start, a..b));
        }
    }
    None
}

/// Build (or adopt) the cover over `parts`, and record it. `None` if the parts stopped
/// being the pieces of their ranges meanwhile.
fn merge(
    meta: &Mutex<Meta>,
    store: &BlobStore,
    level: u32,
    start: u64,
    parts: &[Piece],
    packs: &[Oid],
    lo: u64,
) -> anyhow::Result<Option<Piece>> {
    let end = start + span(level) - 1;
    // Parts are inside the cover; anything else stays a dependency.
    let mut side: Vec<Oid> = parts
        .iter()
        .flat_map(|p| p.side.iter().copied())
        .filter(|p| !packs.contains(p))
        .collect();
    side.sort();
    side.dedup();
    let (trunk_dep, existing) = {
        let m = meta.lock().unwrap();
        // What the cover needs from trunk before it: everything, within the range
        // being merged; at its start, only what the versions needed from before it.
        let trunk_dep = if start > lo {
            start - 1
        } else {
            m.trunk_dep_below(start, end, start)?
        };
        (trunk_dep, m.cover_row(level, start)?)
    };
    let mut cover = Piece {
        level,
        start,
        end,
        pack: None,
        side,
    };
    // A cover row written before this design (which kept every level) holds exactly
    // these positions: adopt its pack rather than build it again.
    let mut recorded = false;
    if let Some((pack, false)) = existing {
        cover.pack = pack;
        recorded = record(meta, store, &cover, parts, trunk_dep, None)?;
    }
    if !recorded {
        let (pack, staged) = build(store, packs)?;
        cover.pack = pack;
        if !record(meta, store, &cover, parts, trunk_dep, staged)? {
            return Ok(None);
        }
    }
    finish(meta, &cover, packs)?;
    Ok(Some(cover))
}

/// A spliced pack waiting in staging, with its index.
type Staged = (std::path::PathBuf, Vec<pack::IdxEntry>);

/// Splice packs into a staging file. Returns the cover's pack: none without parts,
/// the part itself when there is one, otherwise the staged splice.
fn build(store: &BlobStore, packs: &[Oid]) -> anyhow::Result<(Option<Oid>, Option<Staged>)> {
    match packs {
        [] => Ok((None, None)),
        [one] => Ok((Some(*one), None)),
        _ => {
            let loaded: Vec<(memmap2::Mmap, Vec<pack::IdxEntry>)> = packs
                .iter()
                .map(|h| {
                    let map = store.map_pack(h)?;
                    // Read once, front to back: the kernel may drop pages behind us.
                    map.advise(memmap2::Advice::Sequential)?;
                    Ok((map, store.read_idx(h)?))
                })
                .collect::<std::io::Result<_>>()?;
            let parts: Vec<(&[u8], &[pack::IdxEntry])> =
                loaded.iter().map(|(b, i)| (&b[..], i.as_slice())).collect();
            let (path, (hash, idx)) = store.stage(|out| {
                // Parts of one repository's trunk share its format.
                let (hash, idx, _) = pack::concat(packs[0].format(), &parts, out)?;
                Ok((hash, idx))
            })?;
            Ok((Some(hash), Some((path, idx))))
        }
    }
}

/// Publish a staged pack and store the cover as pending. Returns false, recording
/// nothing and discarding `staged`, if `parts` are no longer the pieces of their
/// ranges (gc trims covers concurrently, see `gc.rs`), or if without `staged` the
/// cover's pack is no longer stored.
fn record(
    meta: &Mutex<Meta>,
    store: &BlobStore,
    cover: &Piece,
    parts: &[Piece],
    trunk_dep: u64,
    staged: Option<Staged>,
) -> anyhow::Result<bool> {
    // Under the lock, like a push, so gc cannot delete a pack with the same hash
    // between its publication and its row.
    let mut m = meta.lock().unwrap();
    let mut current = true;
    for part in parts {
        let now = planner::piece_at(&m, part.start)?;
        current &= now.is_some_and(|p| (p.level, p.pack) == (part.level, part.pack));
    }
    if !current {
        if let Some((path, _)) = staged {
            let _ = std::fs::remove_file(path);
        }
        return Ok(false);
    }
    match (staged, cover.pack) {
        (Some((path, idx)), Some(hash)) => store.publish(&path, &hash, &idx)?,
        (_, Some(hash)) if !store.has(&hash) => return Ok(false),
        _ => {}
    }
    m.record_cover(cover, trunk_dep)?;
    Ok(true)
}

/// Move trunk index rows from `members` to the recorded cover, then make it the piece
/// for its range.
fn finish(meta: &Mutex<Meta>, cover: &Piece, members: &[Oid]) -> anyhow::Result<()> {
    if let Some(pack) = cover.pack {
        for member in members.iter().filter(|m| **m != pack) {
            while meta.lock().unwrap().repoint(member, &pack, REPOINT_CHUNK)? == REPOINT_CHUNK {}
        }
    }
    meta.lock().unwrap().finish_cover(cover, members)?;
    Ok(())
}
