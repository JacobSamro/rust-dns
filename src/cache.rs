//! In-RAM DNS cache (moka) with per-entry TTL and a disk snapshot for warm
//! restarts. moka also provides request-coalescing (single-flight) via
//! `try_get_with`, so a burst of identical misses results in one upstream query.

use crate::config::CacheConfig;
use anyhow::Result;
use moka::future::Cache;
use moka::Expiry;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    pub name: String,
    pub rtype: u16,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    /// Raw DNS wire response (transaction id is patched per client on serve).
    pub wire: Vec<u8>,
    /// Effective TTL used for cache expiry.
    pub ttl_secs: u32,
    /// Absolute expiry (unix seconds) — used for the snapshot.
    pub expires_unix: u64,
}

/// Expiry policy: honor each entry's DNS-derived TTL.
struct TtlExpiry;

impl Expiry<CacheKey, Arc<CachedResponse>> for TtlExpiry {
    fn expire_after_create(
        &self,
        _key: &CacheKey,
        value: &Arc<CachedResponse>,
        _now: Instant,
    ) -> Option<Duration> {
        Some(Duration::from_secs(value.ttl_secs.max(1) as u64))
    }
}

pub type DnsCache = Cache<CacheKey, Arc<CachedResponse>>;

pub fn build(cfg: &CacheConfig) -> DnsCache {
    Cache::builder()
        .max_capacity(cfg.max_entries)
        .expire_after(TtlExpiry)
        .build()
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize)]
struct SnapshotEntry {
    key: CacheKey,
    value: CachedResponse,
}

/// Load a previously written snapshot, skipping expired entries.
pub async fn load_snapshot(cache: &DnsCache, path: &Path) {
    if !path.exists() {
        return;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("could not read cache snapshot: {e}");
            return;
        }
    };
    let entries: Vec<SnapshotEntry> = match serde_json::from_str(&text) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("could not parse cache snapshot: {e}");
            return;
        }
    };
    let now = now_unix();
    let mut loaded = 0u64;
    for mut e in entries {
        if e.value.expires_unix <= now {
            continue;
        }
        // remaining lifetime drives the in-memory TTL
        e.value.ttl_secs = (e.value.expires_unix - now) as u32;
        cache.insert(e.key, Arc::new(e.value)).await;
        loaded += 1;
    }
    tracing::info!("loaded {loaded} cache entries from snapshot");
}

/// Write the current cache to disk (atomic rename), skipping expired entries.
pub fn save_snapshot(cache: &DnsCache, path: &Path) -> Result<u64> {
    let now = now_unix();
    let mut entries = Vec::new();
    for (k, v) in cache.iter() {
        if v.expires_unix <= now {
            continue;
        }
        entries.push(SnapshotEntry {
            key: (*k).clone(),
            value: (*v).clone(),
        });
    }
    let count = entries.len() as u64;
    let tmp = path.with_extension("tmp");
    let text = serde_json::to_string(&entries)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(count)
}
