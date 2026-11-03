//! Packfile decoding, validation, and writing.
//!
//! Pando never unpacks a pushed pack into a repository. It decodes the pack once to
//! learn every object id (resolving in-pack deltas), checks the trailer, and keeps a
//! small sidecar index of `(offset, oid)` so the pack can later be served as-is or
//! spliced into a larger pack without inflating it again.

use crate::object::{Kind, Oid, commit_parents, tag_target};
use flate2::{Decompress, FlushDecompress, Status};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;

/// Largest single object Pando will inflate.
pub const MAX_OBJECT_SIZE: usize = 512 << 20;
/// Inflated bytes (objects plus delta results) allowed per byte of pack, with a floor
/// of `MIN_INFLATED_BUDGET`. Decoding streams, so this bounds CPU spent on zlib bombs
/// and chains of deltas that each expand a few bytes into 64 KiB, not memory. Real
/// histories reconstruct to tens of times their packed size.
pub const INFLATE_RATIO: usize = 256;
const MIN_INFLATED_BUDGET: usize = 4 << 30;
/// Never reserve more than this up front; vectors grow as real data arrives.
const MAX_PREALLOC: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("malformed pack: {0}")]
    Malformed(String),
    #[error("pack checksum mismatch")]
    Checksum,
    #[error("thin packs are not accepted ({0} deltas reference objects outside the pack)")]
    Thin(usize),
    #[error("pack exceeds size limits")]
    TooLarge,
}

fn bad(msg: impl Into<String>) -> PackError {
    PackError::Malformed(msg.into())
}

/// One object's location in a pack, in pack order. This is the sidecar index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdxEntry {
    pub offset: u64,
    pub oid: Oid,
}

/// What a single decode pass learns about a pack.
pub struct ParsedPack {
    /// The trailer checksum. Git's index-pack reports this as the pack name, and the
    /// packfile-uris protocol line carries it, so it doubles as our blob key.
    pub hash: Oid,
    pub entries: Vec<IdxEntry>,
    /// Every object id in the pack.
    pub oids: HashSet<Oid>,
    /// Every commit in the pack with its parents.
    pub commits: HashMap<Oid, Vec<Oid>>,
    /// Every annotated tag in the pack with the object it points at.
    pub tags: HashMap<Oid, Oid>,
    /// Outgoing references of every commit (tree + parents), tree (entries, minus
    /// submodule links) and tag (target). Blobs have none.
    pub refs: HashMap<Oid, Vec<Oid>>,
}

impl ParsedPack {
    pub fn contains(&self, oid: &Oid) -> bool {
        self.oids.contains(oid)
    }

    /// Follow a chain of annotated tags in this pack to the first non-tag object.
    /// `None` if `oid` is not a tag here.
    pub fn peel(&self, oid: &Oid) -> Option<Oid> {
        let mut target = *self.tags.get(oid)?;
        for _ in 0..64 {
            match self.tags.get(&target) {
                Some(next) => target = *next,
                None => break,
            }
        }
        Some(target)
    }

    /// Pushed commits reachable from `tip` (peeling tags), with their parents.
    pub fn reachable_commits(&self, tip: &Oid) -> Vec<(Oid, Vec<Oid>)> {
        let start = self.peel(tip).unwrap_or(*tip);
        let mut seen = HashSet::from([start]);
        let mut stack = vec![start];
        let mut out = Vec::new();
        while let Some(oid) = stack.pop() {
            let Some(parents) = self.commits.get(&oid) else {
                continue;
            };
            out.push((oid, parents.clone()));
            stack.extend(parents.iter().filter(|p| seen.insert(**p)));
        }
        out
    }

    /// Objects in this pack reachable from `tips` without leaving the pack.
    pub fn reachable_objects(&self, tips: impl IntoIterator<Item = Oid>) -> HashSet<Oid> {
        let mut seen = HashSet::new();
        let mut stack: Vec<Oid> = tips.into_iter().filter(|t| self.contains(t)).collect();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            for r in self.refs.get(&oid).into_iter().flatten() {
                if self.contains(r) && !seen.contains(r) {
                    stack.push(*r);
                }
            }
        }
        seen
    }

    /// Objects referenced from inside the pack but not contained in it. Together with
    /// the pack itself, these must already exist for the pack to be complete.
    pub fn external_refs(&self) -> HashSet<Oid> {
        self.refs
            .values()
            .flatten()
            .filter(|r| !self.contains(r))
            .copied()
            .collect()
    }
}

