//! The metadata store (the diagrams call it "Turtle").
//!
//! Every time a ref moves, Pando appends an immutable *ref version*: the new tip, the
//! pack that supplied its objects, and the trunk version that pack was negotiated
//! against. The ref pointer is the only mutable, contended key, and it moves by
//! compare-and-swap. A push therefore costs O(1) metadata operations (plus one commit
//! index row per pushed commit), and pushes to different refs never conflict.
//!
//! The dependency rule that makes packs reusable: Pando only ever lets a client omit
//! objects reachable from trunk. So a version of any ref is fully described by
//! `its pack + trunk versions 1..=trunk_dep`. Trunk itself is a linear chain where
//! version `n` depends on `n - 1`.
//!
//! SQLite is the reference backend. Nothing here needs more than single-row CAS and
//! small multi-key transactions, so it maps onto any horizontally scalable KV store.

use crate::object::Oid;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;

/// One immutable ref version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub id: i64,
    pub refname: String,
    /// Position in this ref's history. For trunk this is the trunk sequence number.
    pub seq: u64,
    pub tip: Oid,
    /// The pack that introduced this version's objects. `None` when the push carried
    /// no new objects (for example a branch created at an existing commit).
    pub pack: Option<Oid>,
    /// Trunk versions `1..=trunk_dep` complete this version's history.
    pub trunk_dep: u64,
    /// For annotated tags, the object the tag points at.
    pub peeled: Option<Oid>,
}

/// Where a commit was first stored.
#[derive(Clone, Debug)]
pub struct CommitRow {
    pub version: i64,
    /// The first trunk version whose pack contains this commit, if any.
    pub trunk_seq: Option<u64>,
    pub parents: Vec<Oid>,
}

/// A ref change to apply atomically.
pub struct RefUpdate<'a> {
    pub refname: &'a str,
    /// The version id the caller observed. The update fails if the pointer has moved.
    pub expect: Option<i64>,
    /// `None` deletes the ref.
    pub new: Option<NewVersion<'a>>,
}

pub struct NewVersion<'a> {
    pub tip: Oid,
    pub pack: Option<Oid>,
    pub trunk_dep: u64,
    pub peeled: Option<Oid>,
    /// Commits to add to the commit index, attributed to this version.
    pub commits: &'a [(Oid, Vec<Oid>)],
}

pub struct Meta {
    conn: Connection,
    pub trunk: String,
}

const VERSION_COLS: &str = "v.id, v.ref, v.seq, v.tip, v.pack, v.trunk_dep, v.peeled";

fn oid_col(row: &Row, i: usize) -> rusqlite::Result<Oid> {
    let s: String = row.get(i)?;
    Oid::from_hex(&s).ok_or_else(|| rusqlite::Error::InvalidColumnType(i, "oid".into(), rusqlite::types::Type::Text))
}

fn opt_oid_col(row: &Row, i: usize) -> rusqlite::Result<Option<Oid>> {
    let s: Option<String> = row.get(i)?;
    Ok(s.and_then(|s| Oid::from_hex(&s)))
}

fn version_row(row: &Row) -> rusqlite::Result<Version> {
    Ok(Version {
        id: row.get(0)?,
        refname: row.get(1)?,
        seq: row.get(2)?,
        tip: oid_col(row, 3)?,
        pack: opt_oid_col(row, 4)?,
        trunk_dep: row.get(5)?,
        peeled: opt_oid_col(row, 6)?,
    })
}

