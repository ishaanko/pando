//! The metadata store (the diagrams call it "Turtle").
//!
//! Every time a ref moves, Pando appends an immutable *ref version*: the new tip, the
//! pack that supplied its objects, and what that pack depends on. The ref pointer is
//! the only mutable, contended key, and it moves by compare-and-swap, so pushes to
//! different refs never conflict.
//!
//! Dependencies. A client leaves out of its push anything the server advertised and
//! the client already has. Pando finds exactly what was left out by resolving every
//! reference that points outside the pack through the object index, and records it as:
//!
//! - `trunk`: trunk versions `1..=trunk` hold everything reachable from trunk that
//!   the pack needs. Trunk is a fast-forward-only chain, so this is one number.
//! - `packs`: specific other packs for anything not reachable from trunk.
//!
//! A version is complete given its pack plus its dependencies, transitively.
//!
//! Compaction (see `compact.rs`) merges trunk versions into covers. A merged chain
//! version keeps its row (its tip, position and dependencies still map haves and
//! wants to trunk positions) but is flagged `merged`: the cover holding its position
//! now holds its objects, and its own pack may be gone.
//!
//! Epochs. Trunk versions form one chain (stored under the internal ref [`CHAIN`],
//! whichever branch is trunk). Rewriting trunk (a force push, or making another branch
//! the default) starts a new epoch at the next sequence number. Within the current
//! epoch every version contains the ones before it; a dependency on an older trunk
//! position `k` is served as all chain versions `1..=k`, a superset that stays correct
//! across any number of rewrites.
//!
//! SQLite is the reference backend. Nothing here needs more than single-row CAS and
//! small transactions, so it maps onto any horizontally scalable KV store.

use crate::object::Oid;
use crate::planner::{MAX_LEVEL, span};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;

/// What a pack (or a version without a pack) needs besides itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deps {
    /// Trunk versions `1..=trunk`.
    pub trunk: u64,
    /// Other packs, each with its own dependencies.
    pub packs: Vec<Oid>,
}

impl Deps {
    pub fn add(&mut self, other: &Deps) {
        self.trunk = self.trunk.max(other.trunk);
        for p in &other.packs {
            if !self.packs.contains(p) {
                self.packs.push(*p);
            }
        }
    }
}

/// Ref name under which trunk versions are stored. Not a valid Git ref name, so it
/// never collides with a real ref.
pub const CHAIN: &str = "~trunk";

/// One immutable ref version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub id: i64,
    /// The ref this version belongs to (the trunk branch for chain versions).
    pub refname: String,
    /// A trunk version of the current epoch: it contains every chain version since
    /// the epoch started.
    pub on_trunk: bool,
    /// Position in this ref's history. For trunk this is the trunk sequence number.
    pub seq: u64,
    pub tip: Oid,
    /// The pack that introduced this version's objects. `None` when the push carried
    /// no new objects (for example a branch created at an existing commit).
    pub pack: Option<Oid>,
    pub deps: Deps,
    /// Every object in `pack` is reachable from `tip`, so a client holding `tip` holds
    /// the whole pack. False when one push carried several refs.
    pub pack_complete: bool,
    /// For annotated tags, the non-tag object the tag chain ends at.
    pub peeled: Option<Oid>,
    /// A chain version whose pack was merged into a compaction cover: the cover
    /// holding `seq` serves its objects, and `pack` names a pack that may be gone.
    pub merged: bool,
}

/// Where a commit was stored.
#[derive(Clone, Debug)]
pub struct CommitRow {
    /// The first pack that stored it.
    pub pack: Oid,
    /// The first trunk version of the current epoch whose tip reaches this commit.
    pub trunk_seq: Option<u64>,
    pub parents: Vec<Oid>,
}

/// Where any object is stored.
#[derive(Clone, Debug)]
pub struct ObjectRow {
    pub pack: Oid,
    /// The first trunk version whose tip reaches this object, if any. May belong to
    /// an earlier epoch (see [`Meta::in_epoch`]).
    pub trunk_seq: Option<u64>,
}

