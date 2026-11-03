use axum::serve::ListenerExt;
use clap::{Parser, Subcommand};
use pando::auth::{Auth, UrlSigner};
use pando::express::{self, Express, Origin};
use pando::server::{Advertise, Config, Server};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    about = "A Git server built on immutable packs and per-ref versions",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve repositories over Git smart HTTP (and SSH with --ssh-listen).
    Serve {
        /// Where repositories are stored (one directory per repository).
        #[arg(long, default_value = "./pando-data")]
        data: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Also serve Git over SSH on this address (e.g. 0.0.0.0:2222).
        #[arg(long)]
        ssh_listen: Option<String>,
        /// URL clients use to reach this server (used in packfile URIs).
        #[arg(long)]
        public_url: Option<String>,
        /// Preferred trunk ref for new repositories.
        #[arg(long, default_value = "refs/heads/main")]
        trunk: String,
        /// Which refs pushers see.
        #[arg(long, value_enum, default_value = "all")]
        advertise: Advertise,
        /// Auth file: `<name> <token or ssh public key> <read|write|admin> [repos]` per
        /// line. Without it, anyone who can reach the server can read and push.
        #[arg(long)]
        auth_file: Option<PathBuf>,
        /// With --auth-file, allow fetches without a token.
        #[arg(long)]
        public_read: bool,
        /// Express node base URLs. Packs are spread across them by rendezvous hashing.
        #[arg(long)]
        express: Vec<String>,
        /// Upper bound between compaction passes, in seconds.
        #[arg(long, default_value_t = 30)]
        compact_interval: u64,
        /// Seconds between garbage collection passes (0 disables).
        #[arg(long, default_value_t = 3600)]
        gc_interval: u64,
        /// Seconds an unreachable pack is kept before deletion. Keep it at least as long
        /// as signed pack URLs live (one hour).
        #[arg(long, default_value_t = 3600)]
        gc_grace: u64,
        /// Secret for signing pack URLs when --auth-file is set. Give Express nodes the
        /// same value. Random per process when unset.
        #[arg(long, env = "PANDO_URL_SECRET")]
        url_secret: Option<String>,
        /// POST a JSON description of every push that moves refs to this URL
        /// (repeatable).
        #[arg(long)]
        webhook: Vec<String>,
        /// Sign webhook bodies (HMAC-SHA256 in X-Pando-Signature-256).
        #[arg(long, env = "PANDO_WEBHOOK_SECRET")]
        webhook_secret: Option<String>,
    },
    /// Copy a data directory (consistent while the server runs).
    Backup {
        #[arg(long, default_value = "./pando-data")]
        data: PathBuf,
        /// Where to write the backup (must not exist or be empty).
        #[arg(long)]
        out: PathBuf,
    },
    /// Turn a backup into a new data directory, and verify it.
    Restore {
        #[arg(long)]
        from: PathBuf,
        /// The new data directory (must not exist or be empty).
        #[arg(long)]
        data: PathBuf,
    },
    /// Check that every pack the metadata needs is present.
    Verify {
        #[arg(long, default_value = "./pando-data")]
        data: PathBuf,
        /// Also check every pack's checksum and index.
        #[arg(long)]
        full: bool,
    },
    /// Serve packs from a local cache, filling from an origin Pando or Express node.
    Express {
        #[arg(long, default_value = "127.0.0.1:8081")]
        listen: String,
        #[arg(long)]
        origin: String,
        #[arg(long, default_value = "./express-cache")]
        cache: PathBuf,
        /// Evict least recently served packs to stay under this size (e.g. 50G).
        #[arg(long, value_parser = parse_size)]
        cache_max_size: Option<u64>,
        /// Require pack URLs signed with this secret (the origin's --url-secret).
        #[arg(long, env = "PANDO_URL_SECRET")]
        url_secret: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pando=info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Serve {
            data,
            listen,
            ssh_listen,
            public_url,
            trunk,
            advertise,
            auth_file,
            public_read,
            express,
            compact_interval,
            gc_interval,
            gc_grace,
            url_secret,
            webhook,
            webhook_secret,
        } => {
            let auth = auth_file.as_deref().map(Auth::load).transpose()?;
            if auth.is_none() {
                tracing::warn!("no --auth-file: anyone who can reach {listen} can read and push");
            }
            let mut cfg = Config::new(
                &data,
                public_url.unwrap_or_else(|| format!("http://{listen}")),
            );
            cfg.trunk = trunk;
            cfg.advertise = advertise;
            cfg.auth = auth;
            cfg.public_read = public_read;
            cfg.express = express;
            cfg.compact_every = Some(Duration::from_secs(compact_interval));
            cfg.gc_every = (gc_interval > 0).then(|| Duration::from_secs(gc_interval));
            cfg.gc_grace = Duration::from_secs(gc_grace);
            cfg.url_secret = url_secret;
            cfg.webhooks = webhook;
            cfg.webhook_secret = webhook_secret;
            let server = Server::new(cfg)?;
            let router = server.router();
            if let Some(addr) = ssh_listen {
                let listener = tokio::net::TcpListener::bind(&addr).await?;
                tracing::info!("ssh on {addr}");
                tokio::spawn(async move {
                    if let Err(e) = pando::ssh::serve(server, listener).await {
                        tracing::error!("ssh server stopped: {e:#}");
                    }
                });
            }
            let listener = tokio::net::TcpListener::bind(&listen).await?.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            tracing::info!("pando serving {} on {listen}", data.display());
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Cmd::Backup { data, out } => {
            let s = pando::backup::backup(&data, &out)?;
            println!(
                "backed up {} repositories ({} packs) to {}",
                s.repos,
                s.packs,
                out.display()
            );
        }
        Cmd::Restore { from, data } => {
            let s = pando::backup::restore(&from, &data)?;
            println!("restored {} repositories into {}", s.repos, data.display());
        }
        Cmd::Verify { data, full } => {
            let problems = pando::backup::verify(&data, full)?;
            for p in &problems {
                println!("{p}");
            }
            if !problems.is_empty() {
                anyhow::bail!("{} problem(s)", problems.len());
            }
            println!("ok");
        }
        Cmd::Express {
            listen,
            origin,
            cache,
            cache_max_size,
            url_secret,
        } => {
            let origin = Origin::Remote {
                base: origin.trim_end_matches('/').to_string(),
                client: pando::http_client(),
            };
            let signer = url_secret.map(|s| UrlSigner::new(s.as_bytes()));
            let mut express = Express::new(origin, Some(cache), signer)?;
            if let Some(max) = cache_max_size {
                express = express.with_max_size(max);
            }
            let router = express::router(Arc::new(express));
            let listener = tokio::net::TcpListener::bind(&listen).await?.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            tracing::info!("express serving on {listen}");
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown())
                .await?;
        }
    }
    Ok(())
}

/// `500M`, `50G`, `1T`, or plain bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let scale: u64 = match unit.trim().to_ascii_uppercase().trim_end_matches('B') {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => return Err(format!("unknown size unit in {s:?}")),
    };
    num.parse::<u64>()
        .map(|n| n.saturating_mul(scale))
        .map_err(|_| format!("bad size {s:?}"))
}

/// Resolves on SIGINT or SIGTERM. In-flight pushes and fetches finish first; a push
/// that has not reached its metadata transaction leaves at most an unreferenced pack.
async fn shutdown() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
    tracing::info!("shutting down");
}
