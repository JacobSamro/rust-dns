//! DNS query pipeline + UDP/TCP listeners.
//!
//! Hot path: parse -> blocklist -> cache -> forward. UDP uses SO_REUSEPORT with
//! one socket+receive-loop per worker so the kernel load-balances across cores.

use crate::cache::CacheKey;
use crate::config::DnsConfig;
use crate::qlog::{now_ms, LogRecord};
use crate::state::{AppState, SharedState};
use anyhow::Result;
use hickory_proto::op::{Message, Query, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{RData, Record, RecordType};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

/// Process one raw DNS query, returning the raw response to send back.
/// Returns `None` for unparseable packets (dropped silently).
pub async fn handle(state: &AppState, data: &[u8], client: Option<IpAddr>) -> Option<Vec<u8>> {
    let start = Instant::now();
    let request = Message::from_vec(data).ok()?;
    let query = request.queries.first()?.clone();
    state.stats.queries.fetch_add(1, Relaxed);

    let name = query.name().to_string();
    let name = name.trim_end_matches('.').to_lowercase();
    let qtype = query.query_type();
    let cfg = state.config.load();

    let (response, action) = if state.blocklist.load().is_blocked(&name) {
        state.stats.blocked.fetch_add(1, Relaxed);
        (build_blocked(&request, &query, &cfg.dns), "blocked")
    } else {
        let key = CacheKey {
            name: name.clone(),
            rtype: u16::from(qtype),
        };
        let id = request.metadata.id;

        if let Some(cached) = state.cache.get(&key).await {
            state.stats.cache_hits.fetch_add(1, Relaxed);
            (patch_id(&cached.wire, id), "cached")
        } else {
            // Miss: forward. `try_get_with` collapses concurrent identical
            // misses into one upstream query (single-flight); the init closure
            // runs once per miss, so we persist exactly one copy.
            let data_owned = data.to_vec();
            let upstream = state.upstream.clone();
            let persist = state.persist.clone();
            let pkey = key.clone();
            let result = state
                .cache
                .try_get_with(key, async move {
                    let resp = upstream.resolve(&data_owned).await?;
                    let _ = persist.send((pkey, resp.clone()));
                    Ok::<_, anyhow::Error>(resp)
                })
                .await;
            match result {
                Ok(cached) => {
                    state.stats.forwarded.fetch_add(1, Relaxed);
                    (patch_id(&cached.wire, id), "forwarded")
                }
                Err(e) => {
                    state.stats.upstream_errors.fetch_add(1, Relaxed);
                    tracing::debug!("upstream error: {e}");
                    (
                        build_response_code(&request, &query, ResponseCode::ServFail),
                        "error",
                    )
                }
            }
        }
    };

    if let Some(tx) = &state.qlog {
        let client_str = if cfg.qlog.log_client_ip {
            client.map(|ip| ip.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        let _ = tx.send(LogRecord {
            ts_ms: now_ms(),
            client: client_str,
            domain: name,
            qtype: qtype.to_string(),
            action,
            latency_ms: start.elapsed().as_millis() as u32,
        });
    }

    Some(response)
}

/// Overwrite the 2-byte transaction id so a cached response matches this client.
fn patch_id(wire: &[u8], id: u16) -> Vec<u8> {
    let mut out = wire.to_vec();
    if out.len() >= 2 {
        out[0] = (id >> 8) as u8;
        out[1] = (id & 0xff) as u8;
    }
    out
}

fn base_response(request: &Message, query: &Query) -> Message {
    let mut resp = Message::response(request.metadata.id, request.metadata.op_code);
    resp.metadata.recursion_desired = request.metadata.recursion_desired;
    resp.metadata.recursion_available = true;
    resp.add_query(query.clone());
    resp
}

fn build_blocked(request: &Message, query: &Query, dns: &DnsConfig) -> Vec<u8> {
    let mut resp = base_response(request, query);

    if dns.sinkhole_mode == "nxdomain" {
        resp.metadata.response_code = ResponseCode::NXDomain;
        return resp.to_vec().unwrap_or_default();
    }

    // zero_ip: answer A/AAAA with the configured sinkhole address; other types
    // get an empty NOERROR so resolvers stop chasing the name.
    match query.query_type() {
        RecordType::A => {
            resp.add_answer(Record::from_rdata(
                query.name().clone(),
                dns.sinkhole_ttl,
                RData::A(A(dns.sinkhole_ipv4)),
            ));
        }
        RecordType::AAAA => {
            resp.add_answer(Record::from_rdata(
                query.name().clone(),
                dns.sinkhole_ttl,
                RData::AAAA(AAAA(dns.sinkhole_ipv6)),
            ));
        }
        _ => {}
    }
    resp.to_vec().unwrap_or_default()
}

fn build_response_code(request: &Message, query: &Query, code: ResponseCode) -> Vec<u8> {
    let mut resp = base_response(request, query);
    resp.metadata.response_code = code;
    resp.to_vec().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Listeners
// ---------------------------------------------------------------------------

fn worker_count(configured: usize) -> usize {
    if configured > 0 {
        configured
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

pub fn spawn_udp(state: SharedState, addr: SocketAddr, workers: usize) -> Result<()> {
    let n = worker_count(workers);
    for _ in 0..n {
        let sock = Arc::new(reuseport_udp(addr)?);
        let state = state.clone();
        tokio::spawn(udp_worker(state, sock));
    }
    tracing::info!("DNS UDP listening on {addr} with {n} workers");
    Ok(())
}

fn reuseport_udp(addr: SocketAddr) -> Result<UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    let std_sock: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(std_sock)?)
}

async fn udp_worker(state: SharedState, sock: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match sock.recv_from(&mut buf).await {
            Ok((len, peer)) => {
                let data = buf[..len].to_vec();
                let state = state.clone();
                let sock = sock.clone();
                tokio::spawn(async move {
                    if let Some(resp) = handle(&state, &data, Some(peer.ip())).await {
                        let _ = sock.send_to(&resp, peer).await;
                    }
                });
            }
            Err(e) => tracing::warn!("UDP recv error: {e}"),
        }
    }
}

pub async fn spawn_tcp(state: SharedState, addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("DNS TCP listening on {addr}");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let _ = tcp_conn(&state, stream, peer.ip()).await;
                    });
                }
                Err(e) => tracing::warn!("TCP accept error: {e}"),
            }
        }
    });
    Ok(())
}

async fn tcp_conn(state: &AppState, mut stream: TcpStream, client: IpAddr) -> Result<()> {
    loop {
        let mut len_buf = [0u8; 2];
        if stream.read_exact(&mut len_buf).await.is_err() {
            break; // connection closed
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        if let Some(resp) = handle(state, &buf, Some(client)).await {
            let rlen = (resp.len() as u16).to_be_bytes();
            stream.write_all(&rlen).await?;
            stream.write_all(&resp).await?;
            stream.flush().await?;
        }
    }
    Ok(())
}
