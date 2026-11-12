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
//! Access. [`Meta`] is the writer: one read-write connection that callers serialize
//! with a lock (see `server.rs`). Pushes commit through [`Meta::batch`] and
//! [`Meta::push`]: one transaction, one savepoint per push and per ref group, so one
//! fsync (`synchronous = FULL`, WAL) covers several pushes. Readers use
//! [`Readers::snapshot`]: a read-only connection inside a read transaction, so a whole
//! read (planning a fetch, an advertisement) sees one committed state, including the
//! trunk settings it loads first, while the writer keeps writing. Both share the
//! queries in [`View`]; only the writer can reach its connection to write.
//!
//! Object format. A repository's [`Format`] is recorded in `settings` when its
//! database is created and never changes; databases from before SHA-256 support have
//! none recorded and are SHA-1. Ids are stored as hex, so their width follows.
//!
//! SQLite is the reference backend. Nothing here needs more than single-row CAS and
//! small transactions, so it maps onto any horizontally scalable KV store.

use crate::object::{Format, Oid};
use crate::planner::{MAX_LEVEL, span};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// A ref change, applied by compare-and-swap (see [`Meta::push`]).
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
    /// version's trunk position.
    pub reached: &'a [Oid],
    /// On trunk: other packs whose every object the new tip now reaches (a branch
    /// that trunk fast-forwarded to or merged). They join trunk at this position too.
    pub reached_packs: &'a [Oid],
    /// On trunk: the new tip does not contain the old one, so a new epoch starts.
    pub rewrite: bool,
}

/// Read access to a repository's metadata: every query, over one connection, plus the
/// trunk settings loaded from it. Writers reach it through [`Meta`], readers through a
/// [`Snapshot`]; the connection is private, so nothing holding only a `View` can write.
pub struct View {
    conn: Connection,
    /// The repository's object format, fixed at creation.
    pub format: Format,
    /// The branch that is trunk (and HEAD).
    pub trunk: String,
    /// First chain sequence number of the current epoch.
    pub epoch_start: u64,
}

/// The writer: the one read-write connection of a repository, behind the repository's
/// metadata lock (`Repo::meta`). Reads go through [`View`], which it derefs to.
pub struct Meta(View);

impl Deref for Meta {
    type Target = View;
    fn deref(&self) -> &View {
        &self.0
    }
}

impl DerefMut for Meta {
    fn deref_mut(&mut self) -> &mut View {
        &mut self.0
    }
}

/// Read-only connections to one repository's metadata, kept for reuse.
pub struct Readers {
    path: PathBuf,
    default_trunk: String,
    format: Format,
    idle: Mutex<Vec<Connection>>,
}

/// Idle reader connections kept per repository.
const MAX_IDLE_READERS: usize = 16;

impl Readers {
    /// Readers of the database at `path`, which [`Meta::open`] has created (and found
    /// to be in `format`).
    pub fn new(path: impl Into<PathBuf>, default_trunk: &str, format: Format) -> Readers {
        Readers {
            path: path.into(),
            default_trunk: default_trunk.to_string(),
            format,
            idle: Mutex::new(Vec::new()),
        }
    }

    /// One consistent view of the metadata: a read transaction that lasts until the
    /// snapshot is dropped. It never waits for the writer, nor the writer for it, but
    /// WAL checkpoints cannot pass it, so keep it short (never across streaming a pack).
    pub fn snapshot(&self) -> rusqlite::Result<Snapshot<'_>> {
        let idle = self.idle.lock().unwrap().pop();
        let conn = match idle {
            Some(conn) => conn,
            None => {
                let conn = Connection::open_with_flags(
                    &self.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                conn.busy_timeout(std::time::Duration::from_secs(5))?;
                conn
            }
        };
        conn.execute_batch("BEGIN")?;
        let mut view = View {
            conn,
            format: self.format,
            trunk: self.default_trunk.clone(),
            epoch_start: 1,
        };
        // The first read fixes the snapshot, so the trunk settings come from it too.
        view.load_settings()?;
        Ok(Snapshot {
            view: Some(view),
            readers: self,
        })
    }
}

/// A read transaction on a reader connection (see [`Readers::snapshot`]).
pub struct Snapshot<'a> {
    view: Option<View>,
    readers: &'a Readers,
}

