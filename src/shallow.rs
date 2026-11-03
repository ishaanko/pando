//! Shallow fetches (`--depth`, `--deepen`, `--shallow-since`, `--shallow-exclude`, and
//! any fetch into a shallow clone).
//!
//! Stored packs carry full history, so they cannot answer a request that cuts it. For
//! these requests Pando does what a classic Git server does: walk commits from the
//! wants, decide where history stops, and write a fresh pack of exactly those commits
//! with their trees, minus what the client already has. Objects stored whole are
//! copied without recompressing.

use crate::object::{
    GITLINK_MODE, Kind, Oid, commit_parents, commit_time, commit_tree, tree_entries,
};
use crate::odb::Odb;
use crate::pack::{HashingWriter, encode_type_size};
use crate::pktline;
use crate::planner;
use crate::server::Repo;
use crate::upload::{FetchArgs, send_packfile};
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

pub fn fetch(repo: &Repo, args: &FetchArgs, out: &mut dyn Write) -> anyhow::Result<()> {
    // Load every pack needed for the wants' full history.
    let (odb, not_tips) = {
        let meta = repo.meta.lock().unwrap();
        let plan = planner::plan(&meta, &args.wants, &[])
            .map_err(|e| anyhow::anyhow!("upload-pack: {e}"))?;
        let mut not_tips = Vec::new();
        for name in args.deepen.iter().flat_map(|d| &d.not) {
            let full = if name.starts_with("refs/") {
                name.clone()
            } else {
                format!("refs/heads/{name}")
            };
            match meta.current(&full)? {
                Some(v) => not_tips.push(v.peeled.unwrap_or(v.tip)),
                None => anyhow::bail!("deepen-not: unknown ref {name}"),
            }
        }
        (Odb::open(&repo.store, &plan.packs)?, not_tips)
    };
    let walk = Walk::new(&odb, args, &not_tips)?;
    tracing::info!(repo = %repo.name, commits = walk.send.len(), shallow = walk.shallow.len(), "shallow fetch");

    let mut head = Vec::new();
    if !args.done {
        pktline::line(&mut head, "acknowledgments\n");
        let common: Vec<&Oid> = args.haves.iter().filter(|h| odb.contains(h)).collect();
        if common.is_empty() {
            pktline::line(&mut head, "NAK\n");
        }
        for c in common {
            pktline::line(&mut head, format!("ACK {c}\n"));
        }
        pktline::line(&mut head, "ready\n");
        pktline::delim(&mut head);
    }
    if !walk.shallow.is_empty() || !walk.unshallow.is_empty() {
        pktline::line(&mut head, "shallow-info\n");
        for c in &walk.shallow {
            pktline::line(&mut head, format!("shallow {c}\n"));
        }
        for c in &walk.unshallow {
            pktline::line(&mut head, format!("unshallow {c}\n"));
        }
        pktline::delim(&mut head);
    }
    pktline::line(&mut head, "packfile\n");
    out.write_all(&head)?;
    let objects = walk.objects(&odb, args)?;
    send_packfile(out, |data| write_pack(&odb, &objects, data))
}

/// Which commits to send and where the new shallow boundary is.
struct Walk {
    /// Commits to send, in walk order.
    send: Vec<Oid>,
    /// Commits the client has that border the sent ones; their trees are not resent.
    had_border: Vec<Oid>,
    /// Tags the client asked for.
    tags: Vec<Oid>,
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
        for want in &args.wants {
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
                    _ => anyhow::bail!("want {want} is not a commit or tag"),
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
            if !depth_of.contains_key(&s) {
                depth_of.insert(s, if relative { None } else { Some(1) });
                queue.push_back(s);
            }
        }
        let mut w = Walk {
            send: vec![],
            had_border: vec![],
            tags,
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
            if cut && !client_shallow.contains(&c) && !have.contains(&c) {
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
    /// does not already have.
    fn objects(&self, odb: &Odb, _args: &FetchArgs) -> anyhow::Result<Vec<Oid>> {
        let mut known = HashSet::new();
        for c in &self.had_border {
            if let Some((_, data)) = odb.read(c)? {
                walk_tree(odb, commit_tree(&data), &mut known, &mut |_| {})?;
            }
        }
        let mut out = self.tags.clone();
        let mut seen: HashSet<Oid> = out.iter().copied().collect();
        for c in &self.send {
            if seen.insert(*c) {
                out.push(*c);
            }
            let (_, data) = odb.read(c)?.unwrap();
            walk_tree(odb, commit_tree(&data), &mut known, &mut |oid| {
                if seen.insert(oid) {
                    out.push(oid);
                }
            })?;
        }
        Ok(out)
    }
}

/// Visit every tree and blob under `root` that is not in `known`, adding them to it.
fn walk_tree(
    odb: &Odb,
    root: Option<Oid>,
    known: &mut HashSet<Oid>,
    visit: &mut dyn FnMut(Oid),
) -> anyhow::Result<()> {
    let mut stack: Vec<Oid> = root.into_iter().collect();
    while let Some(tree) = stack.pop() {
        if !known.insert(tree) {
            continue;
        }
        visit(tree);
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
                stack.push(oid);
            } else if known.insert(oid) {
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

/// Write a self-contained pack of `objects`, copying whole stored objects byte for
/// byte and recompressing only those stored as deltas.
fn write_pack(odb: &Odb, objects: &[Oid], out: &mut dyn Write) -> anyhow::Result<()> {
    let mut w = HashingWriter::new(out);
    w.write_all(b"PACK\0\0\0\x02")?;
    w.write_all(&(objects.len() as u32).to_be_bytes())?;
    for oid in objects {
        if let Some(raw) = odb.raw_whole(oid) {
            w.write_all(raw)?;
            continue;
        }
        let (kind, data) = odb
            .read(oid)?
            .ok_or_else(|| anyhow::anyhow!("missing object {oid}"))?;
        let mut head = Vec::new();
        encode_type_size(kind as u8, data.len() as u64, &mut head);
        w.write_all(&head)?;
        let mut z = ZlibEncoder::new(&mut w, Compression::fast());
        z.write_all(&data)?;
        z.finish()?;
    }
    w.finish()?;
    Ok(())
}