/// Outgoing references of an object, parsed from its contents.
fn object_refs(kind: Kind, data: &[u8]) -> Result<Vec<Oid>, PackError> {
    Ok(match kind {
        Kind::Commit => {
            let tree =
                crate::object::commit_tree(data).ok_or_else(|| bad("commit without tree"))?;
            let mut refs = vec![tree];
            refs.extend(commit_parents(data));
            refs
        }
        Kind::Tag => vec![tag_target(data).ok_or_else(|| bad("tag without object line"))?],
        Kind::Tree => crate::object::tree_entries(data)
            .ok_or_else(|| bad("malformed tree"))?
            .into_iter()
            .filter(|(mode, _)| *mode != crate::object::GITLINK_MODE)
            .map(|(_, oid)| oid)
            .collect(),
        Kind::Blob => Vec::new(),
    })
}

/// A parsed object header: type code, inflated size, header length, delta base.
pub(crate) struct Header {
    pub code: u8,
    pub size: usize,
    pub len: usize,
    pub base: Base,
}

pub(crate) enum Base {
    None,
    Offset(u64),
    Oid(Oid),
}

pub(crate) fn read_header(buf: &[u8], offset: usize) -> Result<Header, PackError> {
    let mut pos = offset;
    let byte = |pos: usize| {
        buf.get(pos)
            .copied()
            .ok_or_else(|| bad("truncated object header"))
    };
    let mut c = byte(pos)?;
    pos += 1;
    let code = (c >> 4) & 7;
    let mut size = (c & 15) as usize;
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = byte(pos)?;
        pos += 1;
        size |= ((c & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or_else(|| bad("size overflow"))?;
        shift += 7;
    }
    let base = match code {
        OFS_DELTA => {
            let (dist, used) = decode_ofs(&buf[pos..]).ok_or_else(|| bad("truncated ofs-delta"))?;
            pos += used;
            let base = (offset as u64).checked_sub(dist).filter(|_| dist > 0);
            Base::Offset(base.ok_or_else(|| bad("delta base offset out of range"))?)
        }
        REF_DELTA => {
            let oid = buf.get(pos..pos + 20).and_then(Oid::from_slice);
            pos += 20;
            Base::Oid(oid.ok_or_else(|| bad("truncated ref-delta base"))?)
        }
        1..=4 => Base::None,
        _ => return Err(bad(format!("unknown object type {code}"))),
    };
    Ok(Header {
        code,
        size,
        len: pos - offset,
        base,
    })
}

/// Inflate one zlib stream that must produce exactly `size` bytes. Returns the data and
/// how many compressed bytes the stream used.
pub fn inflate(input: &[u8], size: usize) -> Result<(Vec<u8>, usize), PackError> {
    let mut out = Vec::with_capacity(size.min(MAX_PREALLOC) + 1);
    let used = inflate_with(input, size, |chunk| out.extend_from_slice(chunk))?;
    Ok((out, used))
}

/// Inflate one zlib stream of exactly `size` bytes through a fixed buffer, handing each
/// chunk to `sink`. Returns how many compressed bytes the stream used.
fn inflate_with(
    input: &[u8],
    size: usize,
    mut sink: impl FnMut(&[u8]),
) -> Result<usize, PackError> {
    if size > MAX_OBJECT_SIZE {
        return Err(PackError::TooLarge);
    }
    let mut z = Decompress::new(true);
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let (in0, out0) = (z.total_in() as usize, z.total_out());
        let status = z
            .decompress(&input[in0..], &mut buf, FlushDecompress::None)
            .map_err(|e| bad(format!("zlib: {e}")))?;
        let produced = (z.total_out() - out0) as usize;
        if z.total_out() as usize > size {
            return Err(bad("object larger than its header"));
        }
        sink(&buf[..produced]);
        match status {
            Status::StreamEnd => break,
            _ if produced == 0 && z.total_in() as usize == in0 => {
                return Err(bad("truncated zlib stream"));
            }
            _ => {}
        }
    }
    if z.total_out() as usize != size {
        return Err(bad("object size does not match header"));
    }
    Ok(z.total_in() as usize)
}

