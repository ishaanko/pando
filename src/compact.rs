//! Background compaction, in the spirit of an LSM tree.
//!
//! Every trunk push leaves an L0 pack. Once `FANOUT` consecutive, aligned L0 packs
//! exist, they are spliced into one L1 cover; `FANOUT` L1 covers make an L2, and so on
//! up to `MAX_LEVEL`. Lower levels are never deleted: every level stays servable, and
//! the planner picks whichever mix covers a client's range in the fewest packs.

use crate::meta::Meta;
use crate::object::Oid;
use crate::pack;
use crate::planner::{FANOUT, MAX_LEVEL, span};
use crate::store::BlobStore;
use std::sync::Mutex;

/// Build every cover that can be built right now. Returns how many were written.
/// Metadata is locked only for short reads and writes; splicing happens outside.
pub fn run(meta: &Mutex<Meta>, store: &BlobStore) -> anyhow::Result<usize> {
    let head = meta.lock().unwrap().trunk_head()?.map_or(0, |v| v.seq);
    let mut written = 0;
    for level in 1..=MAX_LEVEL {
        let s = span(level);
        let sub = span(level - 1);
        let mut start = meta.lock().unwrap().last_cover_start(level)?.map_or(1, |last| last + s);
        while start + s - 1 <= head {
            let mut parts = Vec::new();
            {
                let m = meta.lock().unwrap();
                for i in 0..FANOUT {
                    let at = start + i * sub;
                    let part = if level == 1 {
                        m.trunk_version(at)?.map(|v| v.pack)
                    } else {
                        m.cover(level - 1, at)?
                    };
                    match part {
                        Some(p) => parts.extend(p),
                        None => return Ok(written), // lower level not built yet
                    }
                }
            }
            let pack = splice(store, &parts)?;
            meta.lock().unwrap().put_cover(level, start, pack)?;
            tracing::info!(level, start, end = start + s - 1, ?pack, "compacted");
            written += 1;
            start += s;
        }
    }
    Ok(written)
}

/// Splice packs into one stored pack. A single part is reused as-is.
fn splice(store: &BlobStore, parts: &[Oid]) -> anyhow::Result<Option<Oid>> {
    match parts {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        _ => {
            let loaded: Vec<(Vec<u8>, Vec<pack::IdxEntry>)> = parts
                .iter()
                .map(|h| Ok((store.read_pack(h)?, store.read_idx(h)?)))
                .collect::<std::io::Result<_>>()?;
            let refs: Vec<(&[u8], &[pack::IdxEntry])> =
                loaded.iter().map(|(b, i)| (b.as_slice(), i.as_slice())).collect();
            let (hash, idx, bytes) = pack::concat(&refs, Vec::new())?;
            store.put(&hash, &bytes, &idx)?;
            Ok(Some(hash))
        }
    }
}
