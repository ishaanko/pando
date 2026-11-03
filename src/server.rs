//! The HTTP front door: repositories, authentication, and routing.
//!
//! Each repository lives in `<data>/repos/<name>.git/` (a metadata database and a blob
//! store) and is created on its first push. Routes follow Git's smart HTTP layout:
//!
//! - `GET  /<repo>.git/info/refs?service=...`  capability / ref advertisement
//! - `POST /<repo>.git/git-upload-pack`        fetch (see `upload.rs`)
//! - `POST /<repo>.git/git-receive-pack`       push (see `receive.rs`)
//! - `GET  /<repo>.git/pack/<hash>.pack`       stored packs (the local Express origin)

use crate::auth::{Access, Auth, UrlSigner};
use crate::express::{self, Express};
use crate::meta::Meta;
use crate::store::BlobStore;
use crate::{receive, upload};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::io::{StreamReader, SyncIoBridge};

pub const AGENT: &str = concat!("pando/", env!("CARGO_PKG_VERSION"));

/// Which refs pushers see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Advertise {
    /// Every ref, like any Git server. Force pushes, leases and deletes work as usual.
    All,
    /// Only trunk. Every pushed pack then depends on trunk alone, which keeps packs
    /// maximally reusable at very high branch counts. Updating a side branch is
    /// accepted when it fast-forwards, or with `git push -o force`.
    Trunk,
}

pub struct Config {
    pub data: PathBuf,
    /// Preferred trunk ref for new repositories. If a repository's first push does not
    /// include it, `refs/heads/master` or the first pushed branch is used instead.
    pub trunk: String,
    /// URL clients use to reach this server; packfile URIs point here when no Express
    /// nodes are configured.
    pub public_url: String,
    pub express: Vec<String>,
    pub advertise: Advertise,
    /// `None` leaves the server open to anyone who can reach it.
    pub auth: Option<Auth>,
    /// With auth on, let unauthenticated clients fetch.
    pub public_read: bool,
    /// Background compaction period. `None` disables the background compactor.
    pub compact_every: Option<Duration>,
    /// Garbage collection period. `None` disables background collection.
    pub gc_every: Option<Duration>,
    /// How long a pack stays unreachable before it is deleted.
    pub gc_grace: Duration,
    /// Secret for signing pack URLs, shared with Express nodes. Random when unset.
    pub url_secret: Option<String>,
    /// URLs that get a POST after every push that moves refs (see `hooks.rs`).
    pub webhooks: Vec<String>,
    /// Signs webhook bodies when set.
    pub webhook_secret: Option<String>,
}

impl Config {
    pub fn new(data: impl Into<PathBuf>, public_url: impl Into<String>) -> Config {
        Config {
            data: data.into(),
            trunk: "refs/heads/main".into(),
            public_url: public_url.into(),
            express: Vec::new(),
            advertise: Advertise::All,
            auth: None,
            public_read: false,
            compact_every: Some(Duration::from_secs(30)),
            gc_every: Some(Duration::from_secs(3600)),
            gc_grace: Duration::from_secs(3600),
            url_secret: None,
            webhooks: Vec::new(),
            webhook_secret: None,
        }
    }
}

/// One repository.
pub struct Repo {
    pub name: String,
    /// `<data>/repos/<name>.git`
    pub dir: PathBuf,
    /// Set once the repository is deleted or renamed; holders of this handle stop.
    pub closed: std::sync::atomic::AtomicBool,
    pub meta: Mutex<Meta>,
    pub store: BlobStore,
    /// Signalled after trunk moves, to wake the compactor.
    pub compact: Notify,
}

pub struct Server {
    pub cfg: Config,
    repos: Mutex<HashMap<String, Arc<Repo>>>,
    /// Serves this server's own packs; also the origin for remote Express nodes.
    local: Express,
    pub signer: UrlSigner,
    /// For webhooks.
    pub http: reqwest::Client,
}