/// Decode a whole pack: verify the trailer, resolve deltas inside the pack, compute
/// every object id, and record each object's outgoing references. Deltas against
/// objects outside the pack are rejected, because a stored pack must be indexable on
/// its own when a client downloads it.
///
/// Memory stays bounded by delta chains, not pack size: the first pass streams every
/// object through a fixed buffer (hashing whole objects as it goes), and the second
/// re-inflates only delta bases, depth first, dropping each once its chain is done.
pub fn parse(buf: &[u8]) -> Result<ParsedPack, PackError> {
    if buf.len() < 32 || &buf[..4] != b"PACK" {
        return Err(bad("missing PACK signature"));
    }
    let version = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        return Err(bad(format!("unsupported pack version {version}")));
    }
    let count = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
    let body_end = buf.len() - 20;
    if Sha1::digest(&buf[..body_end]).as_slice() != &buf[body_end..] {
        return Err(PackError::Checksum);
    }
    let hash = Oid::from_slice(&buf[body_end..]).unwrap();
    let body = &buf[..body_end];

    struct Raw {
        offset: u64,
        data_at: usize,
        size: usize,
        base: Base,
    }
    let mut raws: Vec<Raw> = Vec::with_capacity(count.min(MAX_PREALLOC / 64));
    let mut resolved: Vec<Option<(Kind, Oid)>> = Vec::with_capacity(raws.capacity());
    let mut refs = HashMap::new();
    let mut budget = MIN_INFLATED_BUDGET.max(buf.len().saturating_mul(INFLATE_RATIO));

    // Pass 1: find object boundaries; hash whole objects while streaming them.
    let mut pos = 12;
    for _ in 0..count {
        if pos >= body_end {
            return Err(bad("pack ends before its object count"));
        }
        let h = read_header(body, pos)?;
        budget = budget.checked_sub(h.size).ok_or(PackError::TooLarge)?;
        let data_at = pos + h.len;
        let kind = Kind::from_code(h.code);
        let used = match (kind, &h.base) {
            (Some(kind), Base::None) => {
                let mut sha = object_hasher(kind, h.size);
                // Keep the bytes only for objects whose references we need.
                let mut keep = (kind != Kind::Blob).then(Vec::new);
                let used = inflate_with(&body[data_at..], h.size, |c| {
                    sha.update(c);
                    if let Some(k) = keep.as_mut() {
                        k.extend_from_slice(c);
                    }
                })?;
                let oid = Oid(sha.finalize().into());
                if let Some(data) = keep {
                    refs.insert(oid, object_refs(kind, &data)?);
                }
                resolved.push(Some((kind, oid)));
                used
            }
            _ => {
                resolved.push(None);
                inflate_with(&body[data_at..], h.size, |_| {})?
            }
        };
        raws.push(Raw {
            offset: pos as u64,
            data_at,
            size: h.size,
            base: h.base,
        });
        pos = data_at + used;
    }
    if pos != body_end {
        return Err(bad("trailing bytes after last object"));
    }

    // Pass 2: resolve deltas depth first from each base.
    let by_offset: HashMap<u64, usize> = raws
        .iter()
        .enumerate()
        .map(|(i, r)| (r.offset, i))
        .collect();
    let mut children_by_index: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut children_by_oid: HashMap<Oid, Vec<usize>> = HashMap::new();
    for (i, r) in raws.iter().enumerate() {
        match r.base {
            Base::None => {}
            Base::Offset(off) => {
                let base = *by_offset
                    .get(&off)
                    .ok_or_else(|| bad("delta base offset is not an object"))?;
                children_by_index.entry(base).or_default().push(i);
            }
            Base::Oid(oid) => children_by_oid.entry(oid).or_default().push(i),
        }
    }
    let mut take_children = |i: usize, oid: &Oid| -> Vec<usize> {
        let mut kids = children_by_index.remove(&i).unwrap_or_default();
        kids.extend(children_by_oid.remove(oid).unwrap_or_default());
        kids
    };
    let roots: Vec<usize> = (0..raws.len()).filter(|&i| resolved[i].is_some()).collect();
    // Each pending delta carries its base's bytes and kind; a delta has its base's kind.
    let mut stack: Vec<(usize, Rc<Vec<u8>>, Kind)> = Vec::new();
    for root in roots {
        let (kind, oid) = resolved[root].unwrap();
        let kids = take_children(root, &oid);
        if kids.is_empty() {
            continue;
        }
        let r = &raws[root];
        let base = Rc::new(inflate(&body[r.data_at..], r.size)?.0);
        stack.extend(kids.into_iter().map(|k| (k, base.clone(), kind)));
        while let Some((k, base, kind)) = stack.pop() {
            if resolved[k].is_some() {
                continue;
            }
            let r = &raws[k];
            let delta = inflate(&body[r.data_at..], r.size)?.0;
            let data = apply_delta(&base, &delta)?;
            drop(base);
            budget = budget.checked_sub(data.len()).ok_or(PackError::TooLarge)?;
            let oid = Oid::for_object(kind, &data);
            if kind != Kind::Blob {
                refs.insert(oid, object_refs(kind, &data)?);
            }
            resolved[k] = Some((kind, oid));
            let kids = take_children(k, &oid);
            if !kids.is_empty() {
                let data = Rc::new(data);
                stack.extend(kids.into_iter().map(|c| (c, data.clone(), kind)));
            }
        }
    }
    let unresolved = resolved.iter().filter(|r| r.is_none()).count();
    if unresolved > 0 {
        return Err(PackError::Thin(unresolved));
    }

    let mut entries = Vec::with_capacity(raws.len());
    let mut oids = HashSet::with_capacity(raws.len());
    let mut commits = HashMap::new();
    let mut tags = HashMap::new();
    for (r, res) in raws.iter().zip(&resolved) {
        let (kind, oid) = res.unwrap();
        entries.push(IdxEntry {
            offset: r.offset,
            oid,
        });
        oids.insert(oid);
        match kind {
            // Commit refs are [tree, parents...]; tag refs are [target].
            Kind::Commit => {
                commits.insert(oid, refs[&oid][1..].to_vec());
            }
            Kind::Tag => {
                tags.insert(oid, refs[&oid][0]);
            }
            _ => {}
        }
    }
    Ok(ParsedPack {
        hash,
        entries,
        oids,
        commits,
        tags,
        refs,
    })
}