/// The stored unit holding trunk positions `start..=end`: one version that is not
/// merged (`level` 0), or a compaction cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub level: u32,
    pub start: u64,
    pub end: u64,
    /// `None` when none of the versions carried objects.
    pub pack: Option<Oid>,
    /// Non-trunk packs the versions depend on.
    pub side: Vec<Oid>,
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
    /// Dependencies of this version (the pack's, plus the tip's if outside the pack).
    pub deps: Deps,
    pub pack_complete: bool,
    pub peeled: Option<Oid>,
    /// On trunk: the objects in `pack` that the new tip reaches. They get this
    /// version's trunk position (see [`Meta::index_pack`] for the rest).
    pub reached: &'a [Oid],
    /// On trunk: other packs whose every object the new tip now reaches (a branch
    /// that trunk fast-forwarded to or merged). They join trunk at this position too.
    pub reached_packs: &'a [Oid],
    /// On trunk: the new tip does not contain the old one, so a new epoch starts.
    pub rewrite: bool,
}

pub struct Meta {
    pub(crate) conn: Connection,
    /// The branch that is trunk (and HEAD).
    pub trunk: String,
    /// First chain sequence number of the current epoch.
    pub epoch_start: u64,
}

const VERSION_COLS: &str = "v.id, v.ref, v.seq, v.tip, v.pack, v.trunk_dep, v.side_deps, v.pack_complete, \
     v.peeled, v.merged";

fn oid_col(row: &Row, i: usize) -> rusqlite::Result<Oid> {
    let s: String = row.get(i)?;
    Oid::from_hex(&s).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(i, "oid".into(), rusqlite::types::Type::Text)
    })
}

fn opt_oid_col(row: &Row, i: usize) -> rusqlite::Result<Option<Oid>> {
    let s: Option<String> = row.get(i)?;
    Ok(s.and_then(|s| Oid::from_hex(&s)))
}

fn oids_col(row: &Row, i: usize) -> rusqlite::Result<Vec<Oid>> {
    let s: String = row.get(i)?;
    Ok(s.split_whitespace().filter_map(Oid::from_hex).collect())
}

fn join_oids(oids: &[Oid]) -> String {
    oids.iter().map(Oid::hex).collect::<Vec<_>>().join(" ")
}

fn version_row(row: &Row) -> rusqlite::Result<Version> {
    Ok(Version {
        id: row.get(0)?,
        refname: row.get(1)?,
        on_trunk: false, // set by Meta::finish
        seq: row.get(2)?,
        tip: oid_col(row, 3)?,
        pack: opt_oid_col(row, 4)?,
        deps: Deps {
            trunk: row.get(5)?,
            packs: oids_col(row, 6)?,
        },
        pack_complete: row.get(7)?,
        peeled: opt_oid_col(row, 8)?,
        merged: row.get(9)?,
    })
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |r| r.get(0),
    )
}

