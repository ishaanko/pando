//! Packfile decoding, validation, and writing.
//!
//! Pando never unpacks a pushed pack into a repository. It decodes the pack once to
//! learn every object id (resolving in-pack deltas), checks the trailer, and keeps a
//! small sidecar index of `(offset, oid)` so the pack can later be served as-is or
//! spliced into a larger pack without inflating it again.
//!
//! The repository's object format sets the width of everything id-shaped in a pack:
//! the trailer (SHA-1 or SHA-256 of the bytes before it), ref-delta bases and tree
//! entries, and the ids in the sidecar.

use crate::object::{Format, Kind, Oid, commit_parents, object_hasher, tag_target};
use flate2::{Decompress, FlushDecompress, Status};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
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
    #[error(transparent)]
    Io(#[from] io::Error),
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
    /// Every commit in the pack with its parents.
    pub commits: HashMap<Oid, Vec<Oid>>,
    /// Every annotated tag in the pack with the object it points at.
    pub tags: HashMap<Oid, Oid>,
    graph: Graph,
}

/// Outgoing references of every commit (tree + parents), tree (entries, minus
/// submodule links) and tag (target), with ids interned as `u32` and all edges in one
/// array. Real histories have tens of millions of tree entries; at 4 bytes each with
/// no per-object allocation, a large import stays in hundreds of megabytes.
struct Graph {
    ids: Vec<Oid>,
    index: Index,
    in_pack: Vec<bool>,
    /// `edges[start..start + len]` for each node.
    spans: Vec<(u32, u32)>,
    edges: Vec<u32>,
}

/// Positions in `Graph::ids`, keyed by raw ids of the pack's format. Plain arrays
/// keep the map as small and fast as before SHA-256 support (an `Oid` key is 33
/// bytes and slowed a large import's parse by a tenth).
enum Index {
    Sha1(HashMap<[u8; 20], u32>),
    Sha256(HashMap<[u8; 32], u32>),
}

