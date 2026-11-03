//! Express: the pack delivery tier.
//!
//! Pando's fetch responses carry no objects, only `packfile-uris` that point here.
//! Packs are immutable and named by hash, so Express is a plain read-through cache:
//! serve from local disk, and on a miss fill from the origin (the blob store, or an
//! upstream Pando/Express over HTTP). Concurrent misses for one pack are coalesced
//! into a single origin read. Pando spreads packs across Express nodes with
//! rendezvous hashing, so each node caches a stable slice of the pack space.

use crate::auth::UrlSigner;
use crate::object::Oid;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
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

/// Finds a pack on this machine by repository name and hash.
pub type LocalPacks = Arc<dyn Fn(&str, &Oid) -> Option<PathBuf> + Send + Sync>;

pub enum Origin {
    /// Read packs straight from blob stores on this machine.
    Local(LocalPacks),
    /// Fill from another node's `/<repo>.git/pack/` endpoint.
    Remote {
        base: String,
        client: reqwest::Client,
    },
}

pub struct Express {
    origin: Origin,
    /// Local disk cache. Without one, a local origin is served directly.
    cache: Option<PathBuf>,
    fills: Mutex<HashMap<Oid, Arc<tokio::sync::Mutex<()>>>>,
    signer: Option<UrlSigner>,
}

impl Express {
    /// With a `signer`, every request must carry a valid signed query (see
    /// [`UrlSigner`]); the same query is forwarded when filling from the origin.
    pub fn new(
        origin: Origin,
        cache: Option<PathBuf>,
        signer: Option<UrlSigner>,
    ) -> std::io::Result<Express> {
        if let Some(dir) = &cache {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Express {
            origin,
            cache,
            fills: Mutex::new(HashMap::new()),
            signer,
        })
    }

    /// A local path holding the pack, filling the cache if needed.
    async fn local_path(
        &self,
        repo: &str,
        hash: Oid,
        query: &str,
    ) -> anyhow::Result<Option<PathBuf>> {
        let Some(dir) = &self.cache else {
            return Ok(match &self.origin {
                Origin::Local(find) => find(repo, &hash).filter(|p| p.exists()),
                Origin::Remote { .. } => anyhow::bail!("remote origin requires a cache directory"),
            });
        };
        let path = dir.join(format!("{hash}.pack"));
        if path.exists() {
            return Ok(Some(path));
        }
        let gate = self.fills.lock().unwrap().entry(hash).or_default().clone();
        let _guard = gate.lock().await;
        let filled = if path.exists() {
            Ok(true)
        } else {
            self.fill(repo, hash, query, &path).await
        };
        self.fills.lock().unwrap().remove(&hash);
        Ok(filled?.then_some(path))
    }

    /// Copy one pack from the origin into the cache, verifying its trailer hash.
    async fn fill(
        &self,
        repo: &str,
        hash: Oid,
        query: &str,
        dest: &std::path::Path,
    ) -> anyhow::Result<bool> {
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
            Origin::Local(find) => {
                let Some(path) = find(repo, &hash) else {
                    return Ok(false);
                };
                let Ok(bytes) = tokio::fs::read(path).await else {
                    return Ok(false);
                };
                put(&bytes, &mut sha);
                file.write_all(&bytes).await?;
            }
            Origin::Remote { base, client } => {
                let resp = client
                    .get(format!("{base}/{repo}.git/pack/{hash}.pack?{query}"))
                    .send()
                    .await?;
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
    Router::new().fallback(get(get_pack)).with_state(express)
}

/// Split `/<repo>.git/pack/<hash>.pack` into the repository name and pack hash.
pub fn parse_pack_path(path: &str) -> Option<(&str, Oid)> {
    let (repo, file) = path.rsplit_once("/pack/")?;
    let hash = Oid::from_hex(file.strip_suffix(".pack")?)?;
    let repo = repo.trim_start_matches('/');
    Some((repo.strip_suffix(".git").unwrap_or(repo), hash))
}

async fn get_pack(
    State(x): State<Arc<Express>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    match parse_pack_path(uri.path()) {
        Some((repo, hash)) => {
            let query = uri.query().unwrap_or("");
            if x.signer
                .as_ref()
                .is_some_and(|s| !s.verify(repo, &hash, query))
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    "pack URL is unsigned or expired\n",
                )
                    .into_response();
            }
            serve(&x, repo, hash, &headers, query).await
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serve one pack, with `Range: bytes=N-` support so interrupted downloads can
/// resume. Pack URLs are capability URLs: the 160-bit hash is only learned from an
/// authorized fetch.
pub async fn serve(
    x: &Express,
    repo: &str,
    hash: Oid,
    headers: &HeaderMap,
    query: &str,
) -> Response {
    let path = match x.local_path(repo, hash, query).await {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(%hash, "express fill failed: {e:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    match serve_file(path, headers).await {
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
    crate::metrics::PACK_BYTES_SERVED.add(len - start.unwrap_or(0));
    if let Some(start) = start {
        file.seek(SeekFrom::Start(start)).await?;
        resp = resp
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{len}", len - 1),
            )
            .header(header::CONTENT_LENGTH, len - start);
    } else {
        resp = resp.header(header::CONTENT_LENGTH, len);
    }
    Ok(resp
        .body(Body::from_stream(ReaderStream::new(file)))
        .unwrap())
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
