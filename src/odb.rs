//! Random access to objects in stored packs.
//!
//! Pando's normal paths never open objects: they move whole packs around. Shallow
//! fetches are the exception, since cutting history means choosing objects one by one.

use crate::object::{Kind, Oid};
use crate::pack::{self, Base, PackError};
use crate::store::BlobStore;
use memmap2::Mmap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// An object's kind and contents, shared between readers.
pub type Object = (Kind, Rc<Vec<u8>>);

/// Delta bases kept materialized between reads.
const CACHE_ENTRIES: usize = 512;

struct Pack {
    map: Mmap,
    /// Object start offsets in pack order; an entry ends where the next begins.
    offsets: Vec<u64>,
    /// The object at each offset.
    oids: Vec<Oid>,
}

pub struct Odb {
    packs: Vec<Pack>,
    index: HashMap<Oid, (usize, u64)>,
    cache: RefCell<HashMap<(usize, u64), Object>>,
}

impl Odb {
    /// Open a set of stored packs. Earlier packs win when an object appears twice.
    pub fn open(store: &BlobStore, hashes: &[Oid]) -> std::io::Result<Odb> {
        let mut packs = Vec::new();
        let mut index = HashMap::new();
        for (i, hash) in hashes.iter().enumerate() {
            let idx = store.read_idx(hash)?;
            for e in &idx {
                index.entry(e.oid).or_insert((i, e.offset));
            }
            let mut by_offset: Vec<(u64, Oid)> = idx.iter().map(|e| (e.offset, e.oid)).collect();
            by_offset.sort();
            packs.push(Pack {
                map: store.map_pack(hash)?,
                offsets: by_offset.iter().map(|e| e.0).collect(),
                oids: by_offset.iter().map(|e| e.1).collect(),
            });
        }
        Ok(Odb {
            packs,
            index,
            cache: RefCell::new(HashMap::new()),
        })
    }

    pub fn contains(&self, oid: &Oid) -> bool {
        self.index.contains_key(oid)
    }

    /// An object's kind and contents.
    pub fn read(&self, oid: &Oid) -> Result<Option<Object>, PackError> {
        match self.index.get(oid) {
            Some(&(p, off)) => self.read_at(p, off).map(Some),
            None => Ok(None),
        }
    }

    /// The stored bytes of an object (header plus compressed data) when it is stored
    /// whole rather than as a delta, so it can be copied into a new pack untouched.
    pub fn raw_whole(&self, oid: &Oid) -> Option<&[u8]> {
        let &(p, off) = self.index.get(oid)?;
        let pack = &self.packs[p];
        let body = &pack.map[..pack.map.len() - 20];
        let h = pack::read_header(body, off as usize).ok()?;
        if !matches!(h.base, Base::None) {
            return None;
        }
        let i = pack.offsets.binary_search(&off).ok()?;
        let end = pack.offsets.get(i + 1).map_or(body.len() as u64, |n| *n);
        Some(&body[off as usize..end as usize])
    }

    /// An object stored as a delta: its base, the delta's inflated size, and the
    /// compressed delta bytes, so it can be copied into a new pack as a ref-delta.
    pub fn raw_delta(&self, oid: &Oid) -> Option<(Oid, usize, &[u8])> {
        let &(p, off) = self.index.get(oid)?;
        let pack = &self.packs[p];
        let body = &pack.map[..pack.map.len() - 20];
        let h = pack::read_header(body, off as usize).ok()?;
        let base = match h.base {
            Base::None => return None,
            Base::Oid(b) => b,
            Base::Offset(b) => pack.oids[pack.offsets.binary_search(&b).ok()?],
        };
        let i = pack.offsets.binary_search(&off).ok()?;
        let end = pack.offsets.get(i + 1).map_or(body.len() as u64, |n| *n);
        Some((base, h.size, &body[off as usize + h.len..end as usize]))
    }

    fn read_at(&self, p: usize, off: u64) -> Result<Object, PackError> {
        if let Some(hit) = self.cache.borrow().get(&(p, off)) {
            return Ok(hit.clone());
        }
        let pack = &self.packs[p];
        let body = &pack.map[..pack.map.len() - 20];
        let h = pack::read_header(body, off as usize)?;
        let (data, _) = pack::inflate(&body[off as usize + h.len..], h.size)?;
        let result = match h.base {
            Base::None => {
                let kind = Kind::from_code(h.code)
                    .ok_or_else(|| PackError::Malformed("bad type".into()))?;
                (kind, Rc::new(data))
            }
            Base::Offset(base_off) => {
                let (kind, base) = self.read_at(p, base_off)?;
                (kind, Rc::new(pack::apply_delta(&base, &data)?))
            }
            Base::Oid(base_oid) => {
                let (kind, base) = self.read(&base_oid)?.ok_or_else(|| {
                    PackError::Malformed(format!("missing delta base {base_oid}"))
                })?;
                (kind, Rc::new(pack::apply_delta(&base, &data)?))
            }
        };
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert((p, off), result.clone());
        Ok(result)
    }
}
