//! Shared application state, used by both the DNS path and the web admin API.

use crate::blocklist::Blocklist;
use crate::cache::{DnsCache, PersistTx};
use crate::config::Config;
use crate::stats::Stats;
use crate::upstream::Upstream;
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct AppState {
    pub config: ArcSwap<Config>,
    pub blocklist: ArcSwap<Blocklist>,
    pub cache: DnsCache,
    /// Write-behind channel to the redb durability store.
    pub persist: PersistTx,
    pub upstream: Arc<Upstream>,
    pub stats: Arc<Stats>,
    pub config_path: PathBuf,
}

pub type SharedState = Arc<AppState>;
