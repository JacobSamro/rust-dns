//! Shared application state, used by both the DNS path and the web admin API.

use crate::blocklist::Blocklist;
use crate::cache::{DnsCache, PersistTx};
use crate::config::Config;
use crate::qlog::LogTx;
use crate::stats::Stats;
use crate::upstream::Upstream;
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct AppState {
    pub config: ArcSwap<Config>,
    pub blocklist: ArcSwap<Blocklist>,
    pub cache: DnsCache,
    /// Caps concurrently-handled queries; a full pool means packets are dropped.
    pub inflight: Arc<Semaphore>,
    /// Write-behind channel to the redb durability store.
    pub persist: PersistTx,
    /// Query-log channel (None when logging is disabled).
    pub qlog: Option<LogTx>,
    pub upstream: Arc<Upstream>,
    pub stats: Arc<Stats>,
    pub config_path: PathBuf,
}

pub type SharedState = Arc<AppState>;