impl Server {
    pub fn new(mut cfg: Config) -> anyhow::Result<Arc<Server>> {
        std::fs::create_dir_all(cfg.data.join("repos"))?;
        cfg.public_url = cfg.public_url.trim_end_matches('/').to_string();
        for node in &mut cfg.express {
            *node = node.trim_end_matches('/').to_string();
        }
        let root = cfg.data.join("repos");
        let find: express::LocalPacks = Arc::new(move |repo: &str, hash| {
            valid_repo_name(repo).then(|| root.join(format!("{repo}.git/blobs/packs/{hash}.pack")))
        });
        let local = Express::new(express::Origin::Local(find), None, None)?;
        let signer = match &cfg.url_secret {
            Some(s) => UrlSigner::new(s.as_bytes()),
            None => UrlSigner::random()?,
        };
        Ok(Arc::new(Server {
            cfg,
            repos: Mutex::new(HashMap::new()),
            local,
            signer,
            http: crate::http_client(),
        }))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new().fallback(dispatch).with_state(self.clone())
    }

    /// Whether pack downloads need a token or a signed URL.
    pub fn packs_need_auth(&self) -> bool {
        self.cfg.auth.is_some() && !self.cfg.public_read
    }

    /// Base URLs that packfile URIs point at.
    pub fn express_nodes(&self) -> Vec<String> {
        if self.cfg.express.is_empty() {
            vec![self.cfg.public_url.clone()]
        } else {
            self.cfg.express.clone()
        }
    }

    /// Check a request's credentials for `need` on `repo`. Returns the caller's name
    /// (`None` when auth is off or for anonymous public reads).
    pub fn authorize(
        &self,
        headers: &axum::http::HeaderMap,
        repo: &str,
        need: Access,
    ) -> Result<Option<String>, Denied> {
        let Some(auth) = &self.cfg.auth else {
            return Ok(None);
        };
        match auth.check(headers) {
            Some(id) if id.allows(repo, need) => Ok(Some(id.name.clone())),
            Some(id) if id.can_see(repo) => {
                crate::metrics::ACCESS_DENIED.inc();
                tracing::warn!(who = id.name, repo, ?need, "denied: insufficient access");
                Err(Denied::Forbidden(format!(
                    "{} cannot {} {repo}",
                    id.name,
                    match need {
                        Access::Read => "read",
                        Access::Write => "push to",
                        Access::Admin => "administer",
                    }
                )))
            }
            Some(id) => {
                crate::metrics::ACCESS_DENIED.inc();
                tracing::warn!(who = id.name, repo, "denied: repository not in scope");
                Err(Denied::Forbidden(format!(
                    "{} cannot access {repo}",
                    id.name
                )))
            }
            None if need == Access::Read && self.cfg.public_read => Ok(None),
            // Git asks without credentials first, so this is routine, not a failure.
            None => Err(Denied::Unauthenticated),
        }
    }

    /// Every repository on disk, sorted by name.
    pub fn list_repos(&self) -> anyhow::Result<Vec<String>> {
        repo_names(&self.cfg.data)
    }

