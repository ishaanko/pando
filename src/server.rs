//! Pando's Git smart-HTTP endpoint.
//!
//! - Fetch speaks protocol v2 (`ls-refs`, `fetch`). Responses list `packfile-uris`
//!   pointing at Express and carry an empty inline pack. Clients that did not ask for
//!   URIs get the same stored packs spliced into one inline pack instead.
//! - Push speaks receive-pack (v0, the only push protocol). The ref advertisement holds
//!   only trunk plus `.have` lines for older trunk tips, and `no-thin`, so every pushed
//!   pack depends on trunk alone and can be reused by any reader.

use crate::express::{self, Express};
use crate::meta::{Meta, NewVersion, RefUpdate, Version};
use crate::object::Oid;
use crate::pack::{self, ParsedPack};
use crate::pktline::{self, Packet, SidebandWriter};
use crate::planner::{self, PlanError};
use crate::store::BlobStore;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};
use tokio_util::io::{StreamReader, SyncIoBridge};

const AGENT: &str = concat!("pando/", env!("CARGO_PKG_VERSION"));
const ANCESTRY_WALK_LIMIT: usize = 100_000;
/// Largest pushed pack accepted.
const MAX_PUSH_BYTES: u64 = 2 << 30;

pub struct App {
    pub meta: Mutex<Meta>,
    pub store: Arc<BlobStore>,
    /// Base URLs of Express nodes used in packfile-uris.
    pub express: Vec<String>,
    /// Signalled after trunk moves, to wake the compactor.
    pub compact: Notify,
}

/// Route by path suffix so any repository path prefix works (`/repo.git/info/refs`).
async fn dispatch(State(app): State<Arc<App>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let result = match (req.method().clone(), path.as_str()) {
        (Method::GET, p) if p.ends_with("/info/refs") => {
            advertise(app, &query, req.headers()).await
        }
        (Method::POST, p) if p.ends_with("/git-upload-pack") => upload_pack(app, req).await,
        (Method::POST, p) if p.ends_with("/git-receive-pack") => receive_pack(app, req).await,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    result.unwrap_or_else(|e| {
        tracing::error!("{path}: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response()
    })
}

fn git_response(content_type: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body.into())
        .unwrap()
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await?
}

// ---------------------------------------------------------------------------------
// Advertisements

async fn advertise(app: Arc<App>, query: &str, headers: &HeaderMap) -> anyhow::Result<Response> {
    let service = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("service="))
        .unwrap_or("");
    let v2 = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(':').any(|p| p == "version=2"));
    let mut out = Vec::new();
    match service {
        "git-upload-pack" if v2 => {
            for line in [
                "version 2".to_string(),
                format!("agent={AGENT}"),
                "ls-refs=unborn".into(),
                "fetch=packfile-uris".into(),
                "object-format=sha1".into(),
            ] {
                pktline::line(&mut out, format!("{line}\n"));
            }
            pktline::flush(&mut out);
        }
        "git-upload-pack" => {
            pktline::line(&mut out, "# service=git-upload-pack\n");
            pktline::flush(&mut out);
            pktline::line(
                &mut out,
                "ERR pando serves fetches over Git protocol v2 only (git >= 2.26)",
            );
        }
        "git-receive-pack" => {
            out = blocking(move || receive_advertisement(&app.meta.lock().unwrap())).await?;
        }
        _ => return Ok((StatusCode::FORBIDDEN, "unsupported service\n").into_response()),
    }
    Ok(git_response(
        &format!("application/x-{service}-advertisement"),
        out,
    ))
}

