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

const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("malformed pack: {0}")]
    Malformed(String),
    #[error("pack checksum mismatch")]
    Checksum,
    #[error("thin packs are not accepted ({0} deltas reference objects outside the pack)")]
    Thin(usize),
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
}

impl ParsedPack {
    pub fn contains(&self, oid: &Oid) -> bool {
        self.commits.contains_key(oid)
            || self.tags.contains_key(oid)
            || self.entries.iter().any(|e| e.oid == *oid)
    }
}

/// A parsed object header: type code, inflated size, header length, delta base.
struct Header {
    code: u8,
    size: usize,
    len: usize,
    base: Base,
}

enum Base {
    None,
    Offset(u64),
    Oid(Oid),
}

fn read_header(buf: &[u8], offset: usize) -> Result<Header, PackError> {
    let mut pos = offset;
    let byte = |pos: usize| buf.get(pos).copied().ok_or_else(|| bad("truncated object header"));
    let mut c = byte(pos)?;
    pos += 1;
    let code = (c >> 4) & 7;
    let mut size = (c & 15) as usize;
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = byte(pos)?;
        pos += 1;
        size |= ((c & 0x7f) as usize).checked_shl(shift).ok_or_else(|| bad("size overflow"))?;
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
    Ok(Header { code, size, len: pos - offset, base })
}

/// Inflate one zlib stream that must produce exactly `size` bytes. Returns the data and
/// how many compressed bytes the stream used.
fn inflate(input: &[u8], size: usize) -> Result<(Vec<u8>, usize), PackError> {
    let mut z = Decompress::new(true);
    let mut out = Vec::with_capacity(size + 1);
    loop {
        let used = z.total_in() as usize;
        let before = (z.total_in(), z.total_out());
        let status = z
            .decompress_vec(&input[used..], &mut out, FlushDecompress::Finish)
            .map_err(|e| bad(format!("zlib: {e}")))?;
        match status {
            Status::StreamEnd => break,
            _ if out.len() > size => return Err(bad("object larger than its header")),
            _ if (z.total_in(), z.total_out()) == before => return Err(bad("truncated zlib stream")),
            _ => out.reserve(64),
        }
    }
    if out.len() != size {
        return Err(bad("object size does not match header"));
    }
    Ok((out, z.total_in() as usize))
}