    /// Take a repository out of service: wait for metadata work in flight, mark it
    /// closed, and move its directory with `relocate`. Holds the repository map, so
    /// nobody reopens it meanwhile.
    fn retire(
        &self,
        name: &str,
        relocate: impl FnOnce(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        let mut repos = self.repos.lock().unwrap();
        let dir = self.cfg.data.join("repos").join(format!("{name}.git"));
        if !valid_repo_name(name) || !dir.join("meta.db").exists() {
            return Ok(false);
        }
        let open = repos.remove(name);
        let _quiet = open.as_ref().map(|r| r.meta.lock().unwrap());
        if let Some(r) = &open {
            r.closed.store(true, std::sync::atomic::Ordering::Release);
        }
        relocate(&dir)?;
        Ok(true)
    }

    /// Delete a repository and everything in it. Returns false if it did not exist.
    pub fn delete_repo(&self, name: &str) -> anyhow::Result<bool> {
        let trash = self.cfg.data.join("trash");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let dest = trash.join(format!("{}-{nanos}", name.replace('/', "%")));
        let deleted = self.retire(name, |dir| {
            std::fs::create_dir_all(&trash)?;
            Ok(std::fs::rename(dir, &dest)?)
        })?;
        if deleted {
            std::fs::remove_dir_all(&dest)?;
            tracing::info!(repo = name, "deleted repository");
        }
        Ok(deleted)
    }

    /// Rename a repository. Fails if the new name is taken.
    pub fn rename_repo(&self, from: &str, to: &str) -> anyhow::Result<bool> {
        anyhow::ensure!(valid_repo_name(to), "invalid repository name {to:?}");
        let dest = self.cfg.data.join("repos").join(format!("{to}.git"));
        anyhow::ensure!(!dest.exists(), "repository {to} already exists");
        let renamed = self.retire(from, |dir| {
            std::fs::create_dir_all(dest.parent().unwrap())?;
            Ok(std::fs::rename(dir, &dest)?)
        })?;
        if renamed {
            tracing::info!(from, to, "renamed repository");
        }
        Ok(renamed)
    }

    /// An existing repository.
    pub fn repo(&self, name: &str) -> anyhow::Result<Option<Arc<Repo>>> {
        self.open_repo(name, false)
    }

    pub fn open_repo(&self, name: &str, create: bool) -> anyhow::Result<Option<Arc<Repo>>> {
        if !valid_repo_name(name) {
            return Ok(None);
        }
        let mut repos = self.repos.lock().unwrap();
        if let Some(r) = repos.get(name) {
            return Ok(Some(r.clone()));
        }
        let dir = self.cfg.data.join("repos").join(format!("{name}.git"));
        if !dir.exists() && !create {
            return Ok(None);
        }
        std::fs::create_dir_all(&dir)?;
        let repo = Arc::new(Repo {
            name: name.to_string(),
            dir: dir.clone(),
            closed: Default::default(),
            meta: Mutex::new(Meta::open(dir.join("meta.db"), &self.cfg.trunk)?),
            store: BlobStore::open(dir.join("blobs"))?,
            compact: Notify::new(),
        });
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            if let Some(every) = self.cfg.compact_every {
                rt.spawn(compactor(repo.clone(), every));
            }
            if let Some(every) = self.cfg.gc_every {
                rt.spawn(collector(repo.clone(), every, self.cfg.gc_grace));
            }
        }
        repos.insert(name.to_string(), repo.clone());
        Ok(Some(repo))
    }
}

/// Why a request was refused.
pub enum Denied {
    /// No valid credentials (HTTP 401, which makes git prompt for them).
    Unauthenticated,
    Forbidden(String),
}

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        match self {
            Denied::Unauthenticated => Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(header::WWW_AUTHENTICATE, "Basic realm=\"pando\"")
                .body(Body::from("authentication required\n"))
                .unwrap(),
            Denied::Forbidden(msg) => (StatusCode::FORBIDDEN, format!("{msg}\n")).into_response(),
        }
    }
}

impl Repo {
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Compact a repository after trunk moves, and at least every `every`.
async fn compactor(repo: Arc<Repo>, every: Duration) {
    loop {
        let _ = tokio::time::timeout(every, repo.compact.notified()).await;
        if repo.is_closed() {
            return;
        }
        let r = repo.clone();
        match tokio::task::spawn_blocking(move || crate::compact::run(&r.meta, &r.store)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(repo = %repo.name, "compaction failed: {e:#}"),
            Err(e) => tracing::error!(repo = %repo.name, "compaction panicked: {e}"),
        }
    }
}

/// Collect garbage every `every`.
async fn collector(repo: Arc<Repo>, every: Duration, grace: Duration) {
    loop {
        tokio::time::sleep(every).await;
        if repo.is_closed() {
            return;
        }
        let r = repo.clone();
        match tokio::task::spawn_blocking(move || crate::gc::run(&r, grace)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(repo = %repo.name, "gc failed: {e:#}"),
            Err(e) => tracing::error!(repo = %repo.name, "gc panicked: {e}"),
        }
    }
}

/// Every repository under a data directory, sorted by name.
pub fn repo_names(data: &Path) -> anyhow::Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            match rel.strip_suffix(".git") {
                Some(name) if path.join("meta.db").exists() => out.push(name.to_string()),
                _ => walk(root, &path, out)?,
            }
        }
        Ok(())
    }
    let root = data.join("repos");
    let mut out = Vec::new();
    if root.exists() {
        walk(&root, &root, &mut out)?;
    }
    out.retain(|n| valid_repo_name(n));
    out.sort();
    Ok(out)
}