/// The push advertisement: trunk, then `.have` lines for trunk tips at exponentially
/// spaced distances so even a stale client finds a recent base it already has.
fn receive_advertisement(meta: &Meta) -> anyhow::Result<Vec<u8>> {
    let caps = format!(
        "report-status delete-refs side-band-64k quiet atomic ofs-delta push-options no-thin \
         object-format=sha1 agent={AGENT}"
    );
    let mut out = Vec::new();
    pktline::line(&mut out, "# service=git-receive-pack\n");
    pktline::flush(&mut out);
    match meta.trunk_head()? {
        Some(head) => {
            pktline::line(&mut out, format!("{} {}\0{caps}\n", head.tip, meta.trunk));
            let mut back = 1;
            while back < head.seq {
                if let Some(v) = meta.trunk_version(head.seq - back)? {
                    pktline::line(&mut out, format!("{} .have\n", v.tip));
                }
                back *= 2;
            }
        }
        None => pktline::line(
            &mut out,
            format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO),
        ),
    }
    pktline::flush(&mut out);
    Ok(out)
}

/// Wrap a request body as a blocking reader, gunzipping if the client compressed it.
/// The returned opener must run on a blocking thread: the gzip decoder reads its
/// header as soon as it is built, and the bridge cannot block an async worker.
fn body_reader(req: Request) -> impl FnOnce() -> Box<dyn BufRead + Send> + Send {
    let gzip = req
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v == "gzip");
    let stream = req.into_body().into_data_stream().map_err(io::Error::other);
    let sync = SyncIoBridge::new(StreamReader::new(stream));
    move || -> Box<dyn BufRead + Send> {
        if gzip {
            Box::new(BufReader::new(flate2::read::GzDecoder::new(sync)))
        } else {
            Box::new(BufReader::new(sync))
        }
    }
}

// ---------------------------------------------------------------------------------
// Fetch (protocol v2)