impl Graph {
    fn new(format: Format) -> Graph {
        Graph {
            ids: Vec::new(),
            index: match format {
                Format::Sha1 => Index::Sha1(HashMap::new()),
                Format::Sha256 => Index::Sha256(HashMap::new()),
            },
            in_pack: Vec::new(),
            spans: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// The position of `oid`, added if new. It must be in the pack's format (see
    /// `object_refs`).
    fn intern(&mut self, oid: Oid) -> u32 {
        let next = self.ids.len() as u32;
        let i = match &mut self.index {
            Index::Sha1(m) => intern_in(m, &oid, next),
            Index::Sha256(m) => intern_in(m, &oid, next),
        };
        if i == next {
            self.ids.push(oid);
            self.in_pack.push(false);
            self.spans.push((0, 0));
        }
        i
    }

    fn get(&self, oid: &Oid) -> Option<u32> {
        match (&self.index, oid) {
            (Index::Sha1(m), Oid::Sha1(k)) => m.get(k).copied(),
            (Index::Sha256(m), Oid::Sha256(k)) => m.get(k).copied(),
            _ => None,
        }
    }

    /// Record the outgoing references of interned object `i`.
    fn set_refs(&mut self, i: u32, refs: &[Oid]) {
        let start = self.edges.len() as u32;
        for r in refs {
            let r = self.intern(*r);
            self.edges.push(r);
        }
        self.spans[i as usize] = (start, refs.len() as u32);
    }

    fn refs(&self, i: u32) -> &[u32] {
        let (start, len) = self.spans[i as usize];
        &self.edges[start as usize..(start + len) as usize]
    }
}

/// `oid`'s position in `m`, or `next` once recorded as its position. A function of
/// its own so the hot lookup works on a plain array key.
fn intern_in<const N: usize>(m: &mut HashMap<[u8; N], u32>, oid: &Oid, next: u32) -> u32 {
    let k: [u8; N] = oid.as_bytes().try_into().expect("id in the pack's format");
    if let Some(&i) = m.get(&k) {
        return i;
    }
    m.insert(k, next);
    next
}

impl ParsedPack {
    pub fn contains(&self, oid: &Oid) -> bool {
        self.graph
            .get(oid)
            .is_some_and(|i| self.graph.in_pack[i as usize])
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
    pub fn reachable_objects(&self, tips: impl IntoIterator<Item = Oid>) -> Vec<Oid> {
        let g = &self.graph;
        let mut seen = vec![false; g.ids.len()];
        let mut stack: Vec<u32> = tips.into_iter().filter_map(|t| g.get(&t)).collect();
        let mut out = Vec::new();
        while let Some(i) = stack.pop() {
            if seen[i as usize] || !g.in_pack[i as usize] {
                continue;
            }
            seen[i as usize] = true;
            out.push(g.ids[i as usize]);
            stack.extend(g.refs(i).iter().filter(|&&r| !seen[r as usize]));
        }
        out
    }

    /// Objects referenced from inside the pack but not contained in it. Together with
    /// the pack itself, these must already exist for the pack to be complete.
    pub fn external_refs(&self) -> Vec<Oid> {
        let g = &self.graph;
        let mut referenced = vec![false; g.ids.len()];
        for &r in &g.edges {
            referenced[r as usize] = true;
        }
        (0..g.ids.len())
            .filter(|&i| referenced[i] && !g.in_pack[i])
            .map(|i| g.ids[i])
            .collect()
    }
}

/// Outgoing references of an object, parsed from its contents. Every one must be an
/// id in the repository's `format`.
fn object_refs(format: Format, kind: Kind, data: &[u8]) -> Result<Vec<Oid>, PackError> {
    let refs = match kind {
        Kind::Commit => {
            let tree =
                crate::object::commit_tree(data).ok_or_else(|| bad("commit without tree"))?;
            let mut refs = vec![tree];
            refs.extend(commit_parents(data));
            refs
        }
        Kind::Tag => vec![tag_target(data).ok_or_else(|| bad("tag without object line"))?],
        Kind::Tree => crate::object::tree_entries(format, data)
            .ok_or_else(|| bad("malformed tree"))?
            .into_iter()
            .filter(|(mode, _)| *mode != crate::object::GITLINK_MODE)
            .map(|(_, oid)| oid)
            .collect(),
        Kind::Blob => Vec::new(),
    };
    match refs.iter().find(|r| r.format() != format) {
        Some(r) => Err(bad(format!(
            "{} {} names {r}, not a {} id",
            kind.name(),
            Oid::for_object(format, kind, data),
            format.name()
        ))),
        None => Ok(refs),
    }
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

pub(crate) fn read_header(format: Format, buf: &[u8], offset: usize) -> Result<Header, PackError> {
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
            let end = pos + format.raw_len();
            let oid = buf.get(pos..end).and_then(|b| format.oid_from_bytes(b));
            pos = end;
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

/// Copy exactly one pack from a stream that may continue after it (SSH keeps the
/// connection open for the status report). Objects are inflated only to find where
/// each one ends; [`parse`] validates them afterwards. Fails past `limit` bytes.
pub fn copy_stream(
    format: Format,
    r: &mut dyn BufRead,
    w: &mut dyn Write,
    limit: u64,
) -> Result<u64, PackError> {
    let mut copied = 0u64;
    let take = |r: &mut dyn BufRead, n: usize| -> Result<Vec<u8>, PackError> {
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf)?;
        Ok(buf)
    };
    let mut emit = |w: &mut dyn Write, bytes: &[u8]| -> Result<(), PackError> {
        copied += bytes.len() as u64;
        if copied > limit {
            return Err(PackError::TooLarge);
        }
        Ok(w.write_all(bytes)?)
    };
    let head = take(r, 12)?;
    if &head[..4] != b"PACK" {
        return Err(bad("missing PACK signature"));
    }
    let count = u32::from_be_bytes(head[8..12].try_into().unwrap());
    emit(w, &head)?;
    let mut scratch = vec![0u8; 64 << 10];
    for _ in 0..count {
        // Type and size, then the delta base if any.
        let mut c = take(r, 1)?[0];
        let code = (c >> 4) & 7;
        emit(w, &[c])?;
        while c & 0x80 != 0 {
            c = take(r, 1)?[0];
            emit(w, &[c])?;
        }
        match code {
            OFS_DELTA => loop {
                let b = take(r, 1)?[0];
                emit(w, &[b])?;
                if b & 0x80 == 0 {
                    break;
                }
            },
            REF_DELTA => emit(w, &take(r, format.raw_len())?)?,
            1..=4 => {}
            _ => return Err(bad(format!("unknown object type {code}"))),
        }
        let mut z = Decompress::new(true);
        loop {
            let input = r.fill_buf()?;
            if input.is_empty() {
                return Err(bad("truncated zlib stream"));
            }
            let before = z.total_in();
            let status = z
                .decompress(input, &mut scratch, FlushDecompress::None)
                .map_err(|e| bad(format!("zlib: {e}")))?;
            let used = (z.total_in() - before) as usize;
            emit(w, &input[..used])?;
            r.consume(used);
            if status == Status::StreamEnd {
                break;
            }
        }
    }
    emit(w, &take(r, format.raw_len())?)?;
    Ok(copied)
}

/// Decode a whole pack of a repository in `format`: verify the trailer, resolve deltas
/// inside the pack, compute every object id, and record each object's outgoing
/// references (which must be ids in `format` too). Deltas against
/// objects outside the pack are rejected, because a stored pack must be indexable on
/// its own when a client downloads it.
///
/// Memory stays bounded by delta chains, not pack size: the first pass streams every
/// object through a fixed buffer (hashing whole objects as it goes), and the second
/// re-inflates only delta bases, depth first, dropping each once its chain is done.
pub fn parse(format: Format, buf: &[u8]) -> Result<ParsedPack, PackError> {
    if buf.len() < 12 + format.raw_len() || &buf[..4] != b"PACK" {
        return Err(bad("missing PACK signature"));
    }
    let version = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        return Err(bad(format!("unsupported pack version {version}")));
    }
    let count = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
    let body_end = buf.len() - format.raw_len();
    let mut trailer = format.hasher();
    trailer.update(&buf[..body_end]);
    let hash = trailer.finish();
    if hash.as_bytes() != &buf[body_end..] {
        return Err(PackError::Checksum);
    }
    let body = &buf[..body_end];

    // Per-object state is kept small (ids live once, interned in the graph): a large
    // import has millions of objects.
    struct Raw {
        offset: u64,
        data_at: usize,
        size: usize,
    }
    let mut raws: Vec<Raw> = Vec::with_capacity(count.min(MAX_PREALLOC / 64));
    // Each object's kind and interned id, once known.
    let mut resolved: Vec<Option<(Kind, u32)>> = Vec::with_capacity(raws.capacity());
    let mut graph = Graph::new(format);
    let mut budget = MIN_INFLATED_BUDGET.max(buf.len().saturating_mul(INFLATE_RATIO));
    // Deltas waiting for their base, by the base's position or id.
    let mut by_offset: HashMap<u64, usize> = HashMap::new();
    let mut children_by_index: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut children_by_oid: HashMap<Oid, Vec<usize>> = HashMap::new();

    // Pass 1: find object boundaries; hash whole objects while streaming them.
    let mut pos = 12;
    for i in 0..count {
        if pos >= body_end {
            return Err(bad("pack ends before its object count"));
        }
        let h = read_header(format, body, pos)?;
        budget = budget.checked_sub(h.size).ok_or(PackError::TooLarge)?;
        let data_at = pos + h.len;
        let kind = Kind::from_code(h.code);
        let used = match (kind, &h.base) {
            (Some(kind), Base::None) => {
                let mut sha = object_hasher(format, kind, h.size);
                // Keep the bytes only for objects whose references we need.
                let mut keep = (kind != Kind::Blob).then(Vec::new);
                let used = inflate_with(&body[data_at..], h.size, |c| {
                    sha.update(c);
                    if let Some(k) = keep.as_mut() {
                        k.extend_from_slice(c);
                    }
                })?;
                let gi = graph.intern(sha.finish());
                if let Some(data) = keep {
                    graph.set_refs(gi, &object_refs(format, kind, &data)?);
                }
                resolved.push(Some((kind, gi)));
                used
            }
            _ => {
                resolved.push(None);
                inflate_with(&body[data_at..], h.size, |_| {})?
            }
        };
        // An offset delta's base comes earlier in the pack, so it is known by now.
        match h.base {
            Base::None => {}
            Base::Offset(off) => {
                let base = *by_offset
                    .get(&off)
                    .ok_or_else(|| bad("delta base offset is not an object"))?;
                children_by_index.entry(base).or_default().push(i);
            }
            Base::Oid(oid) => children_by_oid.entry(oid).or_default().push(i),
        }
        by_offset.insert(pos as u64, i);
        raws.push(Raw {
            offset: pos as u64,
            data_at,
            size: h.size,
        });
        pos = data_at + used;
    }
    drop(by_offset);
    if pos != body_end {
        return Err(bad("trailing bytes after last object"));
    }

    // Pass 2: resolve deltas depth first from each base.
    let mut take_children = |i: usize, oid: &Oid| -> Vec<usize> {
        let mut kids = children_by_index.remove(&i).unwrap_or_default();
        kids.extend(children_by_oid.remove(oid).unwrap_or_default());
        kids
    };
    let roots: Vec<usize> = (0..raws.len()).filter(|&i| resolved[i].is_some()).collect();
    // Each pending delta carries its base's bytes and kind; a delta has its base's kind.
    let mut stack: Vec<(usize, Rc<Vec<u8>>, Kind)> = Vec::new();
    for root in roots {
        let (kind, gi) = resolved[root].unwrap();
        let kids = take_children(root, &graph.ids[gi as usize]);
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
            let oid = Oid::for_object(format, kind, &data);
            let gi = graph.intern(oid);
            if kind != Kind::Blob {
                graph.set_refs(gi, &object_refs(format, kind, &data)?);
            }
            resolved[k] = Some((kind, gi));
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
    let mut commits = HashMap::new();
    let mut tags = HashMap::new();
    for (r, res) in raws.iter().zip(&resolved) {
        let (kind, gi) = res.unwrap();
        let oid = graph.ids[gi as usize];
        entries.push(IdxEntry {
            offset: r.offset,
            oid,
        });
        graph.in_pack[gi as usize] = true;
        let refs = graph.refs(gi);
        match kind {
            // Commit refs are [tree, parents...]; tag refs are [target].
            Kind::Commit => {
                let parents = refs[1..].iter().map(|&r| graph.ids[r as usize]);
                commits.insert(oid, parents.collect());
            }
            Kind::Tag => {
                tags.insert(oid, graph.ids[refs[0] as usize]);
            }
            _ => {}
        }
    }
    Ok(ParsedPack {
        hash,
        entries,
        commits,
        tags,
        graph,
    })
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

/// Serialize a sidecar index: entries in pack order, each an offset and an id (28
/// bytes for SHA-1, 40 for SHA-256; the pack's name says which).
pub fn encode_idx(entries: &[IdxEntry]) -> Vec<u8> {
    let width = 8 + entries.first().map_or(0, |e| e.oid.as_bytes().len());
    let mut out = Vec::with_capacity(8 + entries.len() * width);
    out.extend_from_slice(b"PIDX");
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&e.offset.to_be_bytes());
        out.extend_from_slice(e.oid.as_bytes());
    }
    out
}

/// Read a sidecar index of a pack in `format`.
pub fn decode_idx(format: Format, buf: &[u8]) -> io::Result<Vec<IdxEntry>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "bad pack sidecar index");
    if buf.len() < 8 || &buf[..4] != b"PIDX" {
        return Err(invalid());
    }
    let n = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
    let body = &buf[8..];
    let width = 8 + format.raw_len();
    if body.len() != n * width {
        return Err(invalid());
    }
    Ok(body
        .chunks_exact(width)
        .map(|c| IdxEntry {
            offset: u64::from_be_bytes(c[..8].try_into().unwrap()),
            oid: format.oid_from_bytes(&c[8..]).unwrap(),
        })
        .collect())
}

/// A writer that hashes everything passing through, so a pack's trailer can be
/// appended without buffering the pack.
pub struct HashingWriter<W: Write> {
    inner: W,
    sha: crate::object::Hasher,
    pub written: u64,
}

impl<W: Write> HashingWriter<W> {
    /// A writer for a pack whose trailer is a hash in `format`.
    pub fn new(format: Format, inner: W) -> Self {
        HashingWriter {
            inner,
            sha: format.hasher(),
            written: 0,
        }
    }