/// Repository names: one or more `/`-separated segments of `[A-Za-z0-9._-]`, none
/// starting with a dot.
pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

enum Endpoint {
    InfoRefs,
    UploadPack,
    ReceivePack,
}

fn split_path(path: &str) -> Option<(&str, Endpoint)> {
    let (repo, endpoint) = if let Some(r) = path.strip_suffix("/info/refs") {
        (r, Endpoint::InfoRefs)
    } else if let Some(r) = path.strip_suffix("/git-upload-pack") {
        (r, Endpoint::UploadPack)
    } else {
        (
            path.strip_suffix("/git-receive-pack")?,
            Endpoint::ReceivePack,
        )
    };
    let repo = repo.trim_start_matches('/');
    Some((repo.strip_suffix(".git").unwrap_or(repo), endpoint))
}

async fn dispatch(State(server): State<Arc<Server>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    match path.as_str() {
        "/healthz" => return "ok\n".into_response(),
        "/metrics" => {
            return (
                [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                crate::metrics::render(),
            )
                .into_response();
        }
        p if p.starts_with("/api/") => return crate::admin::handle(server, req).await,
        _ => {}
    }
    if method == Method::GET
        && let Some((repo, hash)) = express::parse_pack_path(&path)
    {
        let query = req.uri().query().unwrap_or("");
        let allowed = !server.packs_need_auth()
            || server.signer.verify(repo, &hash, query)
            || server
                .cfg
                .auth
                .as_ref()
                .and_then(|a| a.check(req.headers()))
                .is_some_and(|i| i.allows(repo, Access::Read));
        if !allowed {
            return (
                StatusCode::UNAUTHORIZED,
                "pack URL is unsigned or expired\n",
            )
                .into_response();
        }
        return express::serve(&server.local, repo, hash, req.headers(), "").await;
    }
    let Some((name, endpoint)) = split_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let query = req.uri().query().unwrap_or("").to_string();
    let service = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("service="))
        .unwrap_or("");
    let push = match (&method, &endpoint) {
        (&Method::GET, Endpoint::InfoRefs) if service == "git-receive-pack" => true,
        (&Method::GET, Endpoint::InfoRefs) if service == "git-upload-pack" => false,
        (&Method::POST, Endpoint::UploadPack) => false,
        (&Method::POST, Endpoint::ReceivePack) => true,
        _ => return (StatusCode::FORBIDDEN, "unsupported service\n").into_response(),
    };

    let need = if push { Access::Write } else { Access::Read };
    let user = match server.authorize(req.headers(), name, need) {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };

    let repo = match server.open_repo(name, push) {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "repository not found\n").into_response(),
        Err(e) => return internal_error(&path, e),
    };
    let result = match endpoint {
        Endpoint::InfoRefs if push => receive::advertise(server.clone(), repo).await,
        Endpoint::InfoRefs => upload::advertise(repo, req.headers()).await,
        Endpoint::UploadPack => upload::handle(server.clone(), repo, req).await,
        Endpoint::ReceivePack => receive::handle(server.clone(), repo, user, req).await,
    };
    result.unwrap_or_else(|e| internal_error(&path, e))
}

fn internal_error(path: &str, e: anyhow::Error) -> Response {
    tracing::error!("{path}: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response()
}

pub fn git_response(content_type: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body.into())
        .unwrap()
}

pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await?
}

/// Wrap a request body as a blocking reader, gunzipping if the client compressed it.
/// The returned opener must run on a blocking thread: the gzip decoder reads its
/// header as soon as it is built, and the bridge cannot block an async worker.
pub fn body_reader(req: Request) -> impl FnOnce() -> Box<dyn BufRead + Send> + Send {
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
