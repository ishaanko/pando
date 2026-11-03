//! Fetch: upload-pack over Git protocol v2 (`ls-refs` and `fetch`), and over v0/v1
//! for clients that do not speak v2 (libgit2, go-git, older git).
//!
//! A fetch is answered from stored packs chosen by the planner. v2 clients that ask
//! for `packfile-uris` get links to Express plus an empty inline pack; everyone else
//! gets the same packs spliced into one inline pack. Shallow and filtered (partial
//! clone) requests are the exception: `generate.rs` writes a fresh pack for them.

use crate::generate::{self, Generated};
use crate::meta::Meta;
use crate::object::Oid;
use crate::pack::{self, IdxEntry};
use crate::pktline::{self, Packet, SidebandWriter};
use crate::planner::{self, Plan, PlanError};
use crate::server::{AGENT, Repo, Server, blocking, body_reader, git_response};
use crate::{express, metrics};
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::Response;
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Which protocol a client asked for (the `Git-Protocol` header or `GIT_PROTOCOL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V0,
    V1,
    V2,
}

impl Version {
    pub fn from_git_protocol(value: Option<&str>) -> Version {
        let params = || value.unwrap_or("").split(':');
        if params().any(|p| p == "version=2") {
            Version::V2
        } else if params().any(|p| p == "version=1") {
            Version::V1
        } else {
            Version::V0
        }
    }

    pub fn from_headers(headers: &HeaderMap) -> Version {
        Version::from_git_protocol(headers.get("git-protocol").and_then(|v| v.to_str().ok()))
    }
}

/// The v2 capability advertisement.
pub fn v2_capabilities() -> Vec<u8> {
    let mut out = Vec::new();
    for line in [
        "version 2".to_string(),
        format!("agent={AGENT}"),
        "ls-refs=unborn".into(),
        "fetch=packfile-uris shallow filter".into(),
        "object-format=sha1".into(),
    ] {
        pktline::line(&mut out, format!("{line}\n"));
    }
    pktline::flush(&mut out);
    out
}

/// Capabilities offered to v0/v1 clients.
fn v0_capabilities(meta: &Meta) -> String {
    let symref = if meta.trunk_fixed().unwrap_or(false) {
        format!(" symref=HEAD:{}", meta.trunk)
    } else {
        String::new()
    };
    format!(
        "multi_ack multi_ack_detailed side-band side-band-64k ofs-delta shallow \
         deepen-since deepen-not deepen-relative no-progress allow-tip-sha1-in-want \
         allow-reachable-sha1-in-want filter object-format=sha1{symref} agent={AGENT}"
    )
}

/// The v0/v1 ref advertisement: HEAD, every ref, and peeled tags.
pub fn v0_advertisement(meta: &Meta, version: Version) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    if version == Version::V1 {
        pktline::line(&mut out, "version 1\n");
    }
    let caps = v0_capabilities(meta);
    let mut refs: Vec<(Oid, String)> = Vec::new();
    if let Some(head) = meta.trunk_head()? {
        refs.push((head.tip, "HEAD".into()));
    }
    for v in meta.refs(&[])? {
        refs.push((v.tip, v.refname.clone()));
        if let Some(p) = v.peeled {
            refs.push((p, format!("{}^{{}}", v.refname)));
        }
    }
    if refs.is_empty() {
        pktline::line(
            &mut out,
            format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO),
        );
    }
    for (i, (oid, name)) in refs.iter().enumerate() {
        if i == 0 {
            pktline::line(&mut out, format!("{oid} {name}\0{caps}\n"));
        } else {
            pktline::line(&mut out, format!("{oid} {name}\n"));
        }
    }
    pktline::flush(&mut out);
    Ok(out)
}

/// `GET info/refs?service=git-upload-pack`.
pub async fn advertise(repo: Arc<Repo>, headers: &HeaderMap) -> anyhow::Result<Response> {
    let version = Version::from_headers(headers);
    let body = blocking(move || {
        let mut out = Vec::new();
        pktline::line(&mut out, "# service=git-upload-pack\n");
        pktline::flush(&mut out);
        if version == Version::V2 {
            out.extend(v2_capabilities());
        } else {
            out.extend(v0_advertisement(&repo.meta.lock().unwrap(), version)?);
        }
        Ok(out)
    })
    .await?;
    Ok(git_response(
        "application/x-git-upload-pack-advertisement",
        body,
    ))
}

/// `POST git-upload-pack`: one v2 command, or one round of a v0/v1 negotiation, run
/// on a blocking thread with output streamed back.
pub async fn handle(
    server: Arc<Server>,
    repo: Arc<Repo>,
    req: Request,
) -> anyhow::Result<Response> {
    let version = Version::from_headers(req.headers());
    let open_body = body_reader(req);
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(8);
    tokio::task::spawn_blocking(move || {
        let mut body = open_body();
        let mut out = ChannelWriter { tx: tx.clone() };
        let result = match version {
            Version::V2 => command(&server, &repo, &mut body, &mut out).map(|_| ()),
            _ => session_v0(&repo, &mut body, &mut out, true),
        };
        if let Err(e) = result {
            metrics::FETCH_ERRORS.inc();
            tracing::error!(repo = %repo.name, "upload-pack: {e:#}");
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

/// A whole upload-pack conversation over a byte stream (SSH): the advertisement, then
/// v2 commands until the client hangs up, or one stateful v0/v1 negotiation.
pub fn serve_stream(
    server: &Server,
    repo: &Repo,
    version: Version,
    r: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if version == Version::V2 {
        out.write_all(&v2_capabilities())?;
        out.flush()?;
        while command(server, repo, r, out)? {
            out.flush()?;
        }
        return Ok(());
    }
    out.write_all(&v0_advertisement(&repo.meta.lock().unwrap(), version)?)?;
    out.flush()?;
    session_v0(repo, r, out, false)
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

/// Run one v2 command. Returns false once the client is done.
fn command(
    server: &Server,
    repo: &Repo,
    r: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    let (head, _) = pktline::read_section(r)?;
    let command = head
        .iter()
        .find_map(|l| l.strip_prefix("command="))
        .unwrap_or("")
        .to_string();
    let (args, _) = pktline::read_section(r)?;
    match command.as_str() {
        "ls-refs" => out.write_all(&ls_refs(&repo.meta.lock().unwrap(), &args)?)?,
        "fetch" => fetch(server, repo, &FetchArgs::parse(&args), out)?,
        "" => return Ok(false), // a bare flush or hang-up: the client is done
        other => anyhow::bail!("unknown command {other}"),
    }
    Ok(true)
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

/// The arguments of a v2 `fetch` request that Pando acts on.
pub struct FetchArgs {
    pub wants: Vec<Oid>,
    pub haves: Vec<Oid>,
    pub done: bool,
    pub progress: bool,
    pub uri_protocols: Vec<String>,
    /// Commits the client holds without their parents.
    pub shallow: Vec<Oid>,
    pub deepen: Option<generate::Deepen>,
    /// A partial clone's object filter spec.
    pub filter: Option<String>,
}

impl FetchArgs {
    pub fn parse(args: &[String]) -> FetchArgs {
        let oids = |prefix: &str| -> Vec<Oid> {
            args.iter()
                .filter_map(|a| a.strip_prefix(prefix))
                .filter_map(Oid::from_hex)
                .collect()
        };
        FetchArgs {
            wants: oids("want "),
            haves: oids("have "),
            done: args.iter().any(|a| a == "done"),
            progress: !args.iter().any(|a| a == "no-progress"),
            uri_protocols: args
                .iter()
                .find_map(|a| a.strip_prefix("packfile-uris "))
                .map(|p| p.split(',').map(String::from).collect())
                .unwrap_or_default(),
            shallow: oids("shallow "),
            deepen: generate::Deepen::parse(args),
            filter: args
                .iter()
                .find_map(|a| a.strip_prefix("filter "))
                .map(String::from),
        }
    }
}

/// Plan which stored packs answer `wants` given `haves`.
fn plan_for(repo: &Repo, wants: &[Oid], haves: &[Oid]) -> anyhow::Result<Plan> {
    match planner::plan(&repo.meta.lock().unwrap(), wants, haves) {
        Ok(p) => Ok(p),
        Err(PlanError::NotOurRef(oid)) => anyhow::bail!("upload-pack: not our ref {oid}"),
        Err(e) => Err(e.into()),
    }
}

fn fetch(
    server: &Server,
    repo: &Repo,
    args: &FetchArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let plan = plan_for(repo, &args.wants, &args.haves)?;
    let generated = if generate::needed(args) {
        Some(Generated::prepare(repo, args)?)
    } else {
        None
    };
    tracing::info!(
        repo = %repo.name,
        wants = args.wants.len(),
        haves = args.haves.len(),
        have = plan.trunk_have,
        want = plan.trunk_want,
        packs = plan.packs.len(),
        "fetch"
    );

    let mut head = Vec::new();
    if !args.done {
        pktline::line(&mut head, "acknowledgments\n");
        if generated.is_none() && !plan.cut_found {
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

    if let Some(g) = &generated {
        if !g.shallow().is_empty() || !g.unshallow().is_empty() {
            pktline::line(&mut head, "shallow-info\n");
            for c in g.shallow() {
                pktline::line(&mut head, format!("shallow {c}\n"));
            }
            for c in g.unshallow() {
                pktline::line(&mut head, format!("unshallow {c}\n"));
            }
            pktline::delim(&mut head);
        }
        pktline::line(&mut head, "packfile\n");
        out.write_all(&head)?;
        let sent = send_packfile(out, Some(pktline::MAX_PAYLOAD - 1), |data| g.write(data));
        metrics::FETCHES.inc();
        metrics::FETCH_TIME.observe(started.elapsed());
        return sent;
    }

    let nodes = server.express_nodes();
    let use_uris = !plan.packs.is_empty()
        && nodes.iter().all(|base| {
            args.uri_protocols
                .iter()
                .any(|p| base.starts_with(&format!("{p}://")))
        });
    if use_uris {
        pktline::line(&mut head, "packfile-uris\n");
        for hash in &plan.packs {
            let node = express::pick_node(&nodes, hash);
            // With auth on, a signed, expiring URL stands in for the client's token.
            let query = if server.packs_need_auth() {
                format!("?{}", server.signer.sign(&repo.name, hash))
            } else {
                String::new()
            };
            pktline::line(
                &mut head,
                format!("{hash} {node}/{}.git/pack/{hash}.pack{query}\n", repo.name),
            );
        }
        pktline::delim(&mut head);
    }
    pktline::line(&mut head, "packfile\n");
    if args.progress {
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

    let packs = if use_uris { &[][..] } else { &plan.packs[..] };
    let sent = send_packfile(out, Some(pktline::MAX_PAYLOAD - 1), |data| {
        write_stored_packs(repo, packs, data)
    });
    metrics::FETCHES.inc();
    metrics::FETCH_TIME.observe(started.elapsed());
    sent
}

/// One v0/v1 upload-pack negotiation. Over HTTP (`stateless`) each request carries
/// the wants and every have so far and gets one round of answers; over SSH the rounds
/// share one connection. The client ends with `done`, and gets the pack.
fn session_v0(
    repo: &Repo,
    r: &mut dyn BufRead,
    out: &mut dyn Write,
    stateless: bool,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    // Wants, capabilities, shallow lines, deepen and filter requests, up to a flush.
    let mut args: Vec<String> = Vec::new();
    let mut caps: HashSet<String> = HashSet::new();
    while let Some(Packet::Data(d)) = pktline::read(r)? {
        let line = String::from_utf8_lossy(&d);
        let line = line.trim_end_matches('\n');
        if let Some(rest) = line.strip_prefix("want ") {
            let mut parts = rest.split(' ');
            args.push(format!("want {}", parts.next().unwrap_or("")));
            caps.extend(parts.filter(|c| !c.is_empty()).map(String::from));
        } else {
            args.push(line.to_string());
        }
    }
    // Capabilities that v2 sends as arguments.
    for arg in ["no-progress", "deepen-relative"] {
        if caps.contains(arg) {
            args.push(arg.into());
        }
    }
    let base = FetchArgs::parse(&args);
    if base.wants.is_empty() {
        return Ok(()); // nothing wanted (e.g. ls-remote)
    }
    let multi_ack = if caps.contains("multi_ack_detailed") {
        2
    } else if caps.contains("multi_ack") {
        1
    } else {
        0
    };
    let band = if caps.contains("side-band-64k") {
        Some(pktline::MAX_PAYLOAD - 1)
    } else if caps.contains("side-band") {
        Some(999)
    } else {
        None
    };
    let deepening = base.deepen.is_some();
    let shallow_info = |args: &FetchArgs, out: &mut Vec<u8>| -> anyhow::Result<Option<Generated>> {
        if !generate::needed(args) {
            return Ok(None);
        }
        let g = Generated::prepare(repo, args)?;
        if deepening {
            for c in g.shallow() {
                pktline::line(out, format!("shallow {c}\n"));
            }
            for c in g.unshallow() {
                pktline::line(out, format!("unshallow {c}\n"));
            }
            pktline::flush(out);
        }
        Ok(Some(g))
    };
    if deepening && !stateless {
        // Over a connection the boundary is announced once, before negotiating.
        let mut head = Vec::new();
        shallow_info(&base, &mut head)?;
        out.write_all(&head)?;
        out.flush()?;
    }

    let mut haves: Vec<Oid> = Vec::new();
    let mut acked_once = false;
    loop {
        let mut batch = Vec::new();
        let mut done = false;
        let mut hung_up = false;
        loop {
            match pktline::read(r)? {
                Some(Packet::Data(d)) => {
                    let line = String::from_utf8_lossy(&d);
                    let line = line.trim_end_matches('\n');
                    if line == "done" {
                        done = true;
                        break;
                    }
                    if let Some(oid) = line.strip_prefix("have ").and_then(Oid::from_hex) {
                        batch.push(oid);
                    }
                }
                Some(_) => break,
                None => {
                    hung_up = true;
                    break;
                }
            }
        }
        // Over HTTP the request simply ends where a flush would be (a shallow
        // request's first round has no haves at all); over SSH it means goodbye.
        if hung_up && !done && !stateless {
            return Ok(());
        }
        haves.extend(&batch);
        let plan = plan_for(repo, &base.wants, &haves)?;
        let common: HashSet<Oid> = plan.common.iter().copied().collect();
        let mut head = Vec::new();
        let mut last = None;
        for h in batch.iter().filter(|h| common.contains(h)) {
            last = Some(*h);
            match multi_ack {
                2 => pktline::line(&mut head, format!("ACK {h} common\n")),
                1 => pktline::line(&mut head, format!("ACK {h} continue\n")),
                _ if !acked_once => {
                    pktline::line(&mut head, format!("ACK {h}\n"));
                    acked_once = true;
                }
                _ => {}
            }
        }
        let last = last.or(plan.common.last().copied());
        if !done {
            // A request that ends without a flush (a shallow request's first round)
            // gets only the shallow boundary, as git's server stops reading there.
            if !hung_up {
                if multi_ack == 2
                    && plan.cut_found
                    && let Some(l) = last
                {
                    pktline::line(&mut head, format!("ACK {l} ready\n"));
                }
                if common.is_empty() || multi_ack > 0 {
                    pktline::line(&mut head, "NAK\n");
                }
            }
            if stateless {
                // The shallow boundary leads every stateless response.
                let mut full = Vec::new();
                if deepening {
                    shallow_info(&base, &mut full)?;
                }
                full.extend(head);
                return Ok(out.write_all(&full)?);
            }
            out.write_all(&head)?;
            out.flush()?;
            continue;
        }

        // `done`: the final answer, then the pack.
        let mut fetch_args = args.clone();
        fetch_args.extend(haves.iter().map(|h| format!("have {h}")));
        let fetch_args = FetchArgs::parse(&fetch_args);
        let mut full = Vec::new();
        let generated = if stateless || !deepening {
            shallow_info(&fetch_args, &mut full)?
        } else if generate::needed(&fetch_args) {
            Some(Generated::prepare(repo, &fetch_args)?)
        } else {
            None
        };
        full.extend(head);
        match last {
            Some(l) if multi_ack > 0 => pktline::line(&mut full, format!("ACK {l}\n")),
            Some(_) if acked_once => {}
            Some(l) => pktline::line(&mut full, format!("ACK {l}\n")),
            None => pktline::line(&mut full, "NAK\n"),
        }
        if band.is_some() && !caps.contains("no-progress") {
            let how = if generated.is_some() {
                "a fresh pack".to_string()
            } else {
                format!("{} stored pack(s)", plan.packs.len())
            };
            pktline::sideband(&mut full, 2, format!("pando: sending {how}\n").as_bytes());
        }
        out.write_all(&full)?;
        tracing::info!(repo = %repo.name, wants = base.wants.len(), haves = haves.len(), packs = plan.packs.len(), "fetch (v0)");
        let sent = send_packfile(out, band, |data| match &generated {
            Some(g) => g.write(data),
            None => write_stored_packs(repo, &plan.packs, data),
        });
        metrics::FETCHES.inc();
        metrics::FETCH_TIME.observe(started.elapsed());
        return sent;
    }
}

/// Write pack bytes from `fill`, framed on side-band 1 in packets of at most `band`
/// bytes (or raw without side-band), then a flush. Errors after the pack has started
/// travel on side-band 3.
pub fn send_packfile(
    out: &mut dyn Write,
    band: Option<usize>,
    fill: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(max) = band else {
        fill(out)?;
        return Ok(out.flush()?);
    };
    let mut data = SidebandWriter::with_limit(&mut *out, max);
    let result = fill(&mut data).and_then(|()| Ok(data.into_inner().map(|_| ())?));
    let mut tail = Vec::new();
    match result {
        Ok(()) => pktline::flush(&mut tail),
        Err(e) => {
            tracing::error!("fetch: {e:#}");
            pktline::sideband(&mut tail, 3, format!("pando: {e:#}\n").as_bytes());
        }
    }
    out.write_all(&tail)?;
    Ok(out.flush()?)
}

/// One stored pack copied as-is, several spliced into one, or an empty pack.
fn write_stored_packs(repo: &Repo, packs: &[Oid], out: &mut dyn Write) -> anyhow::Result<()> {
    match packs {
        [] => out.write_all(&pack::empty_pack())?,
        [single] => out.write_all(&repo.store.map_pack(single)?)?,
        _ => {
            let loaded: Vec<(memmap2::Mmap, Vec<IdxEntry>)> = packs
                .iter()
                .map(|h| Ok((repo.store.map_pack(h)?, repo.store.read_idx(h)?)))
                .collect::<io::Result<_>>()?;
            let parts: Vec<(&[u8], &[IdxEntry])> =
                loaded.iter().map(|(b, i)| (&b[..], i.as_slice())).collect();
            pack::concat(&parts, out)?;
        }
    }
    Ok(())
}