impl Deref for Snapshot<'_> {
    type Target = View;
    fn deref(&self) -> &View {
        self.view.as_ref().unwrap()
    }
}

impl Drop for Snapshot<'_> {
    fn drop(&mut self) {
        let view = self.view.take().unwrap();
        if view.conn.execute_batch("COMMIT").is_ok() {
            let mut idle = self.readers.idle.lock().unwrap();
            if idle.len() < MAX_IDLE_READERS {
                idle.push(view.conn);
            }
        }
    }
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
    /// first push fixes the trunk ref (see [`Meta::fix_trunk`]). A new database records
    /// `format` as the repository's object format, in the transaction that creates
    /// it; an existing one keeps the format it has.
    pub fn open(
        path: impl AsRef<Path>,
        default_trunk: &str,
        format: Format,
    ) -> rusqlite::Result<Meta> {
        let mut conn = Connection::open(path)?;
        // FULL: a committed push survives power loss, not only a crash of this process.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;",
        )?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let new: bool = tx.query_row(
            "SELECT COUNT(*) = 0 FROM sqlite_master WHERE name = 'settings'",
            [],
            |r| r.get(0),
        )?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
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
        if new {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES ('object_format', ?1)",
                [format.name()],
            )?;
        }
        migrate(&tx)?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'object_format'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        let format = match stored {
            None => Format::Sha1,
            Some(name) => Format::from_name(&name).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("unknown object format {name:?}").into(),
                )
            })?,
        };
        let mut view = View {
            conn,
            format,
            trunk: default_trunk.to_string(),
            epoch_start: 1,
        };
        view.load_settings()?;
        Ok(Meta(view))
    }

    /// Choose the trunk ref, once. Later calls keep the first choice.
    pub fn fix_trunk(&mut self, refname: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES ('trunk', ?1)",
            [refname],
        )?;
        self.load_settings()
    }

    /// Record a push in one transaction (or savepoint, inside [`Meta::batch`]): index
    /// its pack (if it stored one), then apply its ref updates in `groups`. Each group lands all-or-nothing, in a savepoint:
    /// one group per ref for a plain push, a single group for an atomic one. Returns,
    /// per group, the new versions or why its compare-and-swap failed.
    pub fn push(
        &mut self,
        pack: Option<&PackRows>,
        groups: &[Vec<&RefUpdate>],
    ) -> rusqlite::Result<Vec<Result<Vec<Option<Version>>, String>>> {
        let trunk = self.trunk.clone();
        let mut epoch = self.epoch_start;
        // A savepoint, so that it nests in a batch (see [`Meta::batch`]).
        let mut tx = self.conn.savepoint()?;
        if let Some(p) = pack {
            index_pack(&tx, p)?;
        }
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let sp = tx.savepoint()?;
            let mut e = epoch;
            match update_refs(&sp, &trunk, &mut e, group)? {
                Ok(versions) => {
                    sp.commit()?;
                    epoch = e;
                    out.push(Ok(versions));
                }
                // Dropping the savepoint rolls the group back.
                Err(why) => out.push(Err(why)),
            }
        }
        tx.commit()?;
        self.epoch_start = epoch;
        Ok(out)
    }

    /// Run `jobs` in one transaction and commit it once: one fsync for all of them.
    /// Each job runs in its own savepoint, rolled back alone if it returns false. If
    /// the commit fails, nothing is written and the error is returned.
    pub fn batch<J: FnOnce(&mut Meta) -> bool>(
        &mut self,
        jobs: impl IntoIterator<Item = J>,
    ) -> rusqlite::Result<()> {
        let result = (|| {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            for job in jobs {
                self.conn.execute_batch("SAVEPOINT job")?;
                if job(self) {
                    self.conn.execute_batch("RELEASE job")?;
                } else {
                    self.conn.execute_batch("ROLLBACK TO job; RELEASE job")?;
                    // The job may have cached settings it wrote.
                    self.load_settings()?;
                }
            }
            self.conn.execute_batch("COMMIT")
        })();
        if result.is_err() {
            if !self.conn.is_autocommit() {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
            let _ = self.load_settings();
        }
        result
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
        let old_trunk = self.trunk.clone();
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
            let (_, id) = insert(&old_trunk, old, &deps)?;
            point(&old_trunk, id)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('trunk', ?1), ('epoch_start', ?2)",
            params![branch, seq.to_string()],
        )?;
        tx.commit()?;
        self.load_settings()?;
        Ok(Ok(()))
    }

    /// The connection, for maintenance code (gc) that writes its own SQL.
    pub(crate) fn conn(&mut self) -> &mut Connection {
        &mut self.0.conn
    }
}

