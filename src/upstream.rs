//! Upstream forwarding with three layers of protection against overrunning the
//! upstream resolver:
//!   1. single-flight (handled by the cache's `try_get_with`)
//!   2. a concurrency cap (semaphore) to bound bursts
//!   3. a hard QPS ceiling (governor rate limiter)
//! Responses are forwarded over UDP, with TCP fallback when truncated.

use crate::cache::{now_unix, CachedResponse};
use crate::config::UpstreamConfig;
use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use hickory_proto::op::{Message, ResponseCode};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::time::timeout;

pub struct Upstream {
    servers: ArcSwap<Vec<SocketAddr>>,
    timeout: Duration,
    limiter: DefaultDirectRateLimiter,
    semaphore: Semaphore,
    min_ttl: u32,
    max_ttl: u32,
    negative_ttl: u32,
}

impl Upstream {
    pub fn new(cfg: &UpstreamConfig, servers: Vec<SocketAddr>, min_ttl: u32, max_ttl: u32, negative_ttl: u32) -> Arc<Upstream> {
        let qps = NonZeroU32::new(cfg.max_qps.max(1)).unwrap();
        Arc::new(Upstream {
            servers: ArcSwap::from_pointee(servers),
            timeout: Duration::from_millis(cfg.timeout_ms),
            limiter: RateLimiter::direct(Quota::per_second(qps)),
            semaphore: Semaphore::new(cfg.max_concurrent.max(1)),
            min_ttl,
            max_ttl,
            negative_ttl,
        })
    }

    /// Replace the upstream server list at runtime (web UI edit).
    pub fn set_servers(&self, servers: Vec<SocketAddr>) {
        self.servers.store(Arc::new(servers));
    }

    /// Forward `query_wire` to upstream and return a cacheable response.
    pub async fn resolve(&self, query_wire: &[u8]) -> Result<Arc<CachedResponse>> {
        let _permit = self.semaphore.acquire().await?;
        self.limiter.until_ready().await;

        let servers = self.servers.load_full();
        if servers.is_empty() {
            return Err(anyhow!("no upstream servers configured"));
        }

        let mut last_err = anyhow!("upstream unreachable");
        for &server in servers.iter() {
            match self.query_udp(server, query_wire).await {
                Ok(wire) => return Ok(Arc::new(self.to_cached(wire))),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    async fn query_udp(&self, server: SocketAddr, query: &[u8]) -> Result<Vec<u8>> {
        let bind = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let sock = UdpSocket::bind(bind).await?;
        sock.connect(server).await?;
        sock.send(query).await?;

        let mut buf = vec![0u8; 4096];
        let n = timeout(self.timeout, sock.recv(&mut buf)).await??;
        buf.truncate(n);

        // TC (truncated) bit set -> retry over TCP for the full answer.
        if n >= 3 && (buf[2] & 0x02) != 0 {
            return self.query_tcp(server, query).await;
        }
        Ok(buf)
    }

    async fn query_tcp(&self, server: SocketAddr, query: &[u8]) -> Result<Vec<u8>> {
        let mut stream = timeout(self.timeout, TcpStream::connect(server)).await??;
        let len = (query.len() as u16).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(query).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 2];
        timeout(self.timeout, stream.read_exact(&mut len_buf)).await??;
        let resp_len = u16::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; resp_len];
        timeout(self.timeout, stream.read_exact(&mut buf)).await??;
        Ok(buf)
    }

    /// Compute the effective TTL and wrap the wire response for caching.
    fn to_cached(&self, wire: Vec<u8>) -> CachedResponse {
        let ttl = self.effective_ttl(&wire);
        CachedResponse {
            ttl_secs: ttl,
            expires_unix: now_unix() + ttl as u64,
            wire,
        }
    }

    fn effective_ttl(&self, wire: &[u8]) -> u32 {
        let parsed = Message::from_vec(wire).ok();
        let ttl = match parsed {
            Some(msg) => {
                if msg.metadata.response_code == ResponseCode::NXDomain || msg.answers.is_empty() {
                    self.negative_ttl
                } else {
                    msg.answers
                        .iter()
                        .map(|r| r.ttl)
                        .min()
                        .unwrap_or(self.negative_ttl)
                }
            }
            None => self.negative_ttl,
        };
        ttl.clamp(self.min_ttl, self.max_ttl)
    }
}
