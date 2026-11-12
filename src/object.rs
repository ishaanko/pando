//! Git object ids and formats, kinds, and the small amount of commit/tag parsing
//! Pando needs.
//!
//! A repository names its objects (and packs) with one hash, its [`Format`]: SHA-1
//! (20-byte ids) or SHA-256 (32-byte ids), fixed when the repository is created. An
//! [`Oid`] carries its format, so ids of different formats never compare equal and
//! nothing needs a format to print, store or look one up. Ids that arrive from a
//! client are parsed with [`Format::parse_oid`], which rejects the other format;
//! [`Oid::from_hex`] takes either and is for names Pando wrote itself.

use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fmt;
use std::hash::Hash;

/// The hash a repository names objects with. Existing repositories without a
/// recorded format are SHA-1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    #[default]
    Sha1,
    Sha256,
}

impl Format {
    /// The name in `object-format=` capabilities, the CLI and the admin API.
    pub fn name(self) -> &'static str {
        match self {
            Format::Sha1 => "sha1",
            Format::Sha256 => "sha256",
        }
    }

    pub fn from_name(name: &str) -> Option<Format> {
        [Format::Sha1, Format::Sha256]
            .into_iter()
            .find(|f| f.name() == name)
    }

    /// Bytes in an id (and a pack trailer).
    pub fn raw_len(self) -> usize {
        match self {
            Format::Sha1 => 20,
            Format::Sha256 => 32,
        }
    }

    /// The all-zero id, which push commands and advertisements use for "no object".
    pub fn zero(self) -> Oid {
        match self {
            Format::Sha1 => Oid::Sha1([0; 20]),
            Format::Sha256 => Oid::Sha256([0; 32]),
        }
    }

    /// An id in this format from hex: exactly `2 * raw_len` lowercase or uppercase
    /// hex digits. Use it for every id a client sends.
    pub fn parse_oid(self, hex: &str) -> Option<Oid> {
        Oid::from_hex(hex).filter(|o| o.format() == self)
    }

    /// An id in this format from raw bytes (tree entries, ref-delta bases).
    pub fn oid_from_bytes(self, b: &[u8]) -> Option<Oid> {
        Some(match self {
            Format::Sha1 => Oid::Sha1(b.try_into().ok()?),
            Format::Sha256 => Oid::Sha256(b.try_into().ok()?),
        })
    }

    pub fn hasher(self) -> Hasher {
        match self {
            Format::Sha1 => Hasher::Sha1(Sha1::new()),
            Format::Sha256 => Hasher::Sha256(Sha256::new()),
        }
    }
}

/// A running hash in some format: object ids and pack trailers.
#[derive(Clone)]
pub enum Hasher {
    Sha1(Sha1),
    Sha256(Sha256),
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha1(h) => h.update(data),
            Hasher::Sha256(h) => h.update(data),
        }
    }

    pub fn finish(self) -> Oid {
        match self {
            Hasher::Sha1(h) => Oid::Sha1(h.finalize().into()),
            Hasher::Sha256(h) => Oid::Sha256(h.finalize().into()),
        }
    }
}

/// An object (or pack) id, in its format. `Copy`, 33 bytes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Oid {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

/// The id's bytes alone, as the fixed-size array they are. Ids of different formats
/// differ in length, so they never collide as keys either.
impl Hash for Oid {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Oid::Sha1(b) => state.write(b),
            Oid::Sha256(b) => state.write(b),
        }
    }
}

impl Oid {
    /// An id of either format, told apart by length (40 or 64 hex digits). For names
    /// Pando wrote (metadata, file names, pack URLs); ids from clients go through
    /// [`Format::parse_oid`].
    pub fn from_hex(s: &str) -> Option<Oid> {
        fn decode<const N: usize>(s: &str) -> Option<[u8; N]> {
            let mut out = [0u8; N];
            hex::decode_to_slice(s, &mut out).ok()?;
            Some(out)
        }
        match s.len() {
            40 => decode(s).map(Oid::Sha1),
            64 => decode(s).map(Oid::Sha256),
            _ => None,
        }
    }

    pub fn format(&self) -> Format {
        match self {
            Oid::Sha1(_) => Format::Sha1,
            Oid::Sha256(_) => Format::Sha256,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Oid::Sha1(b) => b,
            Oid::Sha256(b) => b,
        }
    }

    pub fn hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    pub fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }

    /// The id Git assigns to an object: hash("<kind> <len>\0<data>").
    pub fn for_object(format: Format, kind: Kind, data: &[u8]) -> Oid {
        let mut h = object_hasher(format, kind, data.len());
        h.update(data);
        h.finish()
    }
}

/// A hasher primed with an object's header, ready for its `size` bytes of contents.
pub fn object_hasher(format: Format, kind: Kind, size: usize) -> Hasher {
    let mut h = format.hasher();
    h.update(format!("{} {size}\0", kind.name()).as_bytes());
    h
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

/// The root tree of a commit (the `tree <hex>` header line). Header ids parse in
/// either format; `pack::parse` checks pushed objects name ids in the repository's.
pub fn commit_tree(body: &[u8]) -> Option<Oid> {
    header_lines(body)
        .find_map(|line| line.strip_prefix(b"tree "))
        .and_then(|hex| Oid::from_hex(std::str::from_utf8(hex).ok()?))
}

/// `(mode, id)` for each entry of a tree object: `<octal mode> <name>\0<raw id>`,
/// with ids as long as the repository's `format` makes them.
pub fn tree_entries(format: Format, mut body: &[u8]) -> Option<Vec<(u32, Oid)>> {
    let mut out = Vec::new();
    let end = |nul: usize| nul + 1 + format.raw_len();
    while !body.is_empty() {
        let space = body.iter().position(|&b| b == b' ')?;
        let mode = u32::from_str_radix(std::str::from_utf8(&body[..space]).ok()?, 8).ok()?;
        let nul = space + body[space..].iter().position(|&b| b == 0)?;
        let oid = format.oid_from_bytes(body.get(nul + 1..end(nul))?)?;
        out.push((mode, oid));
        body = &body[end(nul)..];
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
            Oid::for_object(Format::Sha1, Kind::Blob, b"").hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            Oid::for_object(Format::Sha256, Kind::Blob, b"").hex(),
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
        );
    }

    #[test]
    fn ids_keep_their_format() {
        let hex1 = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let one = Oid::from_hex(hex1).unwrap();
        assert_eq!(one.format(), Format::Sha1);
        assert_eq!(Format::Sha256.parse_oid(hex1), None);
        assert_eq!(Format::Sha1.parse_oid(hex1), Some(one));
        // A SHA-1 id is not a SHA-256 id with zeros after it.
        let mut padded = [0u8; 32];
        padded[..20].copy_from_slice(one.as_bytes());
        assert_ne!(Oid::Sha256(padded), one);
        assert_ne!(Format::Sha1.zero(), Format::Sha256.zero());
        assert_eq!(Format::Sha256.zero().hex().len(), 64);
        assert_eq!(std::mem::size_of::<Oid>(), 33);
    }
}