/// Bring databases written by older versions up to date.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    if !has_column(conn, "packs", "dead_since")? {
        conn.execute("ALTER TABLE packs ADD COLUMN dead_since INTEGER", [])?;
    }
    // Schema 3 (merging compaction). Covers written before it overlap at every level
    // and merged nothing: they stay as plain rows, which compaction adopts when their
    // range is due and gc reclaims otherwise.
    if !has_column(conn, "versions", "merged")? {
        conn.execute(
            "ALTER TABLE versions ADD COLUMN merged INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(conn, "covers", "pending")? {
        conn.execute(
            "ALTER TABLE covers ADD COLUMN pending INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    let schema: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if schema.is_none() {
        // Schema 1 stored trunk versions under the trunk branch's own name.
        conn.execute(
            "UPDATE versions SET ref = ?1 WHERE ref = (SELECT value FROM settings WHERE key = 'trunk')",
            [CHAIN],
        )?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('schema', '3')",
        [],
    )?;
    Ok(())
}

impl Meta {
    /// Open (or create) a repository's metadata. `default_trunk` applies until the
    /// first push fixes the trunk ref (see [`Meta::fix_trunk`]).
    pub fn open(path: impl AsRef<Path>, default_trunk: &str) -> rusqlite::Result<Meta> {
        let conn = Connection::open(path)?;
        // FULL: a committed push survives power loss, not only a crash of this process.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS versions (
               id INTEGER PRIMARY KEY,
               ref TEXT NOT NULL,
               seq INTEGER NOT NULL,
               tip TEXT NOT NULL,
               pack TEXT,
               trunk_dep INTEGER NOT NULL,
               side_deps TEXT NOT NULL,
               pack_complete INTEGER NOT NULL,
               peeled TEXT,
               merged INTEGER NOT NULL DEFAULT 0,
               UNIQUE (ref, seq)
             );
             CREATE INDEX IF NOT EXISTS versions_by_tip ON versions (tip);
             CREATE INDEX IF NOT EXISTS versions_by_pack ON versions (pack);
             CREATE TABLE IF NOT EXISTS refs (
               name TEXT PRIMARY KEY,
               version INTEGER NOT NULL REFERENCES versions (id)
             );
             CREATE TABLE IF NOT EXISTS packs (
               hash TEXT PRIMARY KEY,
               trunk_dep INTEGER NOT NULL,
               side_deps TEXT NOT NULL,
               dead_since INTEGER
             );
             CREATE TABLE IF NOT EXISTS commits (
               oid TEXT PRIMARY KEY,
               pack TEXT NOT NULL,
               trunk_seq INTEGER,
               parents TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS objects (
               oid TEXT PRIMARY KEY,
               pack TEXT NOT NULL,
               trunk_seq INTEGER
             );
             CREATE TABLE IF NOT EXISTS tags (oid TEXT PRIMARY KEY, target TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS objects_by_pack ON objects (pack);
             CREATE INDEX IF NOT EXISTS commits_by_pack ON commits (pack);
             CREATE TABLE IF NOT EXISTS covers (
               level INTEGER NOT NULL,
               start INTEGER NOT NULL,
               pack TEXT,
               side_deps TEXT NOT NULL,
               pending INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (level, start)
             );",
        )?;
        migrate(&conn)?;
        let mut meta = Meta {
            conn,
            trunk: default_trunk.to_string(),
            epoch_start: 1,
        };
        meta.load_settings()?;
        Ok(meta)
    }

    fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
    }

    fn load_settings(&mut self) -> rusqlite::Result<()> {
        if let Some(t) = self.setting("trunk")? {
            self.trunk = t;
        }
        self.epoch_start = self
            .setting("epoch_start")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        Ok(())
    }

    /// Fill in what a stored version row cannot know by itself.
    fn finish(&self, mut v: Version) -> Version {
        if v.refname == CHAIN {
            v.on_trunk = v.seq >= self.epoch_start;
            v.refname = self.trunk.clone();
        }
        v
    }

    /// Whether a trunk position belongs to the current epoch.
    pub fn in_epoch(&self, seq: u64) -> bool {
        seq >= self.epoch_start
    }

    /// Whether the trunk ref has been chosen yet.
    pub fn trunk_fixed(&self) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM settings WHERE key = 'trunk'", [], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Choose the trunk ref, once. Later calls keep the first choice.
    pub fn fix_trunk(&mut self, refname: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES ('trunk', ?1)",
            [refname],
        )?;
        self.load_settings()
    }

    pub fn is_trunk(&self, refname: &str) -> bool {
        refname == self.trunk
    }

    /// The version a ref currently points at.
    pub fn current(&self, refname: &str) -> rusqlite::Result<Option<Version>> {
        Ok(self
            .conn
            .prepare_cached(&format!(
                "SELECT {VERSION_COLS} FROM refs r JOIN versions v ON v.id = r.version WHERE r.name = ?1"
            ))?
            .query_row([refname], version_row)
            .optional()?
            .map(|v| self.finish(v)))
    }

    pub fn trunk_head(&self) -> rusqlite::Result<Option<Version>> {
        self.current(&self.trunk.clone())
    }

    /// The chain version at `seq`, of any epoch.
    pub fn trunk_version(&self, seq: u64) -> rusqlite::Result<Option<Version>> {
        Ok(self
            .conn
            .prepare_cached(&format!(
                "SELECT {VERSION_COLS} FROM versions v WHERE v.ref = ?1 AND v.seq = ?2"
            ))?
            .query_row(params![CHAIN, seq], version_row)
            .optional()?
            .map(|v| self.finish(v)))
    }

    /// Current versions of all refs starting with any of `prefixes` (all refs when
    /// empty), sorted by name.
    pub fn refs(&self, prefixes: &[String]) -> rusqlite::Result<Vec<Version>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {VERSION_COLS} FROM refs r JOIN versions v ON v.id = r.version WHERE r.name >= ?1 ORDER BY r.name"
        ))?;
        if prefixes.is_empty() {
            return stmt
                .query_map([""], version_row)?
                .map(|v| v.map(|v| self.finish(v)))
                .collect();
        }
        let mut out = Vec::new();
        for p in prefixes {
            for v in stmt.query_map([p], version_row)? {
                let v = self.finish(v?);
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

    /// Whether creating `refname` would clash with an existing ref as a directory or
    /// a file (`a/b` and `a/b/c` cannot both exist).
    pub fn ref_path_conflict(&self, refname: &str) -> rusqlite::Result<Option<String>> {
        let child: Option<String> = self
            .conn
            .prepare_cached("SELECT name FROM refs WHERE name > ?1 AND name < ?2 LIMIT 1")?
            .query_row(params![format!("{refname}/"), format!("{refname}0")], |r| {
                r.get(0)
            })
            .optional()?;
        if child.is_some() {
            return Ok(child);
        }
        let mut parent = refname;
        while let Some((dir, _)) = parent.rsplit_once('/') {
            if self.current(dir)?.is_some() {
                return Ok(Some(dir.to_string()));
            }
            parent = dir;
        }
        Ok(None)
    }

    /// Complete versions (every pack object reachable from the tip) stored in `pack`.
    pub fn complete_versions_in_pack(&self, pack: &Oid) -> rusqlite::Result<Vec<Version>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {VERSION_COLS} FROM versions v WHERE v.pack = ?1 AND v.pack_complete"
        ))?;
        stmt.query_map([pack.hex()], version_row)?
            .map(|v| v.map(|v| self.finish(v)))
            .collect()
    }

    pub fn versions_with_tip(&self, tip: &Oid) -> rusqlite::Result<Vec<Version>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {VERSION_COLS} FROM versions v WHERE v.tip = ?1 ORDER BY v.id"
        ))?;
        stmt.query_map([tip.hex()], version_row)?
            .map(|v| v.map(|v| self.finish(v)))
            .collect()
    }

    pub fn version(&self, id: i64) -> rusqlite::Result<Option<Version>> {
        Ok(self
            .conn
            .prepare_cached(&format!(
                "SELECT {VERSION_COLS} FROM versions v WHERE v.id = ?1"
            ))?
            .query_row([id], version_row)
            .optional()?
            .map(|v| self.finish(v)))
    }

    pub fn commit(&self, oid: &Oid) -> rusqlite::Result<Option<CommitRow>> {
        self.conn
            .prepare_cached("SELECT pack, trunk_seq, parents FROM commits WHERE oid = ?1")?
            .query_row([oid.hex()], |row| {
                let seq: Option<u64> = row.get(1)?;
                Ok(CommitRow {
                    pack: oid_col(row, 0)?,
                    trunk_seq: seq.filter(|s| self.in_epoch(*s)),
                    parents: oids_col(row, 2)?,
                })
            })
            .optional()
    }

    pub fn object(&self, oid: &Oid) -> rusqlite::Result<Option<ObjectRow>> {
        self.conn
            .prepare_cached("SELECT pack, trunk_seq FROM objects WHERE oid = ?1")?
            .query_row([oid.hex()], |row| {
                Ok(ObjectRow {
                    pack: oid_col(row, 0)?,
                    trunk_seq: row.get(1)?,
                })
            })
            .optional()
    }

    /// The object an annotated tag points at, for any stored tag.
    pub fn tag_target(&self, oid: &Oid) -> rusqlite::Result<Option<Oid>> {
        self.conn
            .prepare_cached("SELECT target FROM tags WHERE oid = ?1")?
            .query_row([oid.hex()], |r| oid_col(r, 0))
            .optional()
    }

    /// Follow annotated tags (stored anywhere) to the first non-tag object. `None` if
    /// `oid` is not a tag.
    pub fn peel(&self, oid: &Oid) -> rusqlite::Result<Option<Oid>> {
        let mut cur = *oid;
        let mut peeled = None;
        for _ in 0..64 {
            match self.tag_target(&cur)? {
                Some(t) => {
                    cur = t;
                    peeled = Some(t);
                }
                None => break,
            }
        }
        Ok(peeled)
    }

    /// Dependencies recorded for a stored pack.
    pub fn pack_deps(&self, pack: &Oid) -> rusqlite::Result<Option<Deps>> {
        self.conn
            .prepare_cached("SELECT trunk_dep, side_deps FROM packs WHERE hash = ?1")?
            .query_row([pack.hex()], |row| {
                Ok(Deps {
                    trunk: row.get(0)?,
                    packs: oids_col(row, 1)?,
                })
            })
            .optional()
    }

    /// Record a newly stored pack: its dependencies, and where each of its objects and
    /// commits lives. Objects already stored elsewhere keep their first location.
    pub fn index_pack(
        &mut self,
        pack: &Oid,
        deps: &Deps,
        objects: impl Iterator<Item = Oid>,
        commits: &std::collections::HashMap<Oid, Vec<Oid>>,
        tags: &std::collections::HashMap<Oid, Oid>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        let hex = pack.hex();
        // Pushed again after gc marked it: it is wanted after all.
        tx.execute(
            "INSERT INTO packs (hash, trunk_dep, side_deps) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO UPDATE SET dead_since = NULL",
            params![hex, deps.trunk, join_oids(&deps.packs)],
        )?;
        {
            let mut obj =
                tx.prepare_cached("INSERT OR IGNORE INTO objects (oid, pack) VALUES (?1, ?2)")?;
            for oid in objects {
                obj.execute(params![oid.hex(), hex])?;
            }
            let mut com = tx.prepare_cached(
                "INSERT OR IGNORE INTO commits (oid, pack, parents) VALUES (?1, ?2, ?3)",
            )?;
            for (oid, parents) in commits {
                com.execute(params![oid.hex(), hex, join_oids(parents)])?;
            }
            let mut tag =
                tx.prepare_cached("INSERT OR IGNORE INTO tags (oid, target) VALUES (?1, ?2)")?;
            for (oid, target) in tags {
                tag.execute(params![oid.hex(), target.hex()])?;
            }
        }
        tx.commit()
    }

    /// Apply ref updates as one transaction: for each, compare-and-swap the ref
    /// pointer and append the new version; on trunk, mark what the new tip reaches.
    /// All-or-nothing: if any pointer moved since the caller looked, nothing is
    /// written and the reason is returned.
    pub fn update_all(
        &mut self,
        updates: &[&RefUpdate],
    ) -> rusqlite::Result<Result<Vec<Option<Version>>, String>> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut out = Vec::with_capacity(updates.len());
        let mut epoch = self.epoch_start;
        for u in updates {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT version FROM refs WHERE name = ?1",
                    [u.refname],
                    |r| r.get(0),
                )
                .optional()?;
            if current != u.expect {
                return Ok(Err("ref moved during push (retry)".into()));
            }
            let Some(new) = &u.new else {
                tx.execute("DELETE FROM refs WHERE name = ?1", [u.refname])?;
                out.push(None);
                continue;
            };
            let on_trunk = u.refname == self.trunk;
            let vref = if on_trunk { CHAIN } else { u.refname };
            let seq: u64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM versions WHERE ref = ?1",
                [vref],
                |r| r.get(0),
            )?;
            if on_trunk && new.rewrite {
                epoch = seq;
                tx.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('epoch_start', ?1)",
                    [seq.to_string()],
                )?;
            }
            tx.execute(
                "INSERT INTO versions (ref, seq, tip, pack, trunk_dep, side_deps, pack_complete, peeled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    vref,
                    seq,
                    new.tip.hex(),
                    new.pack.map(|p| p.hex()),
                    new.deps.trunk,
                    join_oids(&new.deps.packs),
                    new.pack_complete,
                    new.peeled.map(|p| p.hex())
                ],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO refs (name, version) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET version = excluded.version",
                params![u.refname, id],
            )?;
            if on_trunk && let Some(pack) = new.pack {
                // Trunk now reaches these: they get a trunk position (and move to this
                // pack, which holds them) unless an earlier version of this epoch had
                // them.
                let pack = pack.hex();
                let mut obj = tx.prepare_cached(
                    "UPDATE objects SET trunk_seq = ?1, pack = ?2
                     WHERE oid = ?3 AND (trunk_seq IS NULL OR trunk_seq < ?4)",
                )?;
                let mut com = tx.prepare_cached(
                    "UPDATE commits SET trunk_seq = ?1
                     WHERE oid = ?2 AND (trunk_seq IS NULL OR trunk_seq < ?3)",
                )?;
                for oid in new.reached {
                    let hex = oid.hex();
                    obj.execute(params![seq, pack, hex, epoch])?;
                    com.execute(params![seq, hex, epoch])?;
                }
            }
            if on_trunk {
                for pack in new.reached_packs {
                    let hex = pack.hex();
                    tx.execute(
                        "UPDATE objects SET trunk_seq = ?1
                         WHERE pack = ?2 AND (trunk_seq IS NULL OR trunk_seq < ?3)",
                        params![seq, hex, epoch],
                    )?;
                    tx.execute(
                        "UPDATE commits SET trunk_seq = ?1
                         WHERE pack = ?2 AND (trunk_seq IS NULL OR trunk_seq < ?3)",
                        params![seq, hex, epoch],
                    )?;
                }
            }
            out.push(Some(Version {
                id,
                refname: u.refname.to_string(),
                on_trunk,
                seq,
                tip: new.tip,
                pack: new.pack,
                deps: new.deps.clone(),
                pack_complete: new.pack_complete,
                peeled: new.peeled,
                merged: false,
            }));
        }
        tx.commit()?;
        self.epoch_start = epoch;
        Ok(Ok(out))
    }

    /// Make another branch trunk (and HEAD). This starts a new epoch whose first
    /// version is the branch's tip; the old trunk branch continues as a normal branch.
    pub fn set_trunk(&mut self, branch: &str) -> rusqlite::Result<Result<(), String>> {
        if branch == self.trunk {
            return Ok(Ok(()));
        }
        let old = self.trunk_head()?;
        if !branch.starts_with("refs/heads/") {
            return Ok(Err(format!("{branch} is not a branch")));
        }
        let Some(new) = self.current(branch)? else {
            if old.is_none() {
                // Nothing pushed yet: just choose the name.
                self.conn.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('trunk', ?1)",
                    [branch],
                )?;
                self.load_settings()?;
                return Ok(Ok(()));
            }
            return Ok(Err(format!("no such branch {branch}")));
        };
        if new.peeled.is_some() {
            return Ok(Err(format!("{branch} does not point at a commit")));
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let insert = |vref: &str, v: &Version, deps: &Deps| -> rusqlite::Result<(u64, i64)> {
            let seq: u64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM versions WHERE ref = ?1",
                [vref],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO versions (ref, seq, tip, pack, trunk_dep, side_deps, pack_complete, peeled)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0, ?6)",
                params![
                    vref,
                    seq,
                    v.tip.hex(),
                    deps.trunk,
                    join_oids(&deps.packs),
                    v.peeled.map(|p| p.hex())
                ],
            )?;
            Ok((seq, tx.last_insert_rowid()))
        };
        let point = |name: &str, id: i64| {
            tx.execute(
                "INSERT INTO refs (name, version) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET version = excluded.version",
                params![name, id],
            )
        };
        // The new chain version holds whatever the branch's version held.
        let mut deps = new.deps.clone();
        deps.packs.extend(new.pack);
        let (seq, id) = insert(CHAIN, &new, &deps)?;
        point(branch, id)?;
        if let Some(old) = &old {
            let deps = Deps {
                trunk: old.seq,
                packs: vec![],
            };
            let (_, id) = insert(&self.trunk, old, &deps)?;
            point(&self.trunk, id)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('trunk', ?1), ('epoch_start', ?2)",
            params![branch, seq.to_string()],
        )?;
        tx.commit()?;
        self.load_settings()?;
        Ok(Ok(()))
    }

    /// The highest earlier-epoch position anything still depends on, as last stored
    /// for the current epoch (see [`Meta::set_old_needed`]).
    pub fn old_needed(&self) -> rusqlite::Result<Option<u64>> {
        Ok(self.setting("old_needed")?.and_then(|s| {
            let (epoch, old) = s.split_once(' ')?;
            (epoch.parse() == Ok(self.epoch_start))
                .then(|| old.parse().ok())
                .flatten()
        }))
    }

    /// Store what gc found: earlier-epoch positions `1..=old` are needed.
    pub fn set_old_needed(&mut self, old: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('old_needed', ?1)",
            [format!("{} {old}", self.epoch_start)],
        )?;
        Ok(())
    }

    /// The first chain position after `seq` that still has a version row.
    pub fn next_trunk_seq(&self, seq: u64) -> rusqlite::Result<Option<u64>> {
        self.conn
            .prepare_cached("SELECT MIN(seq) FROM versions WHERE ref = ?1 AND seq > ?2")?
            .query_row(params![CHAIN, seq], |r| r.get(0))
    }

    /// The highest trunk dependency below `below` among chain versions `from..=to`.
    pub fn trunk_dep_below(&self, from: u64, to: u64, below: u64) -> rusqlite::Result<u64> {
        self.conn
            .prepare_cached(
                "SELECT COALESCE(MAX(trunk_dep), 0) FROM versions
                 WHERE ref = ?1 AND seq BETWEEN ?2 AND ?3 AND trunk_dep < ?4",
            )?
            .query_row(params![CHAIN, from, to, below], |r| r.get(0))
    }

    /// The finished cover holding a merged position: the lowest-level one containing
    /// it (compaction deletes covers below a cover it records, so this is the piece).
    pub fn cover_containing(&self, seq: u64) -> rusqlite::Result<Option<Piece>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT pack, side_deps FROM covers WHERE level = ?1 AND start = ?2 AND NOT pending",
        )?;
        for level in 1..=MAX_LEVEL {
            let s = span(level);
            let start = (seq - 1) / s * s + 1;
            let found = stmt
                .query_row(params![level, start], |r| {
                    Ok((opt_oid_col(r, 0)?, oids_col(r, 1)?))
                })
                .optional()?;
            if let Some((pack, side)) = found {
                return Ok(Some(Piece {
                    level,
                    start,
                    end: start + s - 1,
                    pack,
                    side,
                }));
            }
        }
        Ok(None)
    }

    /// The cover row at `(level, start)`, if any: its pack and whether it is pending.
    pub fn cover_row(
        &self,
        level: u32,
        start: u64,
    ) -> rusqlite::Result<Option<(Option<Oid>, bool)>> {
        self.conn
            .prepare_cached("SELECT pack, pending FROM covers WHERE level = ?1 AND start = ?2")?
            .query_row(params![level, start], |r| {
                Ok((opt_oid_col(r, 0)?, r.get(1)?))
            })
            .optional()
    }

    /// Covers recorded but not finished (see [`Meta::record_cover`]).
    pub fn pending_covers(&self) -> rusqlite::Result<Vec<Piece>> {
        self.conn
            .prepare(
                "SELECT level, start, pack, side_deps FROM covers WHERE pending ORDER BY level",
            )?
            .query_map([], |r| {
                let (level, start): (u32, u64) = (r.get(0)?, r.get(1)?);
                Ok(Piece {
                    level,
                    start,
                    end: start + span(level) - 1,
                    pack: opt_oid_col(r, 2)?,
                    side: oids_col(r, 3)?,
                })
            })?
            .collect()
    }

    /// First step of recording a cover: store it as pending, with its pack's
    /// dependencies. A pending cover changes nothing the planner sees, and gc keeps
    /// its pack, so index rows can move to it before [`Meta::finish_cover`].
    pub fn record_cover(&mut self, piece: &Piece, trunk_dep: u64) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        let side = join_oids(&piece.side);
        if let Some(pack) = piece.pack {
            // The pack may exist already (a single-part cover reuses its part, or a
            // cover rebuilt after gc marked it). Either set of dependencies is enough;
            // keep the smaller trunk range.
            tx.execute(
                "INSERT INTO packs (hash, trunk_dep, side_deps) VALUES (?1, ?2, ?3)
                 ON CONFLICT (hash) DO UPDATE
                 SET dead_since = NULL, trunk_dep = MIN(trunk_dep, excluded.trunk_dep)",
                params![pack.hex(), trunk_dep, side],
            )?;
        }
        tx.execute(
            "INSERT INTO covers (level, start, pack, side_deps, pending) VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT (level, start) DO UPDATE
             SET pack = excluded.pack, side_deps = excluded.side_deps, pending = 1",
            params![piece.level, piece.start, piece.pack.map(|p| p.hex()), side],
        )?;
        tx.commit()
    }

    /// Move up to `limit` index rows of trunk objects and commits homed in `from` to
    /// `to`, which holds the same objects. Returns the most rows moved in either table. Rows of
    /// objects off trunk stay: `from` is still their smallest home, and whatever
    /// depends on them names it (see `compact.rs`).
    pub fn repoint(&mut self, from: &Oid, to: &Oid, limit: usize) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut moved = 0;
        for table in ["objects", "commits"] {
            moved = moved.max(tx.execute(
                &format!(
                    "UPDATE {table} SET pack = ?2 WHERE rowid IN
                     (SELECT rowid FROM {table} WHERE pack = ?1 AND trunk_seq IS NOT NULL LIMIT ?3)"
                ),
                params![from.hex(), to.hex(), limit],
            )?);
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Second step: the pending cover becomes the piece for its range. Its members
    /// (`members`: the packs of the pieces it replaces) stop being pieces in the same
    /// transaction: covers below it are deleted, versions in it are marked merged, and
    /// any trunk index row still homed in a member moves to the cover.
    pub fn finish_cover(&mut self, piece: &Piece, members: &[Oid]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        if let Some(pack) = piece.pack {
            for m in members.iter().filter(|m| **m != pack) {
                for table in ["objects", "commits"] {
                    tx.execute(
                        &format!(
                            "UPDATE {table} SET pack = ?2 WHERE pack = ?1 AND trunk_seq IS NOT NULL"
                        ),
                        params![m.hex(), pack.hex()],
                    )?;
                }
            }
        }
        tx.execute(
            "UPDATE versions SET merged = 1 WHERE ref = ?1 AND seq BETWEEN ?2 AND ?3",
            params![CHAIN, piece.start, piece.end],
        )?;
        tx.execute(
            "DELETE FROM covers WHERE level < ?1 AND start BETWEEN ?2 AND ?3",
            params![piece.level, piece.start, piece.end],
        )?;
        tx.execute(
            "UPDATE covers SET pending = 0 WHERE level = ?1 AND start = ?2",
            params![piece.level, piece.start],
        )?;
        tx.commit()
    }
}