impl Meta {
    pub fn open(path: impl AsRef<Path>, trunk: &str) -> rusqlite::Result<Meta> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS versions (
               id INTEGER PRIMARY KEY,
               ref TEXT NOT NULL,
               seq INTEGER NOT NULL,
               tip TEXT NOT NULL,
               pack TEXT,
               trunk_dep INTEGER NOT NULL,
               peeled TEXT,
               UNIQUE (ref, seq)
             );
             CREATE INDEX IF NOT EXISTS versions_by_tip ON versions (tip);
             CREATE TABLE IF NOT EXISTS refs (
               name TEXT PRIMARY KEY,
               version INTEGER NOT NULL REFERENCES versions (id)
             );
             CREATE TABLE IF NOT EXISTS commits (
               oid TEXT PRIMARY KEY,
               version INTEGER NOT NULL,
               trunk_seq INTEGER,
               parents TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS covers (
               level INTEGER NOT NULL,
               start INTEGER NOT NULL,
               pack TEXT,
               PRIMARY KEY (level, start)
             );",
        )?;
        Ok(Meta { conn, trunk: trunk.to_string() })
    }

    pub fn is_trunk(&self, refname: &str) -> bool {
        refname == self.trunk
    }

    /// The version a ref currently points at.
    pub fn current(&self, refname: &str) -> rusqlite::Result<Option<Version>> {
        self.conn
            .query_row(
                &format!("SELECT {VERSION_COLS} FROM refs r JOIN versions v ON v.id = r.version WHERE r.name = ?1"),
                [refname],
                version_row,
            )
            .optional()
    }

    pub fn trunk_head(&self) -> rusqlite::Result<Option<Version>> {
        self.current(&self.trunk.clone())
    }

    pub fn trunk_version(&self, seq: u64) -> rusqlite::Result<Option<Version>> {
        self.conn
            .query_row(
                &format!("SELECT {VERSION_COLS} FROM versions v WHERE v.ref = ?1 AND v.seq = ?2"),
                params![self.trunk, seq],
                version_row,
            )
            .optional()
    }

    /// Current versions of all refs starting with any of `prefixes` (all refs when
    /// empty), sorted by name.
    pub fn refs(&self, prefixes: &[String]) -> rusqlite::Result<Vec<Version>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {VERSION_COLS} FROM refs r JOIN versions v ON v.id = r.version WHERE r.name >= ?1 ORDER BY r.name"
        ))?;
        if prefixes.is_empty() {
            return stmt.query_map([""], version_row)?.collect();
        }
        let mut out = Vec::new();
        for p in prefixes {
            for v in stmt.query_map([p], version_row)? {
                let v = v?;
                if !v.refname.starts_with(p.as_str()) {
                    break;
                }
                out.push(v);
            }
        }
        out.sort_by(|a, b| a.refname.cmp(&b.refname));
        out.dedup_by(|a, b| a.refname == b.refname);
        Ok(out)
    }

    pub fn versions_with_tip(&self, tip: &Oid) -> rusqlite::Result<Vec<Version>> {
        let mut stmt =
            self.conn.prepare_cached(&format!("SELECT {VERSION_COLS} FROM versions v WHERE v.tip = ?1 ORDER BY v.id"))?;
        stmt.query_map([tip.hex()], version_row)?.collect()
    }

    pub fn version(&self, id: i64) -> rusqlite::Result<Option<Version>> {
        self.conn
            .query_row(&format!("SELECT {VERSION_COLS} FROM versions v WHERE v.id = ?1"), [id], version_row)
            .optional()
    }

    pub fn commit(&self, oid: &Oid) -> rusqlite::Result<Option<CommitRow>> {
        self.conn
            .prepare_cached("SELECT version, trunk_seq, parents FROM commits WHERE oid = ?1")?
            .query_row([oid.hex()], |row| {
                let parents: String = row.get(2)?;
                Ok(CommitRow {
                    version: row.get(0)?,
                    trunk_seq: row.get(1)?,
                    parents: parents.split_whitespace().filter_map(Oid::from_hex).collect(),
                })
            })
            .optional()
    }

    /// Apply ref updates as one transaction: for each, compare-and-swap the ref
    /// pointer, append the new version, and index its commits. All-or-nothing: if any
    /// pointer moved since the caller looked, nothing is written and the reason is
    /// returned.
    pub fn update_all(&mut self, updates: &[&RefUpdate]) -> rusqlite::Result<Result<Vec<Option<Version>>, String>> {
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut out = Vec::with_capacity(updates.len());
        for u in updates {
            let current: Option<i64> =
                tx.query_row("SELECT version FROM refs WHERE name = ?1", [u.refname], |r| r.get(0)).optional()?;
            if current != u.expect {
                return Ok(Err("ref moved during push (retry)".into()));
            }
            let Some(new) = &u.new else {
                tx.execute("DELETE FROM refs WHERE name = ?1", [u.refname])?;
                out.push(None);
                continue;
            };
            let seq: u64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM versions WHERE ref = ?1",
                [u.refname],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO versions (ref, seq, tip, pack, trunk_dep, peeled) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    u.refname,
                    seq,
                    new.tip.hex(),
                    new.pack.map(|p| p.hex()),
                    new.trunk_dep,
                    new.peeled.map(|p| p.hex())
                ],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO refs (name, version) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET version = excluded.version",
                params![u.refname, id],
            )?;
            let trunk_seq = (u.refname == self.trunk).then_some(seq);
            let mut ins = tx.prepare_cached(
                "INSERT INTO commits (oid, version, trunk_seq, parents) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (oid) DO UPDATE SET trunk_seq = COALESCE(commits.trunk_seq, excluded.trunk_seq)",
            )?;
            for (oid, parents) in new.commits {
                let parents: Vec<String> = parents.iter().map(Oid::hex).collect();
                ins.execute(params![oid.hex(), id, trunk_seq, parents.join(" ")])?;
            }
            out.push(Some(Version {
                id,
                refname: u.refname.to_string(),
                seq,
                tip: new.tip,
                pack: new.pack,
                trunk_dep: new.trunk_dep,
                peeled: new.peeled,
            }));
        }
        tx.commit()?;
        Ok(Ok(out))
    }

    /// A compaction cover: `Some(pack)` if the cover exists (`pack` is `None` when the
    /// covered versions carried no objects).
    pub fn cover(&self, level: u32, start: u64) -> rusqlite::Result<Option<Option<Oid>>> {
        self.conn
            .prepare_cached("SELECT pack FROM covers WHERE level = ?1 AND start = ?2")?
            .query_row(params![level, start], |r| opt_oid_col(r, 0))
            .optional()
    }

    pub fn last_cover_start(&self, level: u32) -> rusqlite::Result<Option<u64>> {
        self.conn.query_row("SELECT MAX(start) FROM covers WHERE level = ?1", [level], |r| r.get(0))
    }

    pub fn put_cover(&self, level: u32, start: u64, pack: Option<Oid>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO covers (level, start, pack) VALUES (?1, ?2, ?3)",
            params![level, start, pack.map(|p| p.hex())],
        )?;
        Ok(())
    }
}