impl View {
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

    /// The highest trunk dependency below position `below` among all stored packs,
    /// live or dead (gc uses it to tell whether anything revivable needs a position).
    pub fn max_trunk_dep_below(&self, below: u64) -> rusqlite::Result<u64> {
        self.conn.query_row(
            "SELECT COALESCE(MAX(trunk_dep), 0) FROM packs WHERE trunk_dep < ?1",
            [below],
            |r| r.get(0),
        )
    }

    /// Whether any stored pack, live or dead, names `pack` as a dependency.
    pub fn pack_is_named(&self, pack: &Oid) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM packs WHERE instr(side_deps, ?1) > 0)",
            [pack.hex()],
            |r| r.get(0),
        )
    }

    /// Whether an object homed in `pack` is marked with a trunk position strictly
    /// between `after` and `before`.
    pub fn homes_positions_between(
        &self,
        pack: &Oid,
        after: u64,
        before: u64,
    ) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM objects WHERE pack = ?1 AND trunk_seq > ?2 AND trunk_seq < ?3)",
            params![pack.hex(), after, before],
            |r| r.get(0),
        )
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
}

impl Meta {
    /// Store what gc (or compaction) found: earlier-epoch positions `1..=old` are
    /// needed. A hint: it goes stale when a ref that needed more goes away.
    pub fn set_old_needed(&mut self, old: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('old_needed', ?1)",
            [format!("{} {old}", self.epoch_start)],
        )?;
        Ok(())
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

    /// Replace the pack of the cover `piece` (an earlier epoch's, holding live chain
    /// positions only up to `cut`) with `new`, a stored copy of its objects in `kept`
    /// (see `gc.rs`). In one transaction: `new` gets the old pack's dependencies, index
    /// rows of kept objects homed in the old pack move to it, and the cover's chain
    /// versions above `cut` are deleted. Rows of objects left out stay with the old
    /// pack, which nothing references any more: it is marked dead as of `now`, and gc
    /// collects it and them.
    ///
    /// `kept` maps each object to the first position that reaches it; a kept row
    /// still marked with a dropped position (`cut < trunk_seq < epoch_start`) gets
    /// that one instead.
    pub fn trim_cover(
        &mut self,
        piece: &Piece,
        new: &Oid,
        cut: u64,
        kept: &std::collections::HashMap<Oid, u64>,
        now: i64,
    ) -> rusqlite::Result<()> {
        let old_pack = piece.pack.expect("a trimmed cover has a pack");
        let old = old_pack.hex();
        let epoch = self.epoch_start;
        let tx = self.conn.transaction()?;
        let added = tx.execute(
            "INSERT INTO packs (hash, trunk_dep, side_deps)
             SELECT ?1, trunk_dep, side_deps FROM packs WHERE hash = ?2 AND ?1 != ?2
             ON CONFLICT (hash) DO UPDATE SET dead_since = NULL",
            params![new.hex(), old],
        )?;
        // Unknown to the metadata, the new pack would be swept as an orphan.
        if added == 0 && *new != old_pack {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        tx.execute(
            "UPDATE covers SET pack = ?3 WHERE level = ?1 AND start = ?2",
            params![piece.level, piece.start, new.hex()],
        )?;
        tx.execute(
            "UPDATE packs SET dead_since = ?2 WHERE hash = ?1 AND ?1 != ?3",
            params![old, now, new.hex()],
        )?;
        for table in ["objects", "commits"] {
            let mut stmt = tx.prepare(&format!(
                "UPDATE {table} SET pack = ?1,
                 trunk_seq = CASE WHEN trunk_seq > ?2 AND trunk_seq < ?3 THEN ?4 ELSE trunk_seq END
                 WHERE oid = ?5 AND pack = ?6"
            ))?;
            for (oid, seq) in kept {
                stmt.execute(params![new.hex(), cut, epoch, seq, oid.hex(), old])?;
            }
        }
        tx.execute(
            "DELETE FROM versions WHERE ref = ?1 AND seq > ?2 AND seq BETWEEN ?3 AND ?4",
            params![CHAIN, cut, piece.start, piece.end],
        )?;
        tx.commit()
    }
}

/// A newly stored pack, as [`Meta::push`] records it: its dependencies, and where each
/// of its objects, commits and tags lives.
pub struct PackRows<'a> {
    pub hash: Oid,
    pub deps: &'a Deps,
    /// The pack's objects, from its index.
    pub objects: &'a [crate::pack::IdxEntry],
    pub commits: &'a std::collections::HashMap<Oid, Vec<Oid>>,
    pub tags: &'a std::collections::HashMap<Oid, Oid>,
}

