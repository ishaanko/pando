//! Fetch over Git protocol v2: `ls-refs` and `fetch`.
//!
//! A fetch is answered from stored packs chosen by the planner. Clients that ask for
//! `packfile-uris` get links to Express plus an empty inline pack; others get the same
//! packs spliced into one inline pack. Shallow requests are the exception: their
//! history has to be cut, so `shallow.rs` builds a fresh pack for them.

use crate::object::Oid;
use crate::pack::{self, IdxEntry};
use crate::pktline::{self, SidebandWriter};
use crate::planner::{self, PlanError};
use crate::server::{AGENT, Repo, Server, body_reader, git_response};
use crate::{express, meta::Meta, shallow};
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::Response;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

/// The v2 capability advertisement (`GET info/refs?service=git-upload-pack`).
pub fn advertise(headers: &HeaderMap) -> Response {
    let v2 = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(':').any(|p| p == "version=2"));
    let mut out = Vec::new();
    if v2 {
        for line in [
            "version 2".to_string(),
            format!("agent={AGENT}"),
            "ls-refs=unborn".into(),
            "fetch=packfile-uris shallow".into(),
            "object-format=sha1".into(),
        ] {
            pktline::line(&mut out, format!("{line}\n"));
        }
        pktline::flush(&mut out);
    } else {
        pktline::line(&mut out, "# service=git-upload-pack\n");
        pktline::flush(&mut out);
        pktline::line(
            &mut out,
            "ERR pando serves fetches over Git protocol v2 only (git >= 2.26)",
        );
    }
    git_response("application/x-git-upload-pack-advertisement", out)
}

/// `POST git-upload-pack`: run one v2 command on a blocking thread, streaming output.
pub async fn handle(
    server: Arc<Server>,
    repo: Arc<Repo>,
    req: Request,
) -> anyhow::Result<Response> {
    let open_body = body_reader(req);
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(8);
    tokio::task::spawn_blocking(move || {
        let mut body = open_body();
        let mut out = ChannelWriter { tx: tx.clone() };
        if let Err(e) = command(&server, &repo, &mut body, &mut out) {
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

fn command(
    server: &Server,
    repo: &Repo,
    r: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
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

/// The arguments of a v2 `fetch` request that Pando acts on.
pub struct FetchArgs {
    pub wants: Vec<Oid>,
    pub haves: Vec<Oid>,
    pub done: bool,
    pub progress: bool,
    pub uri_protocols: Vec<String>,
    /// Commits the client holds without their parents.
    pub shallow: Vec<Oid>,
    pub deepen: Option<shallow::Deepen>,
}

impl FetchArgs {
    fn parse(args: &[String]) -> FetchArgs {
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
            deepen: shallow::Deepen::parse(args),
        }
    }
}

fn fetch(
    server: &Server,
    repo: &Repo,
    args: &FetchArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if args.deepen.is_some() || !args.shallow.is_empty() {
        return shallow::fetch(repo, args, out);
    }
    let plan = match planner::plan(&repo.meta.lock().unwrap(), &args.wants, &args.haves) {
        Ok(p) => p,
        Err(PlanError::NotOurRef(oid)) => anyhow::bail!("upload-pack: not our ref {oid}"),
        Err(e) => return Err(e.into()),
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
    send_packfile(out, |data| write_stored_packs(repo, packs, data))
}

/// Write the body of a `packfile` section: `fill` writes pack bytes, which are framed
/// on side-band 1. Errors after the section has started travel on side-band 3.
pub fn send_packfile(
    out: &mut dyn Write,
    fill: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut data = SidebandWriter::new(&mut *out);
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
    Ok(())
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
