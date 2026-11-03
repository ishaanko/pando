use axum::serve::ListenerExt;
use clap::{Parser, Subcommand};
use pando::express::{self, Express, Origin};
use pando::server;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "A Git backend built on immutable packs and per-ref versions")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve a repository over Git smart HTTP.
    Serve {
        #[arg(long, default_value = "./pando-data")]
        data: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// URL clients use to reach this server (used for packfile URIs when no
        /// --express nodes are given).
        #[arg(long)]
        public_url: Option<String>,
        /// The trunk ref. Only trunk is advertised to pushers.
        #[arg(long, default_value = "refs/heads/main")]
        trunk: String,
        /// Express node base URLs. Packs are spread across them by rendezvous hashing.
        #[arg(long)]
        express: Vec<String>,
        /// Upper bound between compaction passes, in seconds.
        #[arg(long, default_value_t = 30)]
        compact_interval: u64,
    },
    /// Serve packs from a local cache, filling from an origin Pando or Express node.
    Express {
        #[arg(long, default_value = "127.0.0.1:8081")]
        listen: String,
        #[arg(long)]
        origin: String,
        #[arg(long, default_value = "./express-cache")]
        cache: PathBuf,
    },
    /// Run one compaction pass and exit.
    Compact {
        #[arg(long, default_value = "./pando-data")]
        data: PathBuf,
        #[arg(long, default_value = "refs/heads/main")]
        trunk: String,
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
            public_url,
            trunk,
            express,
            compact_interval,
        } => {
            let public_url = public_url.unwrap_or_else(|| format!("http://{listen}"));
            let (app, router) = server::open(&data, &trunk, &public_url, express)?;
            server::spawn_compactor(app, Duration::from_secs(compact_interval));
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!("pando serving {} on {listen}", data.display());
            axum::serve(
                listener.tap_io(|tcp| {
                    let _ = tcp.set_nodelay(true);
                }),
                router,
            )
            .await?;
        }
        Cmd::Express {
            listen,
            origin,
            cache,
        } => {
            let origin = Origin::Remote {
                base: origin.trim_end_matches('/').to_string(),
                client: reqwest::Client::new(),
            };
            let router = express::router(Arc::new(Express::new(origin, Some(cache))?));
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!("express serving on {listen}");
            axum::serve(
                listener.tap_io(|tcp| {
                    let _ = tcp.set_nodelay(true);
                }),
                router,
            )
            .await?;
        }
        Cmd::Compact { data, trunk } => {
            let (app, _) = server::open(&data, &trunk, "http://unused", Vec::new())?;
            let n = pando::compact::run(&app.meta, &app.store)?;
            println!("wrote {n} cover(s)");
        }
    }
    Ok(())
}