/// Index a pack. Objects already stored elsewhere keep their first location.
fn index_pack(conn: &Connection, p: &PackRows) -> rusqlite::Result<()> {
    let hex = p.hash.hex();
    // Pushed again after gc marked it: it is wanted after all.
    conn.execute(
        "INSERT INTO packs (hash, trunk_dep, side_deps) VALUES (?1, ?2, ?3)
         ON CONFLICT (hash) DO UPDATE SET dead_since = NULL",
        params![hex, p.deps.trunk, join_oids(&p.deps.packs)],
    )?;
    let mut obj =
        conn.prepare_cached("INSERT OR IGNORE INTO objects (oid, pack) VALUES (?1, ?2)")?;
    for e in p.objects {
        obj.execute(params![e.oid.hex(), hex])?;
    }
    let mut com = conn
        .prepare_cached("INSERT OR IGNORE INTO commits (oid, pack, parents) VALUES (?1, ?2, ?3)")?;
    for (oid, parents) in p.commits {
        com.execute(params![oid.hex(), hex, join_oids(parents)])?;
    }
    let mut tag =
        conn.prepare_cached("INSERT OR IGNORE INTO tags (oid, target) VALUES (?1, ?2)")?;
    for (oid, target) in p.tags {
        tag.execute(params![oid.hex(), target.hex()])?;
    }
    Ok(())
}

/// Apply ref updates inside an open transaction: for each, compare-and-swap the ref
/// pointer and append the new version; on trunk, mark what the new tip reaches. Stops
/// at the first pointer that moved since the caller looked and returns the reason; the
/// caller rolls back. `epoch` is the current epoch start, advanced by a rewrite.
fn update_refs(
    tx: &Connection,
    trunk: &str,
    epoch: &mut u64,
    updates: &[&RefUpdate],
) -> rusqlite::Result<Result<Vec<Option<Version>>, String>> {
    let mut out = Vec::with_capacity(updates.len());
    for u in updates {
        let current: Option<i64> = tx
            .prepare_cached("SELECT version FROM refs WHERE name = ?1")?
            .query_row([u.refname], |r| r.get(0))
            .optional()?;
        if current != u.expect {
            return Ok(Err("ref moved during push (retry)".into()));
        }
        let Some(new) = &u.new else {
            tx.execute("DELETE FROM refs WHERE name = ?1", [u.refname])?;
            out.push(None);
            continue;
        };
        let on_trunk = u.refname == trunk;
        let vref = if on_trunk { CHAIN } else { u.refname };
        let seq: u64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM versions WHERE ref = ?1",
            [vref],
            |r| r.get(0),
        )?;
        if on_trunk && new.rewrite {
            *epoch = seq;
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
                obj.execute(params![seq, pack, hex, *epoch])?;
                com.execute(params![seq, hex, *epoch])?;
            }
        }
        if on_trunk {
            for pack in new.reached_packs {
                let hex = pack.hex();
                tx.execute(
                    "UPDATE objects SET trunk_seq = ?1
                     WHERE pack = ?2 AND (trunk_seq IS NULL OR trunk_seq < ?3)",
                    params![seq, hex, *epoch],
                )?;
                tx.execute(
                    "UPDATE commits SET trunk_seq = ?1
                     WHERE pack = ?2 AND (trunk_seq IS NULL OR trunk_seq < ?3)",
                    params![seq, hex, *epoch],
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
    Ok(Ok(out))
}
