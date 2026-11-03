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
