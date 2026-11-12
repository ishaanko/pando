//! Git LFS: the batch API and the object transfers it points at, so a stock `git lfs`
//! client works against `https://host/<repo>.git` and `ssh://host/<repo>.git` remotes.
//!
//! - `POST /<repo>.git/info/lfs/objects/batch`  batch API (`basic` transfers, sha256)
//! - `GET  /<repo>.git/info/lfs/objects/<oid>`  download, with `Range` support
//! - `PUT  /<repo>.git/info/lfs/objects/<oid>`  upload
//! - `/<repo>.git/info/lfs/locks...`            501, which makes git-lfs switch lock
//!   verification off for the remote and carry on quietly
//!
//! Batch requests carry the Git credentials and are authorized exactly like Git
//! requests (read to download, write to upload). Over SSH, git-lfs first runs
//! `git-lfs-authenticate <repo> <operation>`, which hands back this server's HTTP
//! endpoint and a short-lived credential bound to the key's identity, the repository
//! and the access the operation needs (see [`ssh_authenticate`]). The batch response
//! then points at transfer URLs signed for one operation on one object of one
//! repository (and, for uploads, one size), valid for an hour, so the transfers need
//! no credentials of their own. All signatures are HMACs under the server's URL secret.
//!
//! Objects live in the repository's own directory, `<repo>.git/lfs/objects/ab/cd/<oid>`,
//! so deleting, renaming and backing up a repository carry them along. An upload is
//! streamed to `<repo>.git/lfs/tmp/`, checked against its oid and size, synced, and
//! only then renamed into place. Objects are never garbage collected: like other LFS
//! servers, Pando keeps every object it accepted.

use crate::auth::Access;
use crate::server::{Denied, Repo, Server, valid_repo_name};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

const CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
/// How long signed transfer URLs stay valid.
const HREF_TTL_SECS: u64 = 3600;
/// How long a `git-lfs-authenticate` credential stays valid. git-lfs asks again when it
/// runs out.
const SSH_TTL_SECS: u64 = 300;

/// Split `/<repo>.git/info/lfs/<rest>` into the repository name and `<rest>`.
pub fn parse_path(path: &str) -> Option<(&str, &str)> {
    let (repo, rest) = path.split_once(".git/info/lfs/")?;
    Some((repo.trim_start_matches('/'), rest))
}

/// Serve one request under `/<repo>.git/info/lfs/`.
pub async fn handle(server: Arc<Server>, repo: &str, rest: &str, req: Request) -> Response {
    if !valid_repo_name(repo) {
        return message(StatusCode::NOT_FOUND, "repository not found");
    }
    if rest == "locks" || rest.starts_with("locks/") {
        return message(StatusCode::NOT_IMPLEMENTED, "locking is not supported");
    }
    let Some(object) = rest.strip_prefix("objects/") else {
        return message(StatusCode::NOT_FOUND, "not found");
    };
    match (req.method().clone(), object) {
        (Method::POST, "batch") => batch(server, repo, req).await,
        (Method::GET, oid) if valid_oid(oid) => download(server, repo, oid, req).await,
        (Method::PUT, oid) if valid_oid(oid) => upload(server, repo, oid, req).await,
        (_, "batch") => message(StatusCode::METHOD_NOT_ALLOWED, "use POST"),
        _ => message(StatusCode::NOT_FOUND, "not found"),
    }
}

/// An oid is 64 lowercase hex characters (the SHA-256 of the object).
pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Where object `oid` (already validated) lives in a repository directory.
pub fn object_path(repo_dir: &Path, oid: &str) -> PathBuf {
    repo_dir
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Download,
    Upload,
}

impl Operation {
    pub fn parse(s: &str) -> Option<Operation> {
        match s {
            "download" => Some(Operation::Download),
            "upload" => Some(Operation::Upload),
            _ => None,
        }
    }

    pub fn need(self) -> Access {
        match self {
            Operation::Download => Access::Read,
            Operation::Upload => Access::Write,
        }
    }
}

