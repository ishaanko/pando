//! Push: receive-pack (v0, the only push protocol).
//!
//! The pack is streamed to staging, decoded once, and published to blob storage. Every
//! reference from the pack to an object outside it is resolved through the object
//! index; that both rejects packs with missing objects and records exactly which trunk
//! range and which other packs the new versions depend on. Each ref then moves by a
//! compare-and-swap on its pointer.

use crate::meta::{CommitRow, Deps, Meta, NewVersion, RefUpdate, Version};
use crate::object::Oid;
use crate::pack::{self, ParsedPack};
use crate::pktline::{self, Packet};
use crate::server::{AGENT, Advertise, Repo, Server, blocking, body_reader, git_response};
use crate::store::{BlobStore, map_file};
use axum::extract::Request;
use axum::response::Response;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, Read};
use std::sync::Arc;

const ANCESTRY_WALK_LIMIT: usize = 100_000;
/// Largest pushed pack accepted.
const MAX_PUSH_BYTES: u64 = 2 << 30;

/// The ref advertisement for pushers, plus `.have` lines for older trunk tips at
/// exponentially spaced distances, so even a stale client finds a base it has.
pub async fn advertise(server: Arc<Server>, repo: Arc<Repo>) -> anyhow::Result<Response> {
    let out = blocking(move || {
        let meta = repo.meta.lock().unwrap();
        let caps = format!(
            "report-status delete-refs side-band-64k quiet atomic ofs-delta push-options no-thin \
             object-format=sha1 agent={AGENT}"
        );
        let mut out = Vec::new();
        pktline::line(&mut out, "# service=git-receive-pack\n");
        pktline::flush(&mut out);
        let mut refs: Vec<Version> = match server.cfg.advertise {
            Advertise::All => meta.refs(&[])?,
            Advertise::Trunk => meta.trunk_head()?.into_iter().collect(),
        };
        if refs.is_empty() {
            pktline::line(
                &mut out,
                format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO),
            );
        }
        for (i, v) in refs.drain(..).enumerate() {
            let caps = if i == 0 {
                format!("\0{caps}")
            } else {
                String::new()
            };
            pktline::line(&mut out, format!("{} {}{caps}\n", v.tip, v.refname));
        }
        if let Some(head) = meta.trunk_head()? {
            let mut back = 1;
            while back < head.seq {
                if let Some(v) = meta.trunk_version(head.seq - back)? {
                    pktline::line(&mut out, format!("{} .have\n", v.tip));
                }
                back *= 2;
            }
        }
        pktline::flush(&mut out);
        Ok(out)
    })
    .await?;
    Ok(git_response(
        "application/x-git-receive-pack-advertisement",
        out,
    ))
}

pub async fn handle(
    server: Arc<Server>,
    repo: Arc<Repo>,
    req: Request,
) -> anyhow::Result<Response> {
    let open_body = body_reader(req);
    let report = blocking(move || {
        let (report, trunk_moved) = push(&server, &repo, open_body())?;
        if trunk_moved {
            repo.compact.notify_one();
        }
        Ok(report)
    })
    .await?;
    Ok(git_response(
        "application/x-git-receive-pack-result",
        report,
    ))
}

struct Command {
    old: Oid,
    new: Oid,
    refname: String,
}

