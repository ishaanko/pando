//! Express: the pack delivery tier.
//!
//! Pando's fetch responses carry no objects, only `packfile-uris` that point here.
//! Packs are immutable and named by hash, so Express is a plain read-through cache:
//! serve from local disk, and on a miss fill from the origin (the blob store, or an
//! upstream Pando/Express over HTTP). Concurrent misses for one pack are coalesced
//! into a single origin read. Pando spreads packs across Express nodes with
//! rendezvous hashing, so each node caches a stable slice of the pack space.

use crate::object::Oid;
use crate::store::BlobStore;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::TryStreamExt;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

pub enum Origin {
    /// Read packs straight from a blob store on this machine.
    Local(Arc<BlobStore>),
    /// Fill from another node's `/pack/` endpoint.
    Remote { base: String, client: reqwest::Client },
}

pub struct Express {
    origin: Origin,
    /// Local disk cache. Without one, a local origin is served directly.
    cache: Option<PathBuf>,
    fills: Mutex<HashMap<Oid, Arc<tokio::sync::Mutex<()>>>>,
}

impl Express {
    pub fn new(origin: Origin, cache: Option<PathBuf>) -> std::io::Result<Express> {
        if let Some(dir) = &cache {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Express { origin, cache, fills: Mutex::new(HashMap::new()) })
    }

    /// A local path holding the pack, filling the cache if needed.
    async fn local_path(&self, hash: Oid) -> anyhow::Result<Option<PathBuf>> {
        let Some(dir) = &self.cache else {
            return Ok(match &self.origin {
                Origin::Local(store) => Some(store.pack_path(&hash)).filter(|p| p.exists()),
                Origin::Remote { .. } => anyhow::bail!("remote origin requires a cache directory"),
            });
        };
        let path = dir.join(format!("{hash}.pack"));
        if path.exists() {
            return Ok(Some(path));
        }
        let gate = self.fills.lock().unwrap().entry(hash).or_default().clone();
        let _guard = gate.lock().await;
        let filled = if path.exists() { Ok(true) } else { self.fill(hash, &path).await };
        self.fills.lock().unwrap().remove(&hash);
        Ok(filled?.then_some(path))
    }

    /// Copy one pack from the origin into the cache, verifying its trailer hash.
    async fn fill(&self, hash: Oid, dest: &std::path::Path) -> anyhow::Result<bool> {
        static FILLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = FILLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dest.with_extension(format!("tmp-{}-{n}", std::process::id()));
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut sha = Sha1::new();
        let mut tail: Vec<u8> = Vec::new();
        let mut put = |chunk: &[u8], sha: &mut Sha1| {
            // Hash everything except the final 20 bytes, which must equal the hash.
            tail.extend_from_slice(chunk);
            if tail.len() > 20 {
                let n = tail.len() - 20;
                sha.update(&tail[..n]);
                tail.drain(..n);
            }
        };
        match &self.origin {
            Origin::Local(store) => {
                let Ok(bytes) = tokio::fs::read(store.pack_path(&hash)).await else { return Ok(false) };
                put(&bytes, &mut sha);
                file.write_all(&bytes).await?;
            }
            Origin::Remote { base, client } => {
                let resp = client.get(format!("{base}/pack/{hash}.pack")).send().await?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(false);
                }
                let mut stream = resp.error_for_status()?.bytes_stream();
                while let Some(chunk) = stream.try_next().await? {
                    put(&chunk, &mut sha);
                    file.write_all(&chunk).await?;
                }
            }
        }
        file.flush().await?;
        if sha.finalize().as_slice() != tail.as_slice() || tail != hash.0 {
            tokio::fs::remove_file(&tmp).await.ok();
            anyhow::bail!("pack {hash} from origin failed verification");
        }
        tokio::fs::rename(&tmp, dest).await?;
        Ok(true)
    }
}

pub fn router(express: Arc<Express>) -> Router {
    Router::new().route("/pack/{file}", get(get_pack)).with_state(express)
}

/// GET /pack/<hash>.pack, with `Range: bytes=N-` support so interrupted downloads
/// can resume.
async fn get_pack(State(x): State<Arc<Express>>, Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some(hash) = file.strip_suffix(".pack").and_then(Oid::from_hex) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = match x.local_path(hash).await {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(%hash, "express fill failed: {e:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    match serve_file(path, &headers).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(%hash, "express read failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn serve_file(path: PathBuf, headers: &HeaderMap) -> std::io::Result<Response> {
    let mut file = tokio::fs::File::open(&path).await?;
    let len = file.metadata().await?.len();
    let start = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s < len);
    let mut resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/x-git-packed-objects")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
    if let Some(start) = start {
        file.seek(SeekFrom::Start(start)).await?;
        resp = resp
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{}/{len}", len - 1))
            .header(header::CONTENT_LENGTH, len - start);
    } else {
        resp = resp.header(header::CONTENT_LENGTH, len);
    }
    Ok(resp.body(Body::from_stream(ReaderStream::new(file))).unwrap())
}

/// Rendezvous (highest random weight) hashing: the node that scores highest for a
/// pack serves it. Adding or removing a node only moves that node's share.
pub fn pick_node<'a>(nodes: &'a [String], hash: &Oid) -> &'a str {
    nodes
        .iter()
        .max_by_key(|node| {
            let mut h = Sha1::new();
            h.update(node.as_bytes());
            h.update(hash.0);
            u64::from_be_bytes(h.finalize()[..8].try_into().unwrap())
        })
        .map(String::as_str)
        .expect("at least one express node")
}