/// A SHA-1 primed with the loose-object header, ready for the object's bytes.
fn object_hasher(kind: Kind, size: usize) -> Sha1 {
    let mut sha = Sha1::new();
    sha.update(format!("{} {size}\0", kind.name()).as_bytes());
    sha
}

/// Apply a Git delta (copy/insert instruction stream) to `base`.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut pos = 0;
    let varint = |pos: &mut usize| -> Result<usize, PackError> {
        let (mut v, mut shift) = (0usize, 0);
        loop {
            let c = *delta
                .get(*pos)
                .ok_or_else(|| bad("truncated delta header"))?;
            *pos += 1;
            v |= ((c & 0x7f) as usize)
                .checked_shl(shift)
                .ok_or_else(|| bad("delta size overflow"))?;
            shift += 7;
            if c & 0x80 == 0 {
                return Ok(v);
            }
        }
    };
    let src_len = varint(&mut pos)?;
    let dst_len = varint(&mut pos)?;
    if src_len != base.len() {
        return Err(bad("delta base size mismatch"));
    }
    if dst_len > MAX_OBJECT_SIZE {
        return Err(PackError::TooLarge);
    }
    let mut out = Vec::with_capacity(dst_len.min(MAX_PREALLOC));
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd & 0x80 != 0 {
            let mut field = |bits: u8, count: usize| -> Result<usize, PackError> {
                let mut v = 0usize;
                for i in 0..count {
                    if bits & (1 << i) != 0 {
                        let b = *delta.get(pos).ok_or_else(|| bad("truncated copy"))?;
                        pos += 1;
                        v |= (b as usize) << (8 * i);
                    }
                }
                Ok(v)
            };
            let off = field(cmd & 0x0f, 4)?;
            let len = match field((cmd >> 4) & 0x07, 3)? {
                0 => 0x10000,
                n => n,
            };
            let src = base
                .get(off..off + len)
                .ok_or_else(|| bad("delta copy out of range"))?;
            if out.len() + len > dst_len {
                return Err(bad("delta result larger than declared"));
            }
            out.extend_from_slice(src);
        } else if cmd != 0 {
            let lit = delta
                .get(pos..pos + cmd as usize)
                .ok_or_else(|| bad("truncated insert"))?;
            out.extend_from_slice(lit);
            pos += cmd as usize;
        } else {
            return Err(bad("delta opcode 0"));
        }
    }
    if out.len() != dst_len {
        return Err(bad("delta result size mismatch"));
    }
    Ok(out)
}