async fn upload_pack(app: Arc<App>, req: Request) -> anyhow::Result<Response> {
    let open_body = body_reader(req);
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(8);
    // Run the command on a blocking thread and stream whatever it writes.
    tokio::task::spawn_blocking(move || {
        let mut body = open_body();
        let mut out = ChannelWriter { tx: tx.clone() };
        if let Err(e) = serve_v2_command(&app, &mut body, &mut out) {
            tracing::error!("upload-pack: {e:#}");
            let mut err = Vec::new();
            pktline::line(&mut err, format!("ERR {e}\n"));
            let _ = tx.blocking_send(Ok(err.into()));
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Ok(git_response(
        "application/x-git-upload-pack-result",
        Body::from_stream(stream),
    ))
}

/// Forwards writes to an HTTP response body.
struct ChannelWriter {
    tx: mpsc::Sender<io::Result<Bytes>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx
            .blocking_send(Ok(Bytes::copy_from_slice(buf)))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client went away"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serve_v2_command(app: &App, r: &mut dyn BufRead, out: &mut dyn Write) -> anyhow::Result<()> {
    let (head, _) = pktline::read_section(r)?;
    let command = head
        .iter()
        .find_map(|l| l.strip_prefix("command="))
        .unwrap_or("")
        .to_string();
    let (args, _) = pktline::read_section(r)?;
    match command.as_str() {
        "ls-refs" => out.write_all(&ls_refs(&app.meta.lock().unwrap(), &args)?)?,
        "fetch" => fetch(app, &args, out)?,
        "" => {} // a bare flush: the client is done
        other => anyhow::bail!("unknown command {other}"),
    }
    Ok(())
}

fn ls_refs(meta: &Meta, args: &[String]) -> anyhow::Result<Vec<u8>> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let peel = args.iter().any(|a| a == "peel");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<String> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .map(String::from)
        .collect();
    let wants_head = prefixes.is_empty() || prefixes.iter().any(|p| "HEAD".starts_with(p.as_str()));
    let mut out = Vec::new();
    if wants_head {
        let symref = if symrefs {
            format!(" symref-target:{}", meta.trunk)
        } else {
            String::new()
        };
        match meta.trunk_head()? {
            Some(v) => pktline::line(&mut out, format!("{} HEAD{symref}\n", v.tip)),
            None if unborn => pktline::line(&mut out, format!("unborn HEAD{symref}\n")),
            None => {}
        }
    }
    for v in meta.refs(&prefixes)? {
        let peeled = match (peel, v.peeled) {
            (true, Some(p)) => format!(" peeled:{p}"),
            _ => String::new(),
        };
        pktline::line(&mut out, format!("{} {}{peeled}\n", v.tip, v.refname));
    }
    pktline::flush(&mut out);
    Ok(out)
}

fn fetch(app: &App, args: &[String], out: &mut dyn Write) -> anyhow::Result<()> {
    let oids = |prefix: &str| -> Vec<Oid> {
        args.iter()
            .filter_map(|a| a.strip_prefix(prefix))
            .filter_map(Oid::from_hex)
            .collect()
    };
    let wants = oids("want ");
    let haves = oids("have ");
    let done = args.iter().any(|a| a == "done");
    let progress = !args.iter().any(|a| a == "no-progress");
    let uri_protocols: Vec<&str> = args
        .iter()
        .find_map(|a| a.strip_prefix("packfile-uris "))
        .map(|p| p.split(',').collect())
        .unwrap_or_default();

    let plan = match planner::plan(&app.meta.lock().unwrap(), &wants, &haves) {
        Ok(p) => p,
        Err(PlanError::NotOurRef(oid)) => anyhow::bail!("upload-pack: not our ref {oid}"),
        Err(e) => return Err(e.into()),
    };
    tracing::info!(
        wants = wants.len(),
        haves = haves.len(),
        have = plan.trunk_have,
        want = plan.trunk_want,
        packs = plan.packs.len(),
        "fetch"
    );

    let mut head = Vec::new();
    if !done {
        pktline::line(&mut head, "acknowledgments\n");
        if !plan.cut_found {
            // Keep negotiating: the client will walk further back in its history.
            if plan.common.is_empty() {
                pktline::line(&mut head, "NAK\n");
            }
            for c in &plan.common {
                pktline::line(&mut head, format!("ACK {c}\n"));
            }
            pktline::flush(&mut head);
            return Ok(out.write_all(&head)?);
        }
        for c in &plan.common {
            pktline::line(&mut head, format!("ACK {c}\n"));
        }
        pktline::line(&mut head, "ready\n");
        pktline::delim(&mut head);
    }

    let use_uris = !app.express.is_empty()
        && !plan.packs.is_empty()
        && app.express.iter().all(|base| {
            uri_protocols
                .iter()
                .any(|p| base.starts_with(&format!("{p}://")))
        });
    if use_uris {
        pktline::line(&mut head, "packfile-uris\n");
        for hash in &plan.packs {
            let node = express::pick_node(&app.express, hash);
            pktline::line(&mut head, format!("{hash} {node}/pack/{hash}.pack\n"));
        }
        pktline::delim(&mut head);
    }
    pktline::line(&mut head, "packfile\n");
    if progress {
        let how = if use_uris {
            "as packfile URIs"
        } else {
            "inline"
        };
        let msg = format!(
            "pando: trunk {}..{}, {} pack(s) {how}\n",
            plan.trunk_have,
            plan.trunk_want,
            plan.packs.len()
        );
        pktline::sideband(&mut head, 2, msg.as_bytes());
    }
    out.write_all(&head)?;

    // Once the packfile section has started, errors must travel on side-band 3.
    if let Err(e) = send_pack_data(app, &plan.packs, use_uris, out) {
        tracing::error!("fetch: {e:#}");
        let mut msg = Vec::new();
        pktline::sideband(&mut msg, 3, format!("pando: {e:#}\n").as_bytes());
        out.write_all(&msg)?;
        return Ok(());
    }
    let mut tail = Vec::new();
    pktline::flush(&mut tail);
    out.write_all(&tail)?;
    Ok(())
}

/// The packfile section body: an empty pack (URIs carry the data), one stored pack
/// copied as-is, or several spliced into one.
fn send_pack_data(
    app: &App,
    packs: &[Oid],
    use_uris: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let mut data = SidebandWriter::new(out);
    if use_uris || packs.is_empty() {
        data.write_all(&pack::empty_pack())?;
    } else if let [single] = packs {
        io::copy(
            &mut std::fs::File::open(app.store.pack_path(single))?,
            &mut data,
        )?;
    } else {
        let loaded: Vec<(Vec<u8>, Vec<pack::IdxEntry>)> = packs
            .iter()
            .map(|h| Ok((app.store.read_pack(h)?, app.store.read_idx(h)?)))
            .collect::<io::Result<_>>()?;
        let parts: Vec<(&[u8], &[pack::IdxEntry])> = loaded
            .iter()
            .map(|(b, i)| (b.as_slice(), i.as_slice()))
            .collect();
        pack::concat(&parts, &mut data)?;
    }
    data.into_inner()?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Push (receive-pack)

async fn receive_pack(app: Arc<App>, req: Request) -> anyhow::Result<Response> {
    let open_body = body_reader(req);
    let report = blocking(move || {
        let (report, trunk_moved) = handle_push(&app, open_body())?;
        if trunk_moved {
            app.compact.notify_one();
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
fn handle_push(app: &App, mut r: Box<dyn BufRead + Send>) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut commands = Vec::new();
    let mut caps = HashSet::new();
    while let Some(Packet::Data(line)) = pktline::read(&mut r)? {
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches('\n');
        if line.starts_with("shallow ") {
            // A shallow client lists its boundary commits first. Its pack is still
            // checked like any other: missing parents must be on trunk.
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

    let statuses: Vec<Result<(), String>>;
    let mut unpack = Ok(());
    let mut parsed: Option<ParsedPack> = None;
    if commands.iter().any(|c| !c.new.is_zero()) {
        match receive_pack_data(&app.store, &mut r) {
            Ok(p) => parsed = Some(p),
            Err(e) => unpack = Err(format!("{e}")),
        }
    }
    let mut trunk_moved = false;
    if let Err(e) = &unpack {
        tracing::warn!("push rejected: {e}");
        statuses = commands
            .iter()
            .map(|_| Err("unpacker error".to_string()))
            .collect();
    } else {
        let mut meta = app.meta.lock().unwrap();
        match plan_updates(&meta, &commands, parsed.as_ref(), force) {
            Err(e) => {
                unpack = Err(e);
                statuses = commands
                    .iter()
                    .map(|_| Err("unpacker error".to_string()))
                    .collect();
            }
            Ok(planned) => {
                // Trunk only claims the commits its new tip reaches. Other pushed commits
                // (a side branch in the same push) are indexed without a trunk position.
                let commits: Vec<Vec<(Oid, Vec<Oid>)>> = commands
                    .iter()
                    .map(|c| match &parsed {
                        Some(p) if meta.is_trunk(&c.refname) => p.reachable_commits(&c.new),
                        Some(p) => p.commits.iter().map(|(k, v)| (*k, v.clone())).collect(),
                        None => Vec::new(),
                    })
                    .collect();
                let atomic = caps.contains("atomic");
                if atomic && planned.iter().any(Result::is_err) {
                    statuses = planned
                        .iter()
                        .map(|p| {
                            Err(p
                                .as_ref()
                                .err()
                                .cloned()
                                .unwrap_or_else(|| "atomic push failed".into()))
                        })
                        .collect();
                } else {
                    let updates: Vec<Option<RefUpdate>> = planned
                        .iter()
                        .zip(&commands)
                        .zip(&commits)
                        .map(|((p, c), commits)| {
                            p.as_ref().ok().map(|plan| RefUpdate {
                                refname: &c.refname,
                                expect: plan.expect,
                                new: (!c.new.is_zero()).then(|| NewVersion {
                                    tip: c.new,
                                    pack: parsed
                                        .as_ref()
                                        .filter(|p| !p.entries.is_empty())
                                        .map(|p| p.hash),
                                    trunk_dep: plan.trunk_dep,
                                    peeled: parsed.as_ref().and_then(|p| p.peel(&c.new)),
                                    commits,
                                }),
                            })
                        })
                        .collect();
                    statuses = apply_updates(&mut meta, &updates, &planned, atomic)?;
                }
                trunk_moved = commands
                    .iter()
                    .zip(&statuses)
                    .any(|(c, s)| s.is_ok() && meta.is_trunk(&c.refname));
            }
        }
    }

    for (c, s) in commands.iter().zip(&statuses) {
        match s {
            Ok(()) => tracing::info!(r#ref = %c.refname, old = %c.old, new = %c.new, "push ok"),
            Err(e) => tracing::info!(r#ref = %c.refname, "push rejected: {e}"),
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
    let parsed = pack::parse(&std::fs::read(&staged.0)?)?;
    if !parsed.entries.is_empty() {
        store.publish(&staged.0, &parsed.hash, &parsed.entries)?;
    }
    Ok(parsed)
}

/// What a validated command will do.
struct PlannedUpdate {
    expect: Option<i64>,
    trunk_dep: u64,
}

/// Validate each command against current metadata. Returns `Err` for the whole push
/// when the pack depends on objects Pando cannot account for.
fn plan_updates(
    meta: &Meta,
    commands: &[Command],
    pack: Option<&ParsedPack>,
    force: bool,
) -> Result<Vec<Result<PlannedUpdate, String>>, String> {
    let db = |e: rusqlite::Error| format!("metadata error: {e}");
    let trunk = meta.trunk_head().map_err(db)?;
    let t = trunk.as_ref().map_or(0, |v| v.seq);
    let in_trunk = |oid: &Oid| -> Result<bool, String> {
        Ok(meta
            .commit(oid)
            .map_err(db)?
            .and_then(|c| c.trunk_seq)
            .is_some_and(|s| s <= t))
    };

    // Commit-level connectivity: anything a pushed commit points at outside the pack
    // must already be on trunk, since trunk is all the client was allowed to omit.
    if let Some(p) = pack {
        for parents in p.commits.values() {
            for parent in parents {
                if !p.commits.contains_key(parent) && !in_trunk(parent)? {
                    return Err(format!(
                        "missing objects: parent {parent} is not in the pack or on trunk"
                    ));
                }
            }
        }
        for target in p.tags.values() {
            if !p.contains(target) && !in_trunk(target)? {
                return Err(format!(
                    "missing objects: tag target {target} is not in the pack or on trunk"
                ));
            }
        }
    }

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
            if c.new.is_zero() {
                if is_trunk {
                    return Err("deleting trunk is not allowed".into());
                }
                if let Some(cur) = &current
                    && !c.old.is_zero()
                    && cur.tip != c.old
                {
                    return Err("stale info".into());
                }
                return Ok(PlannedUpdate {
                    expect,
                    trunk_dep: 0,
                });
            }
            let in_pack = pack.is_some_and(|p| p.contains(&c.new));
            if !in_pack && !in_trunk(&c.new)? {
                return Err(format!(
                    "missing objects: {} is not in the pack or on trunk",
                    c.new
                ));
            }
            if is_trunk {
                if let Some(cur) = &current {
                    if c.old != cur.tip {
                        return Err("fetch first".into());
                    }
                    if !is_ancestor(meta, pack, cur, &c.new).map_err(db)? {
                        return Err("non-fast-forward".into());
                    }
                }
            } else if let Some(cur) = &current {
                // The client never sees side branches in the advertisement, so an
                // update arrives as a create. Accept it if it fast-forwards, or with
                // `git push -o force`.
                if !c.old.is_zero() && c.old != cur.tip {
                    return Err("stale info".into());
                }
                if !force
                    && cur.tip != c.new
                    && !is_ancestor(meta, pack, cur, &c.new).map_err(db)?
                {
                    return Err("non-fast-forward (use git push -o force to overwrite)".into());
                }
            } else if !c.old.is_zero() {
                return Err("stale info".into());
            }
            Ok(PlannedUpdate {
                expect,
                trunk_dep: t,
            })
        })());
    }
    Ok(out)
}

fn apply_updates(
    meta: &mut Meta,
    updates: &[Option<RefUpdate>],
    planned: &[Result<PlannedUpdate, String>],
    atomic: bool,
) -> anyhow::Result<Vec<Result<(), String>>> {
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

/// Is the current tip of a ref an ancestor of `new`? Walks pushed commits first, then
/// the commit index. Trunk commits are not expanded when they cannot lead to the
/// answer, so a non-fast-forward check does not crawl all of history.
fn is_ancestor(
    meta: &Meta,
    pack: Option<&ParsedPack>,
    current: &Version,
    new: &Oid,
) -> rusqlite::Result<bool> {
    let target = current.tip;
    let target_on_trunk = meta.is_trunk(&current.refname)
        || meta.commit(&target)?.and_then(|c| c.trunk_seq).is_some();
    let target_is_trunk_head = meta.is_trunk(&current.refname);
    let mut seen = HashSet::from([*new]);
    let mut queue = VecDeque::from([*new]);
    let mut cache: HashMap<Oid, Vec<Oid>> = HashMap::new();
    while let Some(oid) = queue.pop_front() {
        if oid == target {
            return Ok(true);
        }
        let parents = match pack.and_then(|p| p.commits.get(&oid)) {
            Some(ps) => ps.clone(),
            None => {
                let Some(row) = meta.commit(&oid)? else {
                    continue;
                };
                // Every trunk commit is reachable from the trunk head, so the head
                // cannot be strictly behind one; nor can a commit that is not on trunk.
                if row.trunk_seq.is_some() && (target_is_trunk_head || !target_on_trunk) {
                    continue;
                }
                cache.entry(oid).or_insert(row.parents).clone()
            }
        };
        for p in parents {
            if seen.len() >= ANCESTRY_WALK_LIMIT {
                return Ok(false);
            }
            if seen.insert(p) {
                queue.push_back(p);
            }
        }
    }
    Ok(false)
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

// ---------------------------------------------------------------------------------
// Wiring

/// Open the data directory (`meta.db` + `blobs/`) and build the HTTP router. This
/// process always serves `/pack/` from its blob store, acting as the origin for
/// Express nodes. Packfile URIs point at `express_nodes`, or at `public_url` when
/// none are given.
pub fn open(
    data: &std::path::Path,
    trunk: &str,
    public_url: &str,
    express_nodes: Vec<String>,
) -> anyhow::Result<(Arc<App>, Router)> {
    std::fs::create_dir_all(data)?;
    let store = Arc::new(BlobStore::open(data.join("blobs"))?);
    let meta = Meta::open(data.join("meta.db"), trunk)?;
    let express = match express_nodes.is_empty() {
        true => vec![public_url.trim_end_matches('/').to_string()],
        false => express_nodes
            .iter()
            .map(|n| n.trim_end_matches('/').to_string())
            .collect(),
    };
    let app = Arc::new(App {
        meta: Mutex::new(meta),
        store: store.clone(),
        express,
        compact: Notify::new(),
    });
    let origin = Arc::new(Express::new(express::Origin::Local(store), None)?);
    Ok((
        app.clone(),
        express::router(origin).merge(Router::new().fallback(dispatch).with_state(app)),
    ))
}

/// Run compaction after trunk moves, and at least every `interval`.
pub fn spawn_compactor(app: Arc<App>, interval: std::time::Duration) {
    tokio::spawn(async move {
        loop {
            let _ = tokio::time::timeout(interval, app.compact.notified()).await;
            let a = app.clone();
            match tokio::task::spawn_blocking(move || crate::compact::run(&a.meta, &a.store)).await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::error!("compaction failed: {e:#}"),
                Err(e) => tracing::error!("compaction panicked: {e}"),
            }
        }
    });
}