/// Read commands and the pack, apply ref updates, and build the status report.
/// Returns the report and whether trunk moved.
fn push(
    server: &Server,
    repo: &Repo,
    mut r: Box<dyn BufRead + Send>,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut commands = Vec::new();
    let mut caps = HashSet::new();
    while let Some(Packet::Data(line)) = pktline::read(&mut r)? {
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches('\n');
        if line.starts_with("shallow ") {
            // A shallow client lists its boundary commits first. Its pack is checked
            // like any other: everything it references must already exist.
            continue;
        }
        let (cmd, cap_str) = line.split_once('\0').unwrap_or((line, ""));
        caps.extend(
            cap_str
                .split(' ')
                .filter(|c| !c.is_empty())
                .map(String::from),
        );
        let mut parts = cmd.splitn(3, ' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!("malformed push command {cmd:?}");
        };
        let (Some(old), Some(new)) = (Oid::from_hex(old), Oid::from_hex(new)) else {
            anyhow::bail!("malformed push command {cmd:?}");
        };
        commands.push(Command {
            old,
            new,
            refname: name.to_string(),
        });
    }
    let options = if caps.contains("push-options") {
        pktline::read_section(&mut r)?.0
    } else {
        Vec::new()
    };
    let force = options.iter().any(|o| o == "force");

    let mut unpack: Result<(), String> = Ok(());
    let mut parsed: Option<ParsedPack> = None;
    if commands.iter().any(|c| !c.new.is_zero()) {
        match receive_pack_data(&repo.store, &mut r) {
            Ok(p) => parsed = Some(p),
            Err(e) => unpack = Err(format!("{e}")),
        }
    }

    let mut trunk_moved = false;
    let statuses: Vec<Result<(), String>> = match unpack {
        Err(_) => commands
            .iter()
            .map(|_| Err("unpacker error".to_string()))
            .collect(),
        Ok(()) => {
            let mut meta = repo.meta.lock().unwrap();
            if !meta.trunk_fixed()?
                && let Some(trunk) = choose_trunk(&server.cfg.trunk, &commands)
            {
                meta.fix_trunk(trunk)?;
            }
            match plan_updates(
                &meta,
                server.cfg.advertise,
                &commands,
                parsed.as_ref(),
                force,
            ) {
                Err(e) => {
                    unpack = Err(e);
                    commands
                        .iter()
                        .map(|_| Err("unpacker error".to_string()))
                        .collect()
                }
                Ok((pack_deps, planned)) => {
                    let statuses = apply(
                        &mut meta,
                        &commands,
                        parsed.as_ref(),
                        &pack_deps,
                        &planned,
                        caps.contains("atomic"),
                    )?;
                    trunk_moved = commands
                        .iter()
                        .zip(&statuses)
                        .any(|(c, s)| s.is_ok() && meta.is_trunk(&c.refname));
                    statuses
                }
            }
        }
    };

    for (c, s) in commands.iter().zip(&statuses) {
        match s {
            Ok(()) => {
                tracing::info!(repo = %repo.name, r#ref = %c.refname, old = %c.old, new = %c.new, "push ok")
            }
            Err(e) => tracing::info!(repo = %repo.name, r#ref = %c.refname, "push rejected: {e}"),
        }
    }

    let mut report = Vec::new();
    match &unpack {
        Ok(()) => pktline::line(&mut report, "unpack ok\n"),
        Err(e) => pktline::line(&mut report, format!("unpack {e}\n")),
    }
    for (c, s) in commands.iter().zip(&statuses) {
        match s {
            Ok(()) => pktline::line(&mut report, format!("ok {}\n", c.refname)),
            Err(e) => pktline::line(&mut report, format!("ng {} {e}\n", c.refname)),
        }
    }
    pktline::flush(&mut report);
    if !caps.contains("report-status") {
        report.clear();
    }
    if caps.contains("side-band-64k") && !report.is_empty() {
        let mut framed = Vec::new();
        pktline::sideband(&mut framed, 1, &report);
        pktline::flush(&mut framed);
        report = framed;
    }
    Ok((report, trunk_moved))
}

/// Trunk for a repository's first push: the configured trunk if pushed, else
/// `refs/heads/master`, else the first branch.
fn choose_trunk<'a>(preferred: &str, commands: &'a [Command]) -> Option<&'a str> {
    let branches: Vec<&str> = commands
        .iter()
        .filter(|c| !c.new.is_zero() && c.refname.starts_with("refs/heads/"))
        .map(|c| c.refname.as_str())
        .collect();
    branches
        .iter()
        .find(|b| **b == preferred)
        .or_else(|| branches.iter().find(|b| **b == "refs/heads/master"))
        .or(branches.first())
        .copied()
}

