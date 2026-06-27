//! Configuration: loaded from `config.toml`, hot-swappable at runtime.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub dns: DnsConfig,
    pub upstream: UpstreamConfig,
    pub cache: CacheConfig,
    pub web: WebConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DnsConfig {
    /// Address the DNS server listens on (UDP + TCP). Use :53 in production (needs root).
    pub bind: String,
    /// Number of SO_REUSEPORT sockets / receive loops. 0 = number of CPU cores.
    pub workers: usize,
    /// How blocked domains are answered: "zero_ip" or "nxdomain".
    pub sinkhole_mode: String,
    /// IPv4 returned for blocked A queries when sinkhole_mode = "zero_ip".
    pub sinkhole_ipv4: std::net::Ipv4Addr,
    /// IPv6 returned for blocked AAAA queries when sinkhole_mode = "zero_ip".
    pub sinkhole_ipv6: std::net::Ipv6Addr,
    /// TTL (seconds) put on sinkhole answers.
    pub sinkhole_ttl: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpstreamConfig {
    /// Upstream resolvers, tried in order. e.g. ["1.1.1.1:53", "8.8.8.8:53"].
    pub servers: Vec<String>,
    /// Per-query timeout in milliseconds.
    pub timeout_ms: u64,
    /// Max concurrent in-flight queries toward upstream (burst guard).
    pub max_concurrent: usize,
    /// Hard ceiling on queries-per-second toward upstream.
    pub max_qps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Max number of cached entries (bounds memory + snapshot size well under 1 GB).
    pub max_entries: u64,
    /// Where the warm-start snapshot is written.
    pub snapshot_path: String,
    /// How often (seconds) to write the snapshot.
    pub snapshot_interval_secs: u64,
    /// TTL floor/ceiling applied to upstream answers (seconds).
    pub min_ttl: u32,
    pub max_ttl: u32,
    /// TTL used for negative answers (NXDOMAIN / empty).
    pub negative_ttl: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebConfig {
    /// Address the admin web UI / API listens on.
    pub bind: String,
    /// Shared admin token (Bearer token / ?token=). Empty disables auth.
    pub admin_token: String,
    /// Path to the blocklist file (one domain per line).
    pub blocklist_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dns: DnsConfig {
                bind: "0.0.0.0:53".into(),
                workers: 0,
                sinkhole_mode: "zero_ip".into(),
                sinkhole_ipv4: std::net::Ipv4Addr::UNSPECIFIED,
                sinkhole_ipv6: std::net::Ipv6Addr::UNSPECIFIED,
                sinkhole_ttl: 60,
            },
            upstream: UpstreamConfig {
                servers: vec!["1.1.1.1:53".into(), "8.8.8.8:53".into()],
                timeout_ms: 2000,
                max_concurrent: 256,
                max_qps: 2000,
            },
            cache: CacheConfig {
                max_entries: 500_000,
                snapshot_path: "cache.snapshot.json".into(),
                snapshot_interval_secs: 60,
                min_ttl: 30,
                max_ttl: 86_400,
                negative_ttl: 60,
            },
            web: WebConfig {
                bind: "0.0.0.0:8080".into(),
                admin_token: "change-me".into(),
                blocklist_path: "blocklist.txt".into(),
            },
        }
    }
}

impl Config {
    /// Load from `path`, creating a default file if it does not exist.
    pub fn load_or_create(path: &Path) -> Result<Config> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
        } else {
            let cfg = Config::default();
            cfg.save(path)?;
            tracing::info!("wrote default config to {}", path.display());
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, text).with_context(|| format!("writing config {}", path.display()))?;
        Ok(())
    }

    pub fn upstream_addrs(&self) -> Vec<SocketAddr> {
        self.upstream
            .servers
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    }
}
