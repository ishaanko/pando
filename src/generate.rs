//! Fetches answered with a freshly written pack: shallow fetches (`--depth`,
//! `--deepen`, `--shallow-since`, `--shallow-exclude`, any fetch into a shallow clone)
//! and partial clones (`--filter=blob:none`, `blob:limit=<n>`, `tree:<depth>`, and the
//! on-demand fetches of missing objects that follow).
//!
//! Stored packs carry full history with every object, so they cannot answer a request
//! that cuts history or leaves objects out. For these requests Pando does what a
//! classic Git server does: walk commits from the wants, decide where history stops,
//! and write a fresh pack of exactly those commits with their trees, minus what the
//! client already has or filtered out. Objects stored whole are copied without
//! recompressing.

use crate::object::{
    GITLINK_MODE, Kind, Oid, commit_parents, commit_time, commit_tree, tree_entries,
};
use crate::odb::Odb;
use crate::pack::{HashingWriter, encode_type_size};
use crate::planner;
use crate::server::Repo;
use crate::upload::FetchArgs;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;

/// How far history should reach, from a fetch request's `deepen*` arguments.
#[derive(Debug, Default)]
pub struct Deepen {
    depth: Option<u32>,
    relative: bool,
    since: Option<i64>,
    not: Vec<String>,
}

impl Deepen {
    pub fn parse(args: &[String]) -> Option<Deepen> {
        let mut d = Deepen::default();
        let mut any = false;
        for a in args {
            if let Some(n) = a.strip_prefix("deepen ") {
                d.depth = n.parse().ok();
                any = true;
            } else if let Some(t) = a.strip_prefix("deepen-since ") {
                d.since = t.parse().ok();
                any = true;
            } else if let Some(r) = a.strip_prefix("deepen-not ") {
                d.not.push(r.to_string());
                any = true;
            } else if a == "deepen-relative" {
                d.relative = true;
            }
        }
        any.then_some(d)
    }
}

/// Which objects a partial clone leaves out (`--filter=<spec>`): every part of a
/// `combine:` filter must keep an object for it to be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter(Vec<Atom>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Atom {
    /// `blob:none`
    BlobNone,
    /// `blob:limit=<n>`: blobs of at least `n` bytes are left out.
    BlobLimit(u64),
    /// `tree:<depth>`: trees and blobs at or below `depth` (the root tree is 0).
    Tree(u64),
    /// `object:type=<type>`: only objects of this type.
    Type(Kind),
}

impl Filter {
    pub fn parse(spec: &str) -> anyhow::Result<Filter> {
        match spec.strip_prefix("combine:") {
            Some(parts) => parts
                .split('+')
                .map(|p| Atom::parse(&percent_decode(p)))
                .collect::<anyhow::Result<_>>()
                .map(Filter),
            None => Ok(Filter(vec![Atom::parse(spec)?])),
        }
    }

    /// Whether a tree at `depth` is walked into.
    fn enters(&self, depth: u64) -> bool {
        self.0
            .iter()
            .all(|a| !matches!(a, Atom::Tree(max) if depth >= *max))
    }

    fn keeps_tree(&self, depth: u64) -> bool {
        self.enters(depth) && self.keeps_kind(Kind::Tree)
    }

    /// Commits and tags reached by the walk.
    fn keeps_kind(&self, kind: Kind) -> bool {
        self.0
            .iter()
            .all(|a| !matches!(a, Atom::Type(t) if *t != kind))
    }

