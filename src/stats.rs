//! Lightweight runtime counters, read by the web UI.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub struct Stats {
    pub queries: AtomicU64,
    pub blocked: AtomicU64,
    pub cache_hits: AtomicU64,
    pub forwarded: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub dropped: AtomicU64,
    started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            queries: AtomicU64::new(0),
            blocked: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            upstream_errors: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

#[derive(Serialize)]
pub struct StatsSnapshot {
    pub queries: u64,
    pub blocked: u64,
    pub cache_hits: u64,
    pub forwarded: u64,
    pub upstream_errors: u64,
    pub dropped: u64,
    pub cache_hit_rate: f64,
    pub uptime_secs: u64,
    pub cache_entries: u64,
}

impl Stats {
    pub fn snapshot(&self, cache_entries: u64) -> StatsSnapshot {
        let q = self.queries.load(Ordering::Relaxed);
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let fwd = self.forwarded.load(Ordering::Relaxed);
        let lookups = hits + fwd;
        StatsSnapshot {
            queries: q,
            blocked: self.blocked.load(Ordering::Relaxed),
            cache_hits: hits,
            forwarded: fwd,
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            cache_hit_rate: if lookups > 0 {
                hits as f64 / lookups as f64
            } else {
                0.0
            },
            uptime_secs: self.started.elapsed().as_secs(),
            cache_entries,
        }
    }
}
