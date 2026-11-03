//! Pando: a Git backend built around immutable packs and per-ref versions.

pub mod admin;
pub mod auth;
pub mod compact;
pub mod express;
pub mod gc;
pub mod generate;
pub mod hooks;
pub mod meta;
pub mod metrics;
pub mod object;
pub mod odb;
pub mod pack;
pub mod pktline;
pub mod planner;
pub mod receive;
pub mod server;
pub mod ssh;
pub mod store;
pub mod upload;

/// An HTTP client for webhooks and Express origins, with TLS backed by ring.
pub fn http_client() -> reqwest::Client {
    static TLS: std::sync::Once = std::sync::Once::new();
    TLS.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::new()
}