#[derive(Deserialize)]
struct BatchRequest {
    operation: Operation,
    /// Transfer adapters the client supports; empty means `basic`.
    #[serde(default)]
    transfers: Vec<String>,
    objects: Vec<ObjectSpec>,
    hash_algo: Option<String>,
}

#[derive(Deserialize)]
struct ObjectSpec {
    oid: String,
    size: i64,
}

#[derive(Serialize)]
struct BatchResponse {
    transfer: &'static str,
    objects: Vec<ObjectResult>,
    hash_algo: &'static str,
}

#[derive(Serialize)]
struct ObjectResult {
    oid: String,
    size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    authenticated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<Actions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ObjectError>,
}

#[derive(Serialize)]
struct Actions {
    #[serde(skip_serializing_if = "Option::is_none")]
    download: Option<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload: Option<Action>,
}

#[derive(Serialize)]
struct Action {
    href: String,
    expires_in: u64,
}

#[derive(Serialize)]
struct ObjectError {
    code: u16,
    message: &'static str,
}

#[derive(Serialize)]
struct Message<'a> {
    message: &'a str,
}

/// What `git-lfs-authenticate` prints.
#[derive(Serialize)]
struct SshAuth {
    href: String,
    header: BTreeMap<&'static str, String>,
    expires_in: u64,
}