/// Stream the pack into staging, decode it once, and publish it to blob storage.
fn receive_pack_data(store: &BlobStore, r: &mut dyn Read) -> anyhow::Result<ParsedPack> {
    /// Removes the staging file on every exit path, panics included.
    struct Staged(std::path::PathBuf);
    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let staged = Staged(store.staging_path());
    let mut file = std::fs::File::create(&staged.0)?;
    if io::copy(&mut r.take(MAX_PUSH_BYTES + 1), &mut file)? > MAX_PUSH_BYTES {
        anyhow::bail!("pack larger than {MAX_PUSH_BYTES} bytes");
    }
    drop(file);
    let parsed = pack::parse(&map_file(&staged.0)?)?;
    if !parsed.entries.is_empty() {
        store.publish(&staged.0, &parsed.hash, &parsed.entries)?;
    }
    Ok(parsed)
}

/// What a validated command will do.
struct PlannedUpdate {
    expect: Option<i64>,
    deps: Deps,
    /// A trunk update that is not a fast-forward: trunk starts a new epoch.
    rewrite: bool,
}

/// Where an object outside the pack lives, as a dependency.
fn resolve(meta: &Meta, oid: &Oid) -> Result<Deps, String> {
    match meta.object(oid).map_err(db)? {
        Some(row) => Ok(match row.trunk_seq {
            Some(seq) if meta.in_epoch(seq) => Deps {
                trunk: seq,
                packs: vec![],
            },
            // Off trunk, or on an earlier epoch's trunk: the pack that holds it.
            _ => Deps {
                trunk: 0,
                packs: vec![row.pack],
            },
        }),
        None => Err(format!(
            "missing objects: {oid} is not in the pack or the repository"
        )),
    }
}

fn db(e: rusqlite::Error) -> String {
    format!("metadata error: {e}")
}

/// Validate each command against current metadata. Returns the pack's dependencies and
/// a plan per command, or `Err` for the whole push when the pack references objects
/// that exist nowhere.
fn plan_updates(
    meta: &Meta,
    advertise: Advertise,
    commands: &[Command],
    pack: Option<&ParsedPack>,
    force: bool,
) -> Result<(Deps, Vec<Result<PlannedUpdate, String>>), String> {
    let mut pack_deps = Deps::default();
    if let Some(p) = pack {
        for oid in p.external_refs() {
            pack_deps.add(&resolve(meta, &oid)?);
        }
    }
    let trunk = meta.trunk_head().map_err(db)?;

    let mut out = Vec::new();
    for c in commands {
        out.push((|| -> Result<PlannedUpdate, String> {
            if !valid_refname(&c.refname) {
                return Err("invalid ref name".into());
            }
            let is_trunk = meta.is_trunk(&c.refname);
            let current = if is_trunk {
                trunk.clone()
            } else {
                meta.current(&c.refname).map_err(db)?
            };
            let expect = current.as_ref().map(|v| v.id);
            let stale = |cur: &Option<Version>| c.old != cur.as_ref().map_or(Oid::ZERO, |v| v.tip);
            if c.new.is_zero() {
                if is_trunk {
                    return Err("deleting trunk is not allowed".into());
                }
                if current.is_some() && !c.old.is_zero() && stale(&current) {
                    return Err("stale info".into());
                }
                return Ok(PlannedUpdate {
                    expect,
                    deps: Deps::default(),
                    rewrite: false,
                });
            }

            let mut deps = pack_deps.clone();
            if !pack.is_some_and(|p| p.contains(&c.new)) {
                deps.add(&resolve(meta, &c.new)?);
            }
            if current.is_none() {
                let sibling = commands.iter().find(|o| {
                    !o.new.is_zero()
                        && (o.refname.starts_with(&format!("{}/", c.refname))
                            || c.refname.starts_with(&format!("{}/", o.refname)))
                });
                if let Some(other) = sibling {
                    return Err(format!(
                        "cannot create both {} and {}",
                        c.refname, other.refname
                    ));
                }
                if let Some(other) = meta.ref_path_conflict(&c.refname).map_err(db)? {
                    return Err(format!("conflicts with existing ref {other}"));
                }
            }
            let mut rewrite = false;
            if is_trunk {
                if stale(&current) {
                    return Err("fetch first".into());
                }
                // A forced update (the client only sends one when forced) rewrites
                // trunk history.
                if let Some(cur) = &current {
                    rewrite = !is_ancestor(meta, pack, cur, &c.new).map_err(db)?;
                }
            } else if advertise == Advertise::All || current.is_none() {
                if stale(&current) {
                    return Err("fetch first".into());
                }
            } else if let Some(cur) = &current {
                // With only trunk advertised, an update to an existing side branch
                // arrives as a create. Accept it if it fast-forwards, or with
                // `git push -o force`.
                if !c.old.is_zero() && stale(&current) {
                    return Err("stale info".into());
                }
                if !force
                    && cur.tip != c.new
                    && !is_ancestor(meta, pack, cur, &c.new).map_err(db)?
                {
                    return Err("non-fast-forward (use git push -o force to overwrite)".into());
                }
            }
            Ok(PlannedUpdate {
                expect,
                deps,
                rewrite,
            })
        })());
    }
    Ok((pack_deps, out))
}

