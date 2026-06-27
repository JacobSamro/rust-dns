//! rust-dns — a small, high-performance blocking DNS resolver with a web admin UI.

mod blocklist;
mod cache;
mod config;
mod dns;
mod qlog;
mod state;
mod stats;
mod upstream;
mod web;

use crate::blocklist::Blocklist;
use crate::config::Config;
use crate::state::AppState;
use crate::stats::Stats;
use crate::upstream::Upstream;
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Config path: first CLI arg, else $RUST_DNS_CONFIG, else ./config.toml
    let config_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("RUST_DNS_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let mut cfg = Config::load_or_create(&config_path)?;

    // Never run with the old default credential. If the token is unset (or the
    // legacy "change-me"), generate a strong random one and persist it.
    if cfg.web.admin_token.is_empty() || cfg.web.admin_token == "change-me" {
        let token = random_token();
        cfg.web.admin_token = token.clone();
        cfg.save(&config_path)?;
        tracing::warn!("generated admin token (saved to config): {token}");
    }

    let blocklist = Blocklist::from_file(Path::new(&cfg.web.blocklist_path))?;
    tracing::info!("loaded {} blocked domains", blocklist.len());

    let cache = cache::build(&cfg.cache);

    // Durable backing store (redb) + warm load into RAM, capped at the RAM
    // cache size so a large store can't OOM us on startup.
    let store = Arc::new(cache::Store::open(Path::new(&cfg.cache.db_path))?);
    match store.read_all_valid() {
        Ok(mut entries) => {
            entries.truncate(cfg.cache.max_entries as usize);
            let n = entries.len();
            for (k, v) in entries {
                cache.insert(k, Arc::new(v)).await;
            }
            tracing::info!("loaded {n} cache entries from {}", cfg.cache.db_path);
        }
        Err(e) => tracing::warn!("could not load cache store: {e}"),
    }

    let (persist_tx, persist_rx) = tokio::sync::mpsc::channel(cache::PERSIST_CHANNEL_CAP);

    // Query logging (Parquet + DataFusion), optional.
    let qlog_tx = if cfg.qlog.enabled {
        let (tx, rx) = tokio::sync::mpsc::channel(qlog::LOG_CHANNEL_CAP);
        tokio::spawn(qlog::run_writer(cfg.qlog.clone(), rx));
        tracing::info!("query logging on -> {}", cfg.qlog.dir);
        Some(tx)
    } else {
        None
    };

    let inflight = Arc::new(tokio::sync::Semaphore::new(cfg.dns.max_inflight.max(1)));

    let upstream = Upstream::new(
        &cfg.upstream,
        cfg.upstream_addrs(),
        cfg.cache.min_ttl,
        cfg.cache.max_ttl,
        cfg.cache.negative_ttl,
    );

    let state = Arc::new(AppState {
        config: ArcSwap::from_pointee(cfg.clone()),
        blocklist: ArcSwap::from_pointee(blocklist),
        cache: cache.clone(),
        inflight,
        persist: persist_tx,
        qlog: qlog_tx,
        upstream,
        stats: Arc::new(Stats::default()),
        config_path,
    });

    // Write-behind persister.
    tokio::spawn(cache::run_writer(
        store.clone(),
        persist_rx,
        cfg.cache.flush_ms,
    ));

    let dns_addr: SocketAddr = cfg
        .dns
        .bind
        .parse()
        .with_context(|| format!("invalid dns.bind: {}", cfg.dns.bind))?;
    dns::spawn_udp(state.clone(), dns_addr, cfg.dns.workers)?;
    dns::spawn_tcp(state.clone(), dns_addr).await?;

    // Periodically purge expired rows from the store so it stays bounded.
    {
        let purge_store = store.clone();
        let interval = cfg.cache.purge_interval_secs.max(30);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(interval));
            tick.tick().await; // skip immediate first tick
            loop {
                tick.tick().await;
                let s = purge_store.clone();
                match tokio::task::spawn_blocking(move || s.purge_expired()).await {
                    Ok(Ok(n)) if n > 0 => tracing::debug!("purged {n} expired entries"),
                    Ok(Err(e)) => tracing::warn!("purge failed: {e}"),
                    _ => {}
                }
            }
        });
    }

    // Web admin UI.
    {
        let web_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = web::serve(web_state).await {
                tracing::error!("web server stopped: {e}");
            }
        });
    }

    tracing::info!("rust-dns ready");
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    // The write-behind persister flushes on its interval; entries from the last
    // sub-second window may not be on disk, which is fine for a cache.
    Ok(())
}

/// 32 bytes of OS randomness, hex-encoded. Falls back to a time/pid seed only
/// if `/dev/urandom` is unavailable (then logs a warning at the call site).
fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    format!("{seed:032x}")
}