fn message(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::to_string(&Message { message: msg }).unwrap();
    (status, [(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response()
}

/// The response for a batch request that may not go ahead.
fn denied(why: Denied) -> Response {
    match why {
        Denied::Unauthenticated => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::CONTENT_TYPE, CONTENT_TYPE)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"pando\"")
            .body(Body::from(r#"{"message":"authentication required"}"#))
            .unwrap(),
        Denied::Forbidden(msg) => message(StatusCode::FORBIDDEN, &msg),
    }
}

async fn batch(server: Arc<Server>, name: &str, req: Request) -> Response {
    let headers = req.headers().clone();
    // Every operation needs at least read access, so check that before reading and
    // parsing a body from someone who may have no business here.
    if let Err(why) = authorize(&server, &headers, name, Access::Read) {
        return denied(why);
    }
    let limit = 16 << 20;
    let body = match tokio::time::timeout(
        crate::server::BODY_IDLE,
        axum::body::to_bytes(req.into_body(), limit),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => return message(StatusCode::BAD_REQUEST, "unreadable or oversized request"),
    };
    let request: BatchRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("invalid batch request: {e}");
            return message(StatusCode::UNPROCESSABLE_ENTITY, &msg);
        }
    };
    if request.hash_algo.as_deref().is_some_and(|h| h != "sha256") {
        return message(StatusCode::CONFLICT, "only sha256 objects are supported");
    }
    if !request.transfers.is_empty() && !request.transfers.iter().any(|t| t == "basic") {
        return message(
            StatusCode::UNPROCESSABLE_ENTITY,
            "only the basic transfer adapter is supported",
        );
    }
    let op = request.operation;
    let user = match authorize(&server, &headers, name, op.need()) {
        Ok(user) => user,
        Err(why) => return denied(why),
    };
    // Uploads come from git-lfs's pre-push hook, before the push that creates the
    // repository, so they create it as a push would.
    let repo = match server.open_repo(name, op == Operation::Upload) {
        Ok(Some(r)) => r,
        Ok(None) => return message(StatusCode::NOT_FOUND, "repository not found"),
        Err(e) => {
            tracing::error!(repo = name, "lfs batch: {e:#}");
            return message(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    tracing::info!(
        repo = name,
        ?op,
        user,
        objects = request.objects.len(),
        "lfs batch"
    );
    let s = server.clone();
    let objects = crate::server::blocking(move || {
        Ok(request
            .objects
            .into_iter()
            .map(|o| answer(&s, &repo, op, o))
            .collect())
    })
    .await;
    let objects = match objects {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(repo = name, "lfs batch: {e:#}");
            return message(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let body = serde_json::to_string(&BatchResponse {
        transfer: "basic",
        objects,
        hash_algo: "sha256",
    })
    .unwrap();
    ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response()
}

/// The batch answer for one object.
fn answer(server: &Server, repo: &Repo, op: Operation, obj: ObjectSpec) -> ObjectResult {
    let mut out = ObjectResult {
        oid: obj.oid,
        size: obj.size,
        authenticated: None,
        actions: None,
        error: None,
    };
    if !valid_oid(&out.oid) || out.size < 0 {
        out.error = Some(ObjectError {
            code: 422,
            message: "invalid object id or size",
        });
        return out;
    }
    let stored = fs::metadata(object_path(&repo.dir, &out.oid)).ok();
    let base = format!(
        "{}/{}.git/info/lfs/objects/{}",
        server.cfg.public_url, repo.name, out.oid
    );
    let exp = now() + HREF_TTL_SECS;
    match (op, stored) {
        (Operation::Download, None) => {
            out.error = Some(ObjectError {
                code: 404,
                message: "object does not exist",
            });
            return out;
        }
        (Operation::Download, Some(meta)) => {
            out.size = meta.len() as i64;
            let sig = mac(server, &["lfs-download", &repo.name, &out.oid], exp);
            out.actions = Some(Actions {
                download: Some(Action {
                    href: format!("{base}?exp={exp}&sig={sig}"),
                    expires_in: HREF_TTL_SECS,
                }),
                upload: None,
            });
        }
        // Already stored: no actions tells the client to skip it.
        (Operation::Upload, Some(_)) => {}
        (Operation::Upload, None) => {
            let size = out.size.to_string();
            let sig = mac(server, &["lfs-upload", &repo.name, &out.oid, &size], exp);
            out.actions = Some(Actions {
                download: None,
                upload: Some(Action {
                    href: format!("{base}?size={size}&exp={exp}&sig={sig}"),
                    expires_in: HREF_TTL_SECS,
                }),
            });
        }
    }
    out.authenticated = Some(true);
    out
}

/// Check a batch request's credentials: a `git-lfs-authenticate` credential, or
/// whatever Git requests may carry. Returns the caller's name, if known.
fn authorize(
    server: &Server,
    headers: &HeaderMap,
    repo: &str,
    need: Access,
) -> Result<Option<String>, Denied> {
    match ssh_credential(server, headers, repo) {
        Some((access, user)) if access >= need => Ok(Some(user)),
        Some((_, user)) => Err(Denied::Forbidden(format!("{user} cannot push to {repo}"))),
        None => server.authorize(headers, repo, need),
    }
}

/// What `git-lfs-authenticate <repo> <operation>` prints for a client that SSH
/// authenticated as `user` (`None` when auth is off), which the caller has checked
/// holds `access` on `repo`: the HTTP LFS endpoint and, with auth on, an
/// `Authorization` header valid only for batch requests on `repo` needing at most
/// `access`, for a few minutes.
pub fn ssh_authenticate(server: &Server, repo: &str, access: Access, user: Option<&str>) -> String {
    let mut header = BTreeMap::new();
    if let Some(user) = user {
        let exp = now() + SSH_TTL_SECS;
        let level = access_name(access);
        let who = hex::encode(user);
        let sig = mac(server, &["lfs-ssh", repo, level, &who], exp);
        header.insert(
            "Authorization",
            format!("Bearer lfs.{level}.{who}.{exp}.{sig}"),
        );
    }
    serde_json::to_string(&SshAuth {
        href: format!("{}/{repo}.git/info/lfs", server.cfg.public_url),
        header,
        expires_in: SSH_TTL_SECS,
    })
    .unwrap()
}

/// The access and user a valid `git-lfs-authenticate` credential grants on `repo`.
fn ssh_credential(server: &Server, headers: &HeaderMap, repo: &str) -> Option<(Access, String)> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer lfs.")?;
    let [level, who, exp, sig] = token.split('.').collect::<Vec<_>>().try_into().ok()?;
    let access = match level {
        "read" => Access::Read,
        "write" => Access::Write,
        _ => return None,
    };
    if !verify(server, &["lfs-ssh", repo, level, who], exp, sig) {
        return None;
    }
    let user = String::from_utf8(hex::decode(who).ok()?).ok()?;
    Some((access, user))
}

fn access_name(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write | Access::Admin => "write",
    }
}

/// Sign `parts` (a tag naming the kind of capability, then fields that never contain
/// a newline) until `exp`.
fn mac(server: &Server, parts: &[&str], exp: u64) -> String {
    let msg = format!("{}\n{exp}", parts.join("\n"));
    hex::encode(server.signer.sign_bytes(msg.as_bytes()))
}

fn verify(server: &Server, parts: &[&str], exp: &str, sig: &str) -> bool {
    let Ok(exp) = exp.parse::<u64>() else {
        return false;
    };
    let want = mac(server, parts, exp);
    exp >= now()
        && want.len() == sig.len()
        && want
            .bytes()
            .zip(sig.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// Whether a transfer URL carries a valid signature for `parts`.
fn signed(server: &Server, parts: &[&str], query: &str) -> bool {
    match (param(query, "exp"), param(query, "sig")) {
        (Some(exp), Some(sig)) => verify(server, parts, exp, sig),
        _ => false,
    }
}

fn refused() -> Response {
    message(StatusCode::FORBIDDEN, "transfer URL is unsigned or expired")
}

async fn download(server: Arc<Server>, name: &str, oid: &str, req: Request) -> Response {
    let query = req.uri().query().unwrap_or("");
    if server.packs_need_auth() && !signed(&server, &["lfs-download", name, oid], query) {
        return refused();
    }
    let Ok(Some(repo)) = server.repo(name) else {
        return message(StatusCode::NOT_FOUND, "repository not found");
    };
    let mut file = match tokio::fs::File::open(object_path(&repo.dir, oid)).await {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return message(StatusCode::NOT_FOUND, "object does not exist");
        }
        Err(e) => return io_error(name, e),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return io_error(name, e),
    };
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes");
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="));
    let (resp, start, end) = match range.map(|r| parse_range(r, len)) {
        None => (resp.header(header::CONTENT_LENGTH, len), 0, len),
        Some(Some((start, end))) => (
            resp.status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{}/{len}", end - 1),
                )
                .header(header::CONTENT_LENGTH, end - start),
            start,
            end,
        ),
        Some(None) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
    };
    if let Err(e) = file.seek(io::SeekFrom::Start(start)).await {
        return io_error(name, e);
    }
    crate::metrics::LFS_BYTES_DOWNLOADED.add(end - start);
    resp.body(Body::from_stream(ReaderStream::new(file.take(end - start))))
        .unwrap()
}

/// `N-`, `N-M` or `-N` (the last N bytes) as a half-open range within `len`.
fn parse_range(spec: &str, len: u64) -> Option<(u64, u64)> {
    let (start, end) = spec.trim().split_once('-')?;
    let (start, end) = match (start, end) {
        ("", n) => (len.saturating_sub(n.parse().ok()?), len),
        (s, "") => (s.parse().ok()?, len),
        (s, e) => (s.parse().ok()?, (e.parse::<u64>().ok()? + 1).min(len)),
    };
    (start < end).then_some((start, end))
}

async fn upload(server: Arc<Server>, name: &str, oid: &str, req: Request) -> Response {
    let query = req.uri().query().unwrap_or("");
    let Some(size) = param(query, "size").and_then(|s| s.parse::<u64>().ok()) else {
        return message(StatusCode::BAD_REQUEST, "missing size");
    };
    let auth_on = server.cfg.auth.is_some();
    if auth_on
        && !signed(
            &server,
            &["lfs-upload", name, oid, &size.to_string()],
            query,
        )
    {
        return refused();
    }
    let Ok(Some(repo)) = server.repo(name) else {
        return message(StatusCode::NOT_FOUND, "repository not found");
    };
    let oid = oid.to_string();
    let open = crate::server::body_reader(req);
    let stored = crate::server::blocking(move || Ok(store(&repo, &oid, size, &mut open())?)).await;
    match stored {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(why)) => message(StatusCode::UNPROCESSABLE_ENTITY, why),
        Err(e) => {
            tracing::warn!(repo = name, "lfs upload failed: {e:#}");
            message(StatusCode::BAD_REQUEST, "upload failed")
        }
    }
}

/// Stream an upload into staging, and move it into place only if it is exactly `size`
/// bytes hashing to `oid`. Returns `Err(why)` for content that does not match.
fn store(
    repo: &Repo,
    oid: &str,
    size: u64,
    body: &mut dyn Read,
) -> io::Result<Result<(), &'static str>> {
    let dest = object_path(&repo.dir, oid);
    if dest.exists() {
        return Ok(Ok(()));
    }
    static STAGED: AtomicU64 = AtomicU64::new(0);
    let tmp = repo.dir.join("lfs/tmp");
    fs::create_dir_all(&tmp)?;
    let mut staged = Staged(tmp.join(format!(
        "{oid}-{}-{}",
        std::process::id(),
        STAGED.fetch_add(1, Ordering::Relaxed)
    )));
    let mut file = fs::File::create_new(&staged.0)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut total = 0u64;
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > size {
            return Ok(Err("object is larger than its declared size"));
        }
        hash.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    if total != size {
        return Ok(Err("object is smaller than its declared size"));
    }
    if hex::encode(hash.finalize()) != oid {
        return Ok(Err("object does not match its oid"));
    }
    file.sync_all()?;
    drop(file);
    if repo.is_closed() {
        return Err(io::Error::other("repository was deleted or renamed"));
    }
    let parent = dest.parent().unwrap();
    fs::create_dir_all(parent)?;
    fs::rename(&staged.0, &dest)?;
    staged.0.clear();
    // Sync every directory up to the repository's, in case this upload created them.
    for dir in dest
        .ancestors()
        .skip(1)
        .take_while(|d| d.starts_with(&repo.dir))
    {
        fs::File::open(dir)?.sync_all()?;
    }
    crate::metrics::LFS_BYTES_UPLOADED.add(size);
    Ok(Ok(()))
}

/// A staging file, removed on drop unless its path was cleared after moving it.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

fn io_error(repo: &str, e: io::Error) -> Response {
    tracing::error!(repo, "lfs: {e}");
    message(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// Every object stored in a repository directory, as (oid, path), sorted.
pub fn stored_objects(repo_dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if depth > 0 {
                walk(&path, depth - 1, out)?;
            } else {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                out.push((name, path));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    let root = repo_dir.join("lfs/objects");
    if root.is_dir() {
        walk(&root, 2, &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Problems with a repository's stored objects: files that are not objects or are
/// out of place, and (with `full`) objects whose contents do not hash to their oid.
pub fn check(repo_dir: &Path, full: bool) -> io::Result<Vec<String>> {
    let mut problems = Vec::new();
    for (oid, path) in stored_objects(repo_dir)? {
        if !valid_oid(&oid) || path != object_path(repo_dir, &oid) {
            problems.push(format!("stray file {}", path.display()));
        } else if full && hash_file(&path)? != oid {
            problems.push(format!("LFS object {oid}: checksum mismatch"));
        }
    }
    Ok(problems)
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hash.finalize()));
        }
        hash.update(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oids_and_ranges_parse_strictly() {
        let oid = "a".repeat(64);
        assert!(valid_oid(&oid));
        assert!(!valid_oid(&"A".repeat(64)));
        assert!(!valid_oid(&"a".repeat(63)));
        assert!(!valid_oid(&format!("../{}", "a".repeat(61))));
        assert_eq!(parse_range("10-", 100), Some((10, 100)));
        assert_eq!(parse_range("10-19", 100), Some((10, 20)));
        assert_eq!(parse_range("-30", 100), Some((70, 100)));
        assert_eq!(parse_range("90-500", 100), Some((90, 100)));
        assert_eq!(parse_range("100-", 100), None);
        assert_eq!(parse_range("x-", 100), None);
    }

    #[test]
    fn paths_split_at_the_lfs_endpoint() {
        assert_eq!(
            parse_path("/team/app.git/info/lfs/objects/batch"),
            Some(("team/app", "objects/batch"))
        );
        assert_eq!(parse_path("/app.git/info/refs"), None);
    }
}