    fn keeps_blob(
        &self,
        depth: u64,
        size: impl Fn() -> anyhow::Result<u64>,
    ) -> anyhow::Result<bool> {
        for a in &self.0 {
            let keep = match *a {
                Atom::BlobNone => false,
                Atom::BlobLimit(max) => size()? < max,
                Atom::Tree(max) => depth < max,
                Atom::Type(t) => t == Kind::Blob,
            };
            if !keep {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl Atom {
    fn parse(spec: &str) -> anyhow::Result<Atom> {
        let unsupported = || anyhow::anyhow!("filter '{spec}' not supported");
        if spec == "blob:none" {
            return Ok(Atom::BlobNone);
        }
        if let Some(n) = spec.strip_prefix("blob:limit=") {
            let (digits, unit) = match n.find(|c: char| !c.is_ascii_digit()) {
                Some(i) => n.split_at(i),
                None => (n, ""),
            };
            let scale = match unit.to_ascii_lowercase().as_str() {
                "" => 1,
                "k" => 1 << 10,
                "m" => 1 << 20,
                "g" => 1 << 30,
                _ => return Err(unsupported()),
            };
            let n: u64 = digits.parse().map_err(|_| unsupported())?;
            return Ok(Atom::BlobLimit(n.saturating_mul(scale)));
        }
        if let Some(d) = spec.strip_prefix("tree:") {
            return Ok(Atom::Tree(d.parse().map_err(|_| unsupported())?));
        }
        if let Some(t) = spec.strip_prefix("object:type=") {
            return Ok(Atom::Type(match t {
                "blob" => Kind::Blob,
                "tree" => Kind::Tree,
                "commit" => Kind::Commit,
                "tag" => Kind::Tag,
                _ => return Err(unsupported()),
            }));
        }
        Err(unsupported())
    }
}

/// Undo the `%XX` escapes git uses inside `combine:` filters.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a fetch must be answered with a fresh pack rather than stored ones.
pub fn needed(args: &FetchArgs) -> bool {
    args.deepen.is_some() || !args.shallow.is_empty() || args.filter.is_some()
}

/// A fresh pack, planned but not yet written.
pub struct Generated {
    odb: Odb,
    walk: Walk,
    filter: Option<Filter>,
}

impl Generated {
    /// Plan the pack: load the stored packs that hold the wants' history and walk it.
    pub fn prepare(repo: &Repo, args: &FetchArgs) -> anyhow::Result<Generated> {
        let filter = args.filter.as_deref().map(Filter::parse).transpose()?;
        let (odb, not_tips) = {
            let meta = repo.meta.lock().unwrap();
            let mut not_tips = Vec::new();
            for name in args.deepen.iter().flat_map(|d| &d.not) {
                // Resolve a short name the way git does.
                let candidates = [
                    name.clone(),
                    format!("refs/{name}"),
                    format!("refs/tags/{name}"),
                    format!("refs/heads/{name}"),
                    format!("refs/remotes/{name}"),
                    format!("refs/remotes/{name}/HEAD"),
                ];
                let mut found = None;
                for full in &candidates {
                    if let Some(v) = meta.current(full)? {
                        found = Some(v);
                        break;
                    }
                }
                match found {
                    Some(v) => not_tips.push(v.peeled.unwrap_or(v.tip)),
                    None => anyhow::bail!("deepen-not: unknown ref {name}"),
                }
            }
            (open_odb(repo, &meta, args)?, not_tips)
        };
        let walk = Walk::new(&odb, args, &not_tips)?;
        tracing::info!(
            repo = %repo.name,
            commits = walk.send.len(),
            shallow = walk.shallow.len(),
            ?filter,
            "generated fetch"
        );
        Ok(Generated { odb, walk, filter })
    }

    /// Commits that become (or stay) the client's shallow boundary.
    pub fn shallow(&self) -> &[Oid] {
        &self.walk.shallow
    }

    /// Commits that stop being shallow on the client.
    pub fn unshallow(&self) -> &[Oid] {
        &self.walk.unshallow
    }

    /// Write the pack.
    pub fn write(&self, out: &mut dyn Write) -> anyhow::Result<()> {
        let objects = self.walk.objects(&self.odb, self.filter.as_ref())?;
        write_pack(&self.odb, &objects, out)
    }
}

/// The stored packs holding everything the wants need. A fetch of nothing but blobs
/// (a partial clone filling in what it left out) reads just the packs holding them.
fn open_odb(repo: &Repo, meta: &crate::meta::Meta, args: &FetchArgs) -> anyhow::Result<Odb> {
    if args.deepen.is_none() && args.shallow.is_empty() {
        let mut direct = Vec::new();
        for want in &args.wants {
            match meta.object(want)? {
                Some(row) if !direct.contains(&row.pack) => direct.push(row.pack),
                Some(_) => {}
                None => anyhow::bail!("not our ref {want}"),
            }
        }
        let odb = Odb::open(&repo.store, &direct)?;
        let mut all_blobs = true;
        for want in &args.wants {
            all_blobs &= matches!(odb.read(want)?, Some((Kind::Blob, _)));
        }
        if all_blobs {
            return Ok(odb);
        }
    }
    let plan = planner::plan(meta, &args.wants, &[]).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Odb::open(&repo.store, &plan.packs)?)
}

/// Which commits to send and where the new shallow boundary is.
struct Walk {
    /// Commits to send, in walk order.
    send: Vec<Oid>,
    /// Commits the client has that border the sent ones; their trees are not resent.
    had_border: Vec<Oid>,
    /// Tags the client asked for.
    tags: Vec<Oid>,
    /// What the client asked for by name (always sent, whatever the filter).
    wanted: HashSet<Oid>,
    /// Trees and blobs that wanted tags point at directly; sent with no history.
    tagged: Vec<(Kind, Oid)>,
    shallow: Vec<Oid>,
    unshallow: Vec<Oid>,
}

impl Walk {
    fn new(odb: &Odb, args: &FetchArgs, not_tips: &[Oid]) -> anyhow::Result<Walk> {
        let deepen = args.deepen.as_ref();
        let have: HashSet<Oid> = args.haves.iter().copied().collect();
        let client_shallow: HashSet<Oid> = args.shallow.iter().copied().collect();
        let excluded = reachable_commits(odb, not_tips)?;

        let mut tags = Vec::new();
        let mut starts = Vec::new();
        let mut tagged = Vec::new();
        'wants: for want in &args.wants {
            let mut oid = *want;
            loop {
                let (kind, data) = odb
                    .read(&oid)?
                    .ok_or_else(|| anyhow::anyhow!("not our ref {want}"))?;
                match kind {
                    Kind::Tag => {
                        tags.push(oid);
                        oid = crate::object::tag_target(&data)
                            .ok_or_else(|| anyhow::anyhow!("bad tag {oid}"))?;
                    }
                    Kind::Commit => break,
                    kind => {
                        tagged.push((kind, oid));
                        continue 'wants;
                    }
                }
            }
            starts.push(oid);
        }

        // Depth counts from the wants (depth 1). With deepen-relative it counts from
        // the client's current boundary instead (depth 0), and the path from the wants
        // down to history the client has is unlimited (`None`).
        let relative = deepen.is_some_and(|d| d.relative);
        let limit = deepen.and_then(|d| d.depth).map(|n| n.max(1));
        let mut depth_of: HashMap<Oid, Option<u32>> = HashMap::new();
        let mut queue = VecDeque::new();
        if relative {
            for s in &client_shallow {
                depth_of.insert(*s, Some(0));
                queue.push_back(*s);
            }
        }
        for s in starts {
            if let std::collections::hash_map::Entry::Vacant(e) = depth_of.entry(s) {
                e.insert(if relative { None } else { Some(1) });
                queue.push_back(s);
            }
        }
        let mut w = Walk {
            wanted: args.wants.iter().copied().collect(),
            send: vec![],
            had_border: vec![],
            tags,
            tagged,
            shallow: vec![],
            unshallow: vec![],
        };
        let mut sent: HashSet<Oid> = HashSet::new();
        while let Some(c) = queue.pop_front() {
            let d = depth_of[&c];
            let (_, data) = odb
                .read(&c)?
                .ok_or_else(|| anyhow::anyhow!("missing commit {c}"))?;
            let is_had = have.contains(&c) || client_shallow.contains(&c);
            if !is_had && sent.insert(c) {
                w.send.push(c);
            }
            let parents = commit_parents(&data);
            // History the client has is a hard stop, unless we are deepening below it.
            if is_had && (deepen.is_none() || (d.is_none() && !client_shallow.contains(&c))) {
                continue;
            }
            let at_limit = matches!((d, limit), (Some(d), Some(n)) if d >= n);
            let too_old = |p: &Oid| -> anyhow::Result<bool> {
                let Some(since) = deepen.and_then(|d| d.since) else {
                    return Ok(false);
                };
                let (_, pdata) = odb
                    .read(p)?
                    .ok_or_else(|| anyhow::anyhow!("missing commit {p}"))?;
                Ok(commit_time(&pdata).is_some_and(|t| t < since))
            };
            let mut cut = false;
            let mut descended = false;
            for p in &parents {
                if at_limit || excluded.contains(p) || too_old(p)? {
                    cut = true;
                    continue;
                }
                descended = true;
                if !depth_of.contains_key(p) {
                    depth_of.insert(*p, d.map(|d| d + 1));
                    queue.push_back(*p);
                }
            }
            // Like git, the boundary ignores haves: a shorter depth cuts history the
            // client already has.
            if cut && !client_shallow.contains(&c) {
                w.shallow.push(c);
            }
            if descended && client_shallow.contains(&c) {
                w.unshallow.push(c);
            }
        }
        // Trees of had commits next to sent ones are known to the client.
        for c in &w.send {
            let (_, data) = odb.read(c)?.unwrap();
            for p in commit_parents(&data) {
                if have.contains(&p) || client_shallow.contains(&p) {
                    w.had_border.push(p);
                }
            }
        }
        w.had_border.extend(w.unshallow.iter().copied());
        Ok(w)
    }

    /// Every object to send: tags, commits, and the parts of their trees the client
    /// does not already have and the filter keeps. Objects the client asked for by
    /// name are always sent.
    fn objects(&self, odb: &Odb, filter: Option<&Filter>) -> anyhow::Result<Vec<Oid>> {
        // Trees and blobs the client has, next to what is sent.
        let mut has = HashSet::new();
        for c in &self.had_border {
            if let Some((_, data)) = odb.read(c)? {
                let mut depths = HashMap::new();
                let none = HashSet::new();
                walk_tree(
                    odb,
                    commit_tree(&data),
                    None,
                    &none,
                    &mut depths,
                    &mut |o| {
                        has.insert(o);
                    },
                )?;
            }
        }
        let mut out = self.tags.clone();
        let mut seen: HashSet<Oid> = out.iter().copied().collect();
        let mut depths = HashMap::new();
        let mut emit = |o: Oid| {
            if seen.insert(o) {
                out.push(o);
            }
        };
        for (kind, oid) in &self.tagged {
            emit(*oid);
            if *kind == Kind::Tree {
                walk_tree(odb, Some(*oid), filter, &has, &mut depths, &mut emit)?;
            }
        }
        for c in &self.send {
            if self.wanted.contains(c) || filter.is_none_or(|f| f.keeps_kind(Kind::Commit)) {
                emit(*c);
            }
            let (_, data) = odb.read(c)?.unwrap();
            walk_tree(
                odb,
                commit_tree(&data),
                filter,
                &has,
                &mut depths,
                &mut emit,
            )?;
        }
        Ok(out)
    }
}

/// Visit every tree and blob under `root` that the client does not have (`has`) and
/// `filter` keeps. `depths` remembers the shallowest depth each tree was walked at,
/// so a tree first met deep down is walked again when it turns up higher (it matters
/// for `tree:<depth>`). Visits may repeat.
fn walk_tree(
    odb: &Odb,
    root: Option<Oid>,
    filter: Option<&Filter>,
    has: &HashSet<Oid>,
    depths: &mut HashMap<Oid, u64>,
    visit: &mut dyn FnMut(Oid),
) -> anyhow::Result<()> {
    let mut queue: VecDeque<(Oid, u64)> = root.into_iter().map(|r| (r, 0)).collect();
    while let Some((tree, depth)) = queue.pop_front() {
        if has.contains(&tree) || depths.get(&tree).is_some_and(|d| *d <= depth) {
            continue;
        }
        depths.insert(tree, depth);
        if filter.is_none_or(|f| f.keeps_tree(depth)) {
            visit(tree);
        }
        if filter.is_some_and(|f| !f.enters(depth)) {
            continue;
        }
        let (_, data) = odb
            .read(&tree)?
            .ok_or_else(|| anyhow::anyhow!("missing tree {tree}"))?;
        for (mode, oid) in
            tree_entries(&data).ok_or_else(|| anyhow::anyhow!("malformed tree {tree}"))?
        {
            if mode == GITLINK_MODE {
                continue;
            }
            if mode & 0o170000 == 0o040000 {
                queue.push_back((oid, depth + 1));
                continue;
            }
            if has.contains(&oid) {
                continue;
            }
            let size = || Ok(odb.size(&oid)?.unwrap_or(0) as u64);
            if filter.map_or(Ok(true), |f| f.keeps_blob(depth + 1, size))? {
                visit(oid);
            }
        }
    }
    Ok(())
}

/// All commits reachable from `tips`.
fn reachable_commits(odb: &Odb, tips: &[Oid]) -> anyhow::Result<HashSet<Oid>> {
    let mut seen: HashSet<Oid> = tips.iter().copied().collect();
    let mut stack = tips.to_vec();
    while let Some(c) = stack.pop() {
        let Some((Kind::Commit, data)) = odb.read(&c)? else {
            continue;
        };
        for p in commit_parents(&data) {
            if seen.insert(p) {
                stack.push(p);
            }
        }
    }
    Ok(seen)
}

/// Write a self-contained pack of `objects`, copying stored bytes where possible:
/// whole objects as they are, and deltas whose base is also being sent as ref-deltas.
/// Only deltas against objects left out are expanded and recompressed.
fn write_pack(odb: &Odb, objects: &[Oid], out: &mut dyn Write) -> anyhow::Result<()> {
    let sending: HashSet<&Oid> = objects.iter().collect();
    let mut w = HashingWriter::new(out);
    w.write_all(b"PACK\0\0\0\x02")?;
    w.write_all(&(objects.len() as u32).to_be_bytes())?;
    let mut head = Vec::new();
    for oid in objects {
        if let Some(raw) = odb.raw_whole(oid) {
            w.write_all(raw)?;
            continue;
        }
        head.clear();
        if let Some((base, size, delta)) = odb.raw_delta(oid)
            && sending.contains(&base)
        {
            encode_type_size(REF_DELTA, size as u64, &mut head);
            head.extend_from_slice(&base.0);
            w.write_all(&head)?;
            w.write_all(delta)?;
            continue;
        }
        let (kind, data) = odb
            .read(oid)?
            .ok_or_else(|| anyhow::anyhow!("missing object {oid}"))?;
        encode_type_size(kind as u8, data.len() as u64, &mut head);
        w.write_all(&head)?;
        let mut z = ZlibEncoder::new(&mut w, Compression::fast());
        z.write_all(&data)?;
        z.finish()?;
    }
    w.finish()?;
    Ok(())
}

/// Pack type code of a delta against an object named by id.
const REF_DELTA: u8 = 7;
