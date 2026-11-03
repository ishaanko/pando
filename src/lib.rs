//! Pando: a Git backend built around immutable packs and per-ref versions.

pub mod auth;
pub mod compact;
pub mod express;
pub mod gc;
pub mod generate;
pub mod meta;
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
