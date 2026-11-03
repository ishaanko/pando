//! Git object ids, kinds, and the small amount of commit/tag parsing Pando needs.

use sha1::{Digest, Sha1};
use std::fmt;

/// A SHA-1 object id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const ZERO: Oid = Oid([0; 20]);

    pub fn from_hex(s: &str) -> Option<Oid> {
        let mut out = [0u8; 20];
        hex::decode_to_slice(s.get(..40)?, &mut out).ok()?;
        (s.len() == 40).then_some(Oid(out))
    }

    pub fn from_slice(b: &[u8]) -> Option<Oid> {
        Some(Oid(b.try_into().ok()?))
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn is_zero(&self) -> bool {
        *self == Oid::ZERO
    }

    /// The id Git assigns to an object: sha1("<kind> <len>\0<data>").
    pub fn for_object(kind: Kind, data: &[u8]) -> Oid {
        let mut h = Sha1::new();
        h.update(kind.name().as_bytes());
        h.update(b" ");
        h.update(data.len().to_string().as_bytes());
        h.update([0]);
        h.update(data);
        Oid(h.finalize().into())
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", &self.hex()[..12])
    }
}

/// The four object kinds. Discriminants match the pack type codes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
}

impl Kind {
    pub fn from_code(code: u8) -> Option<Kind> {
        match code {
            1 => Some(Kind::Commit),
            2 => Some(Kind::Tree),
            3 => Some(Kind::Blob),
            4 => Some(Kind::Tag),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }
}

/// Tree entries use this mode for submodules; the id names a commit in another
/// repository, so it is not a reference Pando can or should resolve.
pub const GITLINK_MODE: u32 = 0o160000;

/// The root tree of a commit (the `tree <hex>` header line).
pub fn commit_tree(body: &[u8]) -> Option<Oid> {
    header_lines(body)
        .find_map(|line| line.strip_prefix(b"tree "))
        .and_then(|hex| Oid::from_hex(std::str::from_utf8(hex).ok()?))
}

/// `(mode, id)` for each entry of a tree object: `<octal mode> <name>\0<20-byte id>`.
pub fn tree_entries(mut body: &[u8]) -> Option<Vec<(u32, Oid)>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let space = body.iter().position(|&b| b == b' ')?;
        let mode = u32::from_str_radix(std::str::from_utf8(&body[..space]).ok()?, 8).ok()?;
        let nul = space + body[space..].iter().position(|&b| b == 0)?;
        let oid = Oid::from_slice(body.get(nul + 1..nul + 21)?)?;
        out.push((mode, oid));
        body = &body[nul + 21..];
    }
    Some(out)
}

/// Committer timestamp (seconds since the epoch) of a commit.
pub fn commit_time(body: &[u8]) -> Option<i64> {
    let line = header_lines(body).find_map(|line| line.strip_prefix(b"committer "))?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.rsplitn(3, ' ');
    let _tz = parts.next()?;
    parts.next()?.parse().ok()
}

/// Parent ids from a commit body (the `parent <hex>` header lines).
pub fn commit_parents(body: &[u8]) -> Vec<Oid> {
    header_lines(body)
        .filter_map(|line| line.strip_prefix(b"parent "))
        .filter_map(|hex| Oid::from_hex(std::str::from_utf8(hex).ok()?))
        .collect()
}

/// The object an annotated tag points at (the `object <hex>` header line).
pub fn tag_target(body: &[u8]) -> Option<Oid> {
    header_lines(body)
        .find_map(|line| line.strip_prefix(b"object "))
        .and_then(|hex| Oid::from_hex(std::str::from_utf8(hex).ok()?))
}

/// Header lines of a commit or tag, stopping at the blank line before the message.
fn header_lines(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    body.split(|&b| b == b'\n').take_while(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_id_matches_git() {
        assert_eq!(
            Oid::for_object(Kind::Blob, b"").hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }
}