/// Decode a whole pack in one pass: verify the trailer, resolve deltas inside the pack,
/// and compute every object id. Deltas against objects outside the pack are rejected,
/// because a stored pack must be indexable on its own when a client downloads it.
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

    // Pass 1: walk headers and inflate everything.
    struct Raw {
        offset: u64,
        kind: Option<Kind>,
        base: Base,
        data: Vec<u8>,
    }
    let mut raws = Vec::with_capacity(count.min(1 << 20));
    let mut pos = 12;
    for _ in 0..count {
        if pos >= body_end {
            return Err(bad("pack ends before its object count"));
        }
        let h = read_header(buf, pos)?;
        let (data, used) = inflate(&buf[pos + h.len..body_end], h.size)?;
        raws.push(Raw { offset: pos as u64, kind: Kind::from_code(h.code), base: h.base, data });
        pos += h.len + used;
    }
    if pos != body_end {
        return Err(bad("trailing bytes after last object"));
    }

    // Pass 2: resolve deltas breadth-first from their bases.
    let by_offset: HashMap<u64, usize> = raws.iter().enumerate().map(|(i, r)| (r.offset, i)).collect();
    let mut children_by_index: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut children_by_oid: HashMap<Oid, Vec<usize>> = HashMap::new();
    let mut resolved: Vec<Option<(Kind, Oid)>> = vec![None; raws.len()];
    let mut bodies: Vec<Option<Vec<u8>>> = Vec::with_capacity(raws.len());
    let mut queue = Vec::new();
    for (i, r) in raws.iter_mut().enumerate() {
        match r.base {
            Base::None => {
                let kind = r.kind.unwrap();
                resolved[i] = Some((kind, Oid::for_object(kind, &r.data)));
                queue.push(i);
            }
            Base::Offset(off) => {
                let base = *by_offset.get(&off).ok_or_else(|| bad("delta base offset is not an object"))?;
                children_by_index.entry(base).or_default().push(i);
            }
            Base::Oid(oid) => children_by_oid.entry(oid).or_default().push(i),
        }
        bodies.push(Some(std::mem::take(&mut r.data)));
    }
    let mut full: Vec<Option<Vec<u8>>> = vec![None; raws.len()];
    while let Some(i) = queue.pop() {
        let (kind, oid) = resolved[i].unwrap();
        if full[i].is_none() {
            full[i] = bodies[i].take();
        }
        let mut kids = children_by_index.remove(&i).unwrap_or_default();
        kids.extend(children_by_oid.remove(&oid).unwrap_or_default());
        for k in kids {
            if resolved[k].is_some() {
                continue;
            }
            let delta = bodies[k].take().unwrap();
            let data = apply_delta(full[i].as_ref().unwrap(), &delta)?;
            resolved[k] = Some((kind, Oid::for_object(kind, &data)));
            full[k] = Some(data);
            queue.push(k);
        }
    }
    let unresolved = resolved.iter().filter(|r| r.is_none()).count();
    if unresolved > 0 {
        return Err(PackError::Thin(unresolved));
    }

    let mut entries = Vec::with_capacity(raws.len());
    let mut commits = HashMap::new();
    let mut tags = HashMap::new();
    for (i, r) in raws.iter().enumerate() {
        let (kind, oid) = resolved[i].unwrap();
        entries.push(IdxEntry { offset: r.offset, oid });
        let data = full[i].as_deref().unwrap();
        match kind {
            Kind::Commit => {
                commits.insert(oid, commit_parents(data));
            }
            Kind::Tag => {
                tags.insert(oid, tag_target(data).ok_or_else(|| bad("tag without object line"))?);
            }
            _ => {}
        }
    }
    Ok(ParsedPack { hash, entries, commits, tags })
}

/// Apply a Git delta (copy/insert instruction stream) to `base`.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut pos = 0;
    let mut varint = |pos: &mut usize| -> Result<usize, PackError> {
        let (mut v, mut shift) = (0usize, 0);
        loop {
            let c = *delta.get(*pos).ok_or_else(|| bad("truncated delta header"))?;
            *pos += 1;
            v |= ((c & 0x7f) as usize).checked_shl(shift).ok_or_else(|| bad("delta size overflow"))?;
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
    let mut out = Vec::with_capacity(dst_len);
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
            let src = base.get(off..off + len).ok_or_else(|| bad("delta copy out of range"))?;
            out.extend_from_slice(src);
        } else if cmd != 0 {
            let lit = delta.get(pos..pos + cmd as usize).ok_or_else(|| bad("truncated insert"))?;
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
        .chunks_exact(28)
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
        HashingWriter { inner, sha: Sha1::new(), written: 0 }
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

fn encode_type_size(code: u8, mut size: u64, out: &mut Vec<u8>) {
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
pub fn concat<W: Write>(packs: &[(&[u8], &[IdxEntry])], out: W) -> io::Result<(Oid, Vec<IdxEntry>, W)> {
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
                    let base_oid = oid_at.get(&base_off).ok_or_else(|| io::Error::other("dangling ofs-delta"))?;
                    let base_new = new_offset[base_oid];
                    encode_type_size(OFS_DELTA, h.size as u64, &mut head);
                    encode_ofs(here - base_new, &mut head);
                }
                _ => head.extend_from_slice(&buf[e.offset as usize..e.offset as usize + h.len]),
            }
            w.write_all(&head)?;
            w.write_all(data)?;
            new_offset.insert(e.oid, here);
            entries.push(IdxEntry { offset: here, oid: e.oid });
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

    #[test]
    fn ofs_roundtrip() {
        for dist in [1u64, 127, 128, 16511, 16512, 1 << 40] {
            let mut enc = Vec::new();
            encode_ofs(dist, &mut enc);
            assert_eq!(decode_ofs(&enc), Some((dist, enc.len())));
        }
    }
}
