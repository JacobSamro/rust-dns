//! rust-dns — a small, high-performance blocking DNS resolver with a web admin UI.

mod blocklist;
mod cache;
mod config;
mod dns;
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

    let cfg = Config::load_or_create(&config_path)?;
    let blocklist = Blocklist::from_file(Path::new(&cfg.web.blocklist_path))?;
    tracing::info!("loaded {} blocked domains", blocklist.len());

    let cache = cache::build(&cfg.cache);
    cache::load_snapshot(&cache, Path::new(&cfg.cache.snapshot_path)).await;

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
        upstream,
        stats: Arc::new(Stats::default()),
        config_path,
    });

    let dns_addr: SocketAddr = cfg
        .dns
        .bind
        .parse()
        .with_context(|| format!("invalid dns.bind: {}", cfg.dns.bind))?;
    dns::spawn_udp(state.clone(), dns_addr, cfg.dns.workers)?;
    dns::spawn_tcp(state.clone(), dns_addr).await?;

    // Periodic cache snapshot for warm restarts.
    {
        let snap_state = state.clone();
        let interval = cfg.cache.snapshot_interval_secs.max(5);
        let snap_path = cfg.cache.snapshot_path.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(interval));
            tick.tick().await; // skip immediate first tick
            loop {
                tick.tick().await;
                match cache::save_snapshot(&snap_state.cache, Path::new(&snap_path)) {
                    Ok(n) => tracing::debug!("cache snapshot: {n} entries"),
                    Err(e) => tracing::warn!("cache snapshot failed: {e}"),
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
    tracing::info!("shutting down, saving cache snapshot");

    let snap_path = state.config.load().cache.snapshot_path.clone();
    if let Err(e) = cache::save_snapshot(&state.cache, Path::new(&snap_path)) {
        tracing::warn!("final snapshot failed: {e}");
    }
    Ok(())
}