fn apply(
    meta: &mut Meta,
    commands: &[Command],
    pack: Option<&ParsedPack>,
    pack_deps: &Deps,
    planned: &[Result<PlannedUpdate, String>],
    atomic: bool,
) -> anyhow::Result<Vec<Result<(), String>>> {
    if atomic && planned.iter().any(Result::is_err) {
        return Ok(planned
            .iter()
            .map(|p| {
                Err(p
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_else(|| "atomic push failed".into()))
            })
            .collect());
    }
    let stored = pack.filter(|p| !p.entries.is_empty());
    if let Some(p) = stored {
        meta.index_pack(
            &p.hash,
            pack_deps,
            p.entries.iter().map(|e| e.oid),
            &p.commits,
            &p.tags,
        )?;
    }
    // With a single ref pushed, every object in the pack is reachable from its tip.
    let complete = commands.iter().filter(|c| !c.new.is_zero()).count() == 1;
    // Trunk marks only what its new tip reaches; walk that once.
    let reached: Vec<Vec<Oid>> = commands
        .iter()
        .map(|c| match stored {
            Some(p) if !c.new.is_zero() && meta.is_trunk(&c.refname) => {
                p.reachable_objects([c.new]).into_iter().collect()
            }
            _ => Vec::new(),
        })
        .collect();

    // Branch packs a new trunk tip has absorbed (fast-forwarded to or merged).
    let reached_packs: Vec<Vec<Oid>> = commands
        .iter()
        .zip(planned)
        .map(|(c, p)| match p {
            Ok(plan)
                if !c.new.is_zero() && meta.is_trunk(&c.refname) && !plan.deps.packs.is_empty() =>
            {
                absorbed_packs(meta, pack, &c.new, &plan.deps.packs)
            }
            _ => Ok(Vec::new()),
        })
        .collect::<rusqlite::Result<_>>()?;
    let peeled: Vec<Option<Oid>> = commands
        .iter()
        .map(|c| peel(meta, pack, &c.new))
        .collect::<rusqlite::Result<_>>()?;

    let updates: Vec<Option<RefUpdate>> = planned
        .iter()
        .zip(commands)
        .zip(reached.iter().zip(&reached_packs).zip(&peeled))
        .map(|((p, c), ((reached, reached_packs), peeled))| {
            p.as_ref().ok().map(|plan| RefUpdate {
                refname: &c.refname,
                expect: plan.expect,
                new: (!c.new.is_zero()).then(|| NewVersion {
                    tip: c.new,
                    pack: stored.map(|p| p.hash),
                    deps: plan.deps.clone(),
                    pack_complete: complete,
                    peeled: *peeled,
                    reached,
                    reached_packs,
                    rewrite: plan.rewrite,
                }),
            })
        })
        .collect();

    if atomic {
        let all: Vec<&RefUpdate> = updates.iter().flatten().collect();
        return Ok(match meta.update_all(&all)? {
            Ok(_) => planned.iter().map(|_| Ok(())).collect(),
            Err(e) => planned.iter().map(|_| Err(e.clone())).collect(),
        });
    }
    let mut out = Vec::new();
    for (u, p) in updates.iter().zip(planned) {
        out.push(match (u, p) {
            (Some(u), _) => meta.update_all(&[u])?.map(|_| ()),
            (None, Err(e)) => Err(e.clone()),
            (None, Ok(_)) => unreachable!(),
        });
    }
    Ok(out)
}