    /// Write the trailer and return the pack hash plus the inner writer.
    pub fn finish(self) -> io::Result<(Oid, W)> {
        let HashingWriter { mut inner, sha, .. } = self;
        let hash = sha.finish();
        inner.write_all(hash.as_bytes())?;
        Ok((hash, inner))
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
pub fn empty_pack(format: Format) -> Vec<u8> {
    let mut w = HashingWriter::new(format, Vec::new());
    w.write_all(b"PACK\0\0\0\x02\0\0\0\0").unwrap();
    w.finish().unwrap().1
}

/// Splice several stored packs into one, without inflating any object.
///
/// Objects already emitted by an earlier pack are dropped. Offset deltas are re-encoded
/// against the base's new position (which is always earlier, since input order is
/// preserved); ref deltas and whole objects are copied byte for byte. Used for
/// compaction covers and for inline fetch responses to clients without packfile-uris.
///
/// Output streams to `out`. Memory grows with the number of objects (the input and
/// returned indexes, plus about 40 bytes each), never with their size. The packs
/// all belong to one repository, in `format`.
pub fn concat<W: Write>(
    format: Format,
    packs: &[(&[u8], &[IdxEntry])],
    out: W,
) -> io::Result<(Oid, Vec<IdxEntry>, W)> {
    // Where each object first appears, as (pack, entry). Later copies are dropped.
    let mut first: HashMap<Oid, (u32, u32)> = HashMap::new();
    for (p, (_, idx)) in packs.iter().enumerate() {
        for (i, e) in idx.iter().enumerate() {
            first.entry(e.oid).or_insert((p as u32, i as u32));
        }
    }
    let total = u32::try_from(first.len()).map_err(|_| io::Error::other("too many objects"))?;

    let mut w = HashingWriter::new(format, out);
    w.write_all(b"PACK\0\0\0\x02")?;
    w.write_all(&total.to_be_bytes())?;
    // Each entry's offset in the new pack, by input pack (u64::MAX where dropped).
    let mut new_offsets: Vec<Vec<u64>> = Vec::with_capacity(packs.len());
    let mut entries = Vec::with_capacity(first.len());
    let mut head = Vec::with_capacity(32);
    for (p, (buf, idx)) in packs.iter().enumerate() {
        let mut offsets = vec![u64::MAX; idx.len()];
        let body_end = (buf.len() - format.raw_len()) as u64;
        for (i, e) in idx.iter().enumerate() {
            if first[&e.oid] != (p as u32, i as u32) {
                continue;
            }
            let end = idx.get(i + 1).map_or(body_end, |n| n.offset);
            let h = read_header(format, buf, e.offset as usize).map_err(io::Error::other)?;
            let data = &buf[e.offset as usize + h.len..end as usize];
            let here = w.written; // offset within the new pack (header included)
            head.clear();
            match h.base {
                Base::Offset(base_off) => {
                    // Entries are in pack order, so the base is found by offset.
                    let j = idx
                        .binary_search_by_key(&base_off, |e| e.offset)
                        .map_err(|_| io::Error::other("dangling ofs-delta"))?;
                    // A dropped base was written as its first copy: earlier in this
                    // pack, or in an earlier one.
                    let base_new = match offsets[j] {
                        u64::MAX => {
                            let (bp, bi) = first[&idx[j].oid];
                            let from = new_offsets.get(bp as usize).unwrap_or(&offsets);
                            from[bi as usize]
                        }
                        o => o,
                    };
                    if base_new >= here {
                        return Err(io::Error::other("ofs-delta base after the delta"));
                    }
                    encode_type_size(OFS_DELTA, h.size as u64, &mut head);
                    encode_ofs(here - base_new, &mut head);
                }
                _ => head.extend_from_slice(&buf[e.offset as usize..e.offset as usize + h.len]),
            }
            w.write_all(&head)?;
            w.write_all(data)?;
            offsets[i] = here;
            entries.push(IdxEntry {
                offset: here,
                oid: e.oid,
            });
        }
        new_offsets.push(offsets);
    }
    let (hash, inner) = w.finish()?;
    Ok((hash, entries, inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pack_parses() {
        for format in [Format::Sha1, Format::Sha256] {
            let pack = empty_pack(format);
            assert_eq!(pack.len(), 12 + format.raw_len());
            assert!(parse(format, &pack).unwrap().entries.is_empty());
        }
        assert!(matches!(
            parse(Format::Sha256, &empty_pack(Format::Sha1)),
            Err(PackError::Malformed(_))
        ));
    }

    /// A pack around `objects` with a correct header count and trailer.
    fn wrap(count: u32, objects: &[u8]) -> Vec<u8> {
        let mut w = HashingWriter::new(Format::Sha1, Vec::new());
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
        assert!(matches!(
            parse(Format::Sha1, &wrap(1, &obj)),
            Err(PackError::TooLarge)
        ));

        // A delta declaring a 2^50-byte result.
        let delta = [0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(matches!(apply_delta(&[], &delta), Err(PackError::TooLarge)));

        // A ref-delta header that runs into the trailer.
        let mut obj = Vec::new();
        encode_type_size(REF_DELTA, 5, &mut obj);
        obj.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert!(parse(Format::Sha1, &wrap(1, &obj)).is_err());
    }

    /// A delta may sit on the second copy of an object its own pack holds twice.
    #[test]
    fn concat_follows_a_delta_to_the_kept_copy_of_its_base() {
        let deflate = |data: &[u8]| {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Default::default());
            z.write_all(data).unwrap();
            z.finish().unwrap()
        };
        let mut objects = Vec::new();
        for _ in 0..2 {
            encode_type_size(3, 6, &mut objects);
            objects.extend_from_slice(&deflate(b"hello\n"));
        }
        let second = 12 + objects.len() as u64 / 2;
        let delta_at = 12 + objects.len() as u64;
        // Copy the base's 6 bytes, then insert "!".
        let delta = [6, 7, 0x90, 6, 1, b'!'];
        encode_type_size(OFS_DELTA, delta.len() as u64, &mut objects);
        encode_ofs(delta_at - second, &mut objects);
        objects.extend_from_slice(&deflate(&delta));

        let pack = wrap(3, &objects);
        let parsed = parse(Format::Sha1, &pack).unwrap();
        assert_eq!(parsed.entries[0].oid, parsed.entries[1].oid);
        let (_, entries, out) =
            concat(Format::Sha1, &[(&pack, &parsed.entries)], Vec::new()).unwrap();
        assert_eq!(entries.len(), 2);
        let spliced = parse(Format::Sha1, &out).unwrap();
        assert_eq!(spliced.entries[1].oid, parsed.entries[2].oid);
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
