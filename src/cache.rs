//! Caching layer.
//!
//! Lookups hit an in-RAM `moka` cache (per-entry TTL + single-flight). For
//! durability the cache is backed by an embedded `redb` key-value store: new
//! entries are streamed to a background writer that batches them into redb
//! transactions (write-behind), so the query path never touches disk. On
//! startup the store is loaded back into RAM for a warm cache.

use crate::config::CacheConfig;
use anyhow::Result;
use moka::future::Cache;
use moka::Expiry;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub name: String,
    pub rtype: u16,
}

#[derive(Clone)]
pub struct CachedResponse {
    /// Raw DNS wire response (transaction id is patched per client on serve).
    pub wire: Vec<u8>,
    /// Effective TTL used for in-RAM expiry.
    pub ttl_secs: u32,
    /// Absolute expiry (unix seconds), persisted so restarts can drop stale rows.
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

// ---------------------------------------------------------------------------
// Durable backing store (redb)
// ---------------------------------------------------------------------------

const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("dns_cache");

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let db = Database::create(path)?;
        // Make sure the table exists so later read txns don't fail.
        let w = db.begin_write()?;
        {
            let _ = w.open_table(TABLE)?;
        }
        w.commit()?;
        Ok(Store { db })
    }

    /// Persist a batch of entries in a single transaction.
    pub fn write_batch(&self, items: &[(CacheKey, Arc<CachedResponse>)]) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(TABLE)?;
            for (k, v) in items {
                t.insert(enc_key(k).as_slice(), enc_val(v).as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }

    /// Read every non-expired entry, with `ttl_secs` set to remaining lifetime.
    pub fn read_all_valid(&self) -> Result<Vec<(CacheKey, CachedResponse)>> {
        let now = now_unix();
        let r = self.db.begin_read()?;
        let t = match r.open_table(TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, v) = row?;
            if let (Some(key), Some(mut val)) = (dec_key(k.value()), dec_val(v.value())) {
                if val.expires_unix > now {
                    val.ttl_secs = (val.expires_unix - now) as u32;
                    out.push((key, val));
                }
            }
        }
        Ok(out)
    }

    /// Delete expired rows so the file doesn't grow without bound.
    pub fn purge_expired(&self) -> Result<u64> {
        let now = now_unix();
        let mut expired: Vec<Vec<u8>> = Vec::new();
        {
            let r = self.db.begin_read()?;
            let t = r.open_table(TABLE)?;
            for row in t.iter()? {
                let (k, v) = row?;
                if let Some(val) = dec_val(v.value()) {
                    if val.expires_unix <= now {
                        expired.push(k.value().to_vec());
                    }
                }
            }
        }
        if expired.is_empty() {
            return Ok(0);
        }
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(TABLE)?;
            for k in &expired {
                t.remove(k.as_slice())?;
            }
        }
        w.commit()?;
        Ok(expired.len() as u64)
    }
}

// key  = rtype(2 BE) ++ name bytes
// value = expires_unix(8 BE) ++ ttl_secs(4 BE) ++ wire
fn enc_key(k: &CacheKey) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + k.name.len());
    b.extend_from_slice(&k.rtype.to_be_bytes());
    b.extend_from_slice(k.name.as_bytes());
    b
}
fn dec_key(b: &[u8]) -> Option<CacheKey> {
    if b.len() < 2 {
        return None;
    }
    let rtype = u16::from_be_bytes([b[0], b[1]]);
    let name = String::from_utf8(b[2..].to_vec()).ok()?;
    Some(CacheKey { name, rtype })
}
fn enc_val(v: &CachedResponse) -> Vec<u8> {
    let mut b = Vec::with_capacity(12 + v.wire.len());
    b.extend_from_slice(&v.expires_unix.to_be_bytes());
    b.extend_from_slice(&v.ttl_secs.to_be_bytes());
    b.extend_from_slice(&v.wire);
    b
}
fn dec_val(b: &[u8]) -> Option<CachedResponse> {
    if b.len() < 12 {
        return None;
    }
    let expires_unix = u64::from_be_bytes(b[0..8].try_into().ok()?);
    let ttl_secs = u32::from_be_bytes(b[8..12].try_into().ok()?);
    Some(CachedResponse {
        wire: b[12..].to_vec(),
        ttl_secs,
        expires_unix,
    })
}

// ---------------------------------------------------------------------------
// Write-behind plumbing
// ---------------------------------------------------------------------------

pub type PersistTx = mpsc::UnboundedSender<(CacheKey, Arc<CachedResponse>)>;
pub type PersistRx = mpsc::UnboundedReceiver<(CacheKey, Arc<CachedResponse>)>;

const BATCH_MAX: usize = 256;

/// Background writer: batches entries from the channel and commits them to redb.
/// The redb commit (which fsyncs) runs on a blocking thread, off the runtime.
pub async fn run_writer(store: Arc<Store>, mut rx: PersistRx, flush_ms: u64) {
    let mut buf: Vec<(CacheKey, Arc<CachedResponse>)> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(flush_ms.max(50)));
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(item) => {
                    buf.push(item);
                    if buf.len() >= BATCH_MAX {
                        flush(&store, &mut buf).await;
                    }
                }
                None => { flush(&store, &mut buf).await; break; } // senders dropped
            },
            _ = tick.tick() => flush(&store, &mut buf).await,
        }
    }
}

async fn flush(store: &Arc<Store>, buf: &mut Vec<(CacheKey, Arc<CachedResponse>)>) {
    if buf.is_empty() {
        return;
    }
    let batch = std::mem::take(buf);
    let n = batch.len();
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.write_batch(&batch)).await {
        Ok(Ok(())) => tracing::trace!("persisted {n} cache entries"),
        Ok(Err(e)) => tracing::warn!("redb write failed: {e}"),
        Err(e) => tracing::warn!("redb writer join error: {e}"),
    }
}
