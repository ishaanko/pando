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
        /// Secret for signing pack URLs when --auth-file is set. Give Express nodes the
        /// same value. Random per process when unset.
        #[arg(long, env = "PANDO_URL_SECRET")]
        url_secret: Option<String>,
    },
    /// Serve packs from a local cache, filling from an origin Pando or Express node.
    Express {
        #[arg(long, default_value = "127.0.0.1:8081")]
        listen: String,
        #[arg(long)]
        origin: String,
        #[arg(long, default_value = "./express-cache")]
        cache: PathBuf,
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
            url_secret,
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
            cfg.url_secret = url_secret;
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
        Cmd::Express {
            listen,
            origin,
            cache,
            url_secret,
        } => {
            let origin = Origin::Remote {
                base: origin.trim_end_matches('/').to_string(),
                client: reqwest::Client::new(),
            };
            let signer = url_secret.map(|s| UrlSigner::new(s.as_bytes()));
            let router = express::router(Arc::new(Express::new(origin, Some(cache), signer)?));
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