/// Serialize a sidecar index: entries in pack order, 28 bytes each.
pub fn encode_idx(entries: &[IdxEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + entries.len() * 28);
    out.extend_from_slice(b"PIDX");
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&e.offset.to_be_bytes());
        out.extend_from_slice(&e.oid.0);
    }
    out
}

pub fn decode_idx(buf: &[u8]) -> io::Result<Vec<IdxEntry>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "bad pack sidecar index");
    if buf.len() < 8 || &buf[..4] != b"PIDX" {
        return Err(invalid());
    }
    let n = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
    let body = &buf[8..];
    if body.len() != n * 28 {
        return Err(invalid());
    }
    Ok(body
        .as_chunks::<28>()
        .0
        .iter()
        .map(|c| IdxEntry {
            offset: u64::from_be_bytes(c[..8].try_into().unwrap()),
            oid: Oid::from_slice(&c[8..]).unwrap(),
        })
        .collect())
}

/// A writer that hashes everything passing through, so a pack's trailer can be
/// appended without buffering the pack.
pub struct HashingWriter<W: Write> {
    inner: W,
    sha: Sha1,
    pub written: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            sha: Sha1::new(),
            written: 0,
        }
    }

    /// Write the trailer and return the pack hash plus the inner writer.
    pub fn finish(mut self) -> io::Result<(Oid, W)> {
        let hash = Oid(self.sha.finalize_reset().into());
        self.inner.write_all(&hash.0)?;
        Ok((hash, self.inner))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn encode_type_size(code: u8, mut size: u64, out: &mut Vec<u8>) {
    let mut c = (code << 4) | (size & 15) as u8;
    size >>= 4;
    while size != 0 {
        out.push(c | 0x80);
        c = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(c);
}

/// Decode an ofs-delta distance. Returns the distance and bytes used.
fn decode_ofs(buf: &[u8]) -> Option<(u64, usize)> {
    let mut c = *buf.first()?;
    let mut used = 1;
    let mut dist = (c & 0x7f) as u64;
    while c & 0x80 != 0 {
        c = *buf.get(used)?;
        used += 1;
        dist = (dist.checked_add(1)? << 7) | (c & 0x7f) as u64;
    }
    Some((dist, used))
}

fn encode_ofs(mut dist: u64, out: &mut Vec<u8>) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len() - 1;
    tmp[i] = (dist & 0x7f) as u8;
    dist >>= 7;
    while dist != 0 {
        dist -= 1;
        i -= 1;
        tmp[i] = 0x80 | (dist & 0x7f) as u8;
        dist >>= 7;
    }
    out.extend_from_slice(&tmp[i..]);
}

/// The smallest valid pack: header with zero objects plus trailer.
pub fn empty_pack() -> Vec<u8> {
    let mut w = HashingWriter::new(Vec::new());
    w.write_all(b"PACK\0\0\0\x02\0\0\0\0").unwrap();
    w.finish().unwrap().1
}

/// Splice several stored packs into one, without inflating any object.
///
/// Objects already emitted by an earlier pack are dropped. Offset deltas are re-encoded
/// against the base's new position (which is always earlier, since input order is
/// preserved); ref deltas and whole objects are copied byte for byte. Used for
/// compaction covers and for inline fetch responses to clients without packfile-uris.
pub fn concat<W: Write>(
    packs: &[(&[u8], &[IdxEntry])],
    out: W,
) -> io::Result<(Oid, Vec<IdxEntry>, W)> {
    let mut seen = HashSet::new();
    let keep: Vec<Vec<bool>> = packs
        .iter()
        .map(|(_, idx)| idx.iter().map(|e| seen.insert(e.oid)).collect())
        .collect();
    let total: usize = keep.iter().flatten().filter(|k| **k).count();

    let mut w = HashingWriter::new(out);
    w.write_all(b"PACK\0\0\0\x02")?;
    w.write_all(&(total as u32).to_be_bytes())?;
    let mut new_offset: HashMap<Oid, u64> = HashMap::new();
    let mut entries = Vec::with_capacity(total);
    let mut head = Vec::with_capacity(32);
    for ((buf, idx), keep) in packs.iter().zip(&keep) {
        let oid_at: HashMap<u64, Oid> = idx.iter().map(|e| (e.offset, e.oid)).collect();
        let body_end = buf.len() as u64 - 20;
        for (i, e) in idx.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            let end = idx.get(i + 1).map_or(body_end, |n| n.offset);
            let h = read_header(buf, e.offset as usize).map_err(io::Error::other)?;
            let data = &buf[e.offset as usize + h.len..end as usize];
            let here = w.written; // offset within the new pack (header included)
            head.clear();
            match h.base {
                Base::Offset(base_off) => {
                    let base_oid = oid_at
                        .get(&base_off)
                        .ok_or_else(|| io::Error::other("dangling ofs-delta"))?;
                    let base_new = new_offset[base_oid];
                    encode_type_size(OFS_DELTA, h.size as u64, &mut head);
                    encode_ofs(here - base_new, &mut head);
                }
                _ => head.extend_from_slice(&buf[e.offset as usize..e.offset as usize + h.len]),
            }
            w.write_all(&head)?;
            w.write_all(data)?;
            new_offset.insert(e.oid, here);
            entries.push(IdxEntry {
                offset: here,
                oid: e.oid,
            });
        }
    }
    let (hash, inner) = w.finish()?;
    Ok((hash, entries, inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pack_parses() {
        let p = parse(&empty_pack()).unwrap();
        assert!(p.entries.is_empty());
    }

    /// A pack around `objects` with a correct header count and trailer.
    fn wrap(count: u32, objects: &[u8]) -> Vec<u8> {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"PACK\0\0\0\x02").unwrap();
        w.write_all(&count.to_be_bytes()).unwrap();
        w.write_all(objects).unwrap();
        w.finish().unwrap().1
    }

    #[test]
    fn hostile_sizes_are_rejected_without_allocating() {
        // A blob claiming to be 2^50 bytes.
        let mut obj = Vec::new();
        encode_type_size(3, 1 << 50, &mut obj);
        obj.extend_from_slice(&[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert!(matches!(parse(&wrap(1, &obj)), Err(PackError::TooLarge)));

        // A delta declaring a 2^50-byte result.
        let delta = [0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(matches!(apply_delta(&[], &delta), Err(PackError::TooLarge)));

        // A ref-delta header that runs into the trailer.
        let mut obj = Vec::new();
        encode_type_size(REF_DELTA, 5, &mut obj);
        obj.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert!(parse(&wrap(1, &obj)).is_err());
    }

    #[test]
    fn ofs_roundtrip() {
        for dist in [1u64, 127, 128, 16511, 16512, 1 << 40] {
            let mut enc = Vec::new();
            encode_ofs(dist, &mut enc);
            assert_eq!(decode_ofs(&enc), Some((dist, enc.len())));
        }
    }
}