/// Follow annotated tags from `tip`, through this pack and tags stored earlier, to the
/// first non-tag object. `None` if `tip` is not a tag.
fn peel(meta: &Meta, pack: Option<&ParsedPack>, tip: &Oid) -> rusqlite::Result<Option<Oid>> {
    let mut cur = *tip;
    let mut peeled = None;
    for _ in 0..64 {
        let next = match pack.and_then(|p| p.tags.get(&cur)) {
            Some(t) => Some(*t),
            None => meta.tag_target(&cur)?,
        };
        match next {
            Some(t) => {
                cur = t;
                peeled = Some(t);
            }
            None => break,
        }
    }
    Ok(peeled)
}

/// Which of `targets` are reachable from `from` (inclusive). Pushed commits are walked
/// freely; at most `ANCESTRY_WALK_LIMIT` commits come from the index, and `skip`
/// prunes indexed commits that cannot lead to an answer.
fn reach(
    meta: &Meta,
    pack: Option<&ParsedPack>,
    from: &Oid,
    targets: &HashSet<Oid>,
    skip: impl Fn(&CommitRow) -> bool,
) -> rusqlite::Result<HashSet<Oid>> {
    let mut found = HashSet::new();
    let mut seen = HashSet::from([*from]);
    let mut queue = VecDeque::from([*from]);
    let mut lookups = 0;
    while let Some(oid) = queue.pop_front() {
        if targets.contains(&oid) {
            found.insert(oid);
            if found.len() == targets.len() {
                break;
            }
        }
        let parents = match pack.and_then(|p| p.commits.get(&oid)) {
            Some(ps) => ps.clone(),
            None => {
                lookups += 1;
                if lookups > ANCESTRY_WALK_LIMIT {
                    break;
                }
                match meta.commit(&oid)? {
                    Some(row) if !skip(&row) => row.parents,
                    _ => continue,
                }
            }
        };
        queue.extend(parents.into_iter().filter(|p| seen.insert(*p)));
    }
    Ok(found)
}

/// Is the current tip of a ref an ancestor of `new`?
fn is_ancestor(
    meta: &Meta,
    pack: Option<&ParsedPack>,
    current: &Version,
    new: &Oid,
) -> rusqlite::Result<bool> {
    let target = current.tip;
    let target_is_trunk_head = meta.is_trunk(&current.refname);
    let target_on_trunk =
        target_is_trunk_head || meta.commit(&target)?.and_then(|c| c.trunk_seq).is_some();
    // Every trunk commit is reachable from the trunk head, so the head cannot be
    // strictly behind one; nor can a commit that is not on trunk.
    let skip =
        |row: &CommitRow| row.trunk_seq.is_some() && (target_is_trunk_head || !target_on_trunk);
    Ok(!reach(meta, pack, new, &HashSet::from([target]), skip)?.is_empty())
}

/// Packs among `deps` (and their dependencies) whose objects are all reachable from
/// a new trunk tip: those holding a complete version whose tip the new tip reaches.
fn absorbed_packs(
    meta: &Meta,
    pack: Option<&ParsedPack>,
    tip: &Oid,
    deps: &[Oid],
) -> rusqlite::Result<Vec<Oid>> {
    let mut by_tip: HashMap<Oid, Vec<Oid>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut work = deps.to_vec();
    while let Some(q) = work.pop() {
        if seen.len() >= 1000 || !seen.insert(q) {
            continue;
        }
        for v in meta.complete_versions_in_pack(&q)? {
            by_tip.entry(v.tip).or_default().push(q);
        }
        if let Some(d) = meta.pack_deps(&q)? {
            work.extend(d.packs);
        }
    }
    let targets: HashSet<Oid> = by_tip.keys().copied().collect();
    let found = reach(meta, pack, tip, &targets, |row| row.trunk_seq.is_some())?;
    let mut out: Vec<Oid> = found
        .iter()
        .flat_map(|t| by_tip[t].iter().copied())
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// A conservative subset of `git check-ref-format`.
fn valid_refname(name: &str) -> bool {
    name.starts_with("refs/")
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b" ~^:?*[\\".contains(&b))
}
