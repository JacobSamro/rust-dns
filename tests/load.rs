//! Load / stress harness for the DNS path. Ignored by default (slow); run with:
//!
//!     cargo test --release --test load -- --ignored --nocapture
//!
//! It boots the real binary against a local stub upstream, then drives a burst
//! of concurrent UDP queries through the hot path. Throughput and latency are
//! reported (to stdout and, in CI, to `$GITHUB_STEP_SUMMARY`) but never
//! asserted — runner noise makes absolute numbers meaningless. What IS asserted
//! are correctness invariants that should hold at any speed:
//!
//!   * every response is correct (blocked -> sinkhole, forwarded -> stub IP),
//!   * the drop rate stays under a generous ceiling, and
//!   * a thundering herd of identical misses collapses to one upstream query
//!     (single-flight held under real contention).

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const TOKEN: &str = "loadtok";
const STUB_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

/// Total queries in the timed phase. Override with `RUST_DNS_LOAD_QUERIES`.
const DEFAULT_QUERIES: usize = 100_000;
/// Concurrent client threads. Override with `RUST_DNS_LOAD_THREADS`.
const DEFAULT_THREADS: usize = 64;
/// Concurrent identical misses in the single-flight herd phase.
const HERD: usize = 200;
/// A query with no reply inside this window counts as a drop.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Max tolerated drop rate before the test fails (well above any healthy run).
const MAX_DROP_RATE: f64 = 0.01;

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Upstream that answers every A query with `STUB_IP` and counts hits.
fn start_stub_upstream(hits: Arc<AtomicUsize>) -> u16 {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        loop {
            let (n, peer) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => continue,
            };
            hits.fetch_add(1, Ordering::Relaxed);
            let req = match Message::from_vec(&buf[..n]) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let Some(q) = req.queries.first().cloned() else {
                continue;
            };
            let mut resp = Message::response(req.metadata.id, req.metadata.op_code);
            resp.metadata.recursion_available = true;
            resp.add_query(q.clone());
            if q.query_type() == RecordType::A {
                resp.add_answer(Record::from_rdata(
                    q.name().clone(),
                    300,
                    RData::A(A(STUB_IP)),
                ));
            }
            if let Ok(bytes) = resp.to_vec() {
                let _ = sock.send_to(&bytes, peer);
            }
        }
    });
    port
}

/// Send one A query and return its first A answer, or `None` on timeout/drop.
fn query_once(sock: &UdpSocket, server: SocketAddr, name: &str) -> Option<Ipv4Addr> {
    let mut msg = Message::new(0x1234, MessageType::Query, OpCode::Query);
    msg.metadata.recursion_desired = true;
    let qname = Name::from_ascii(format!("{name}.")).ok()?;
    msg.add_query(Query::query(qname, RecordType::A));
    sock.send_to(&msg.to_vec().ok()?, server).ok()?;
    let mut buf = [0u8; 1500];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    let resp = Message::from_vec(&buf[..n]).ok()?;
    resp.answers.iter().find_map(|r| match &r.data {
        RData::A(a) => Some(a.0),
        _ => None,
    })
}

fn http_ready(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "GET /api/stats HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .map(|c| c == "200")
        .unwrap_or(false)
}

fn percentile(sorted_micros: &[u64], p: f64) -> f64 {
    if sorted_micros.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted_micros.len() - 1) as f64).round() as usize;
    sorted_micros[idx] as f64 / 1000.0
}

#[test]
#[ignore = "load test; run explicitly with --ignored"]
fn load() {
    let queries: usize = std::env::var("RUST_DNS_LOAD_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_QUERIES);
    let threads: usize = std::env::var("RUST_DNS_LOAD_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_THREADS)
        .max(1);

    let hits = Arc::new(AtomicUsize::new(0));
    let stub_port = start_stub_upstream(hits.clone());
    let dns_port = free_udp_port();
    let web_port = free_tcp_port();

    let dir = std::env::temp_dir().join(format!("rustdns-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("blocklist.txt"), "blocked.test\n").unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            r#"
[dns]
bind = "127.0.0.1:{dns_port}"
workers = 0
sinkhole_mode = "zero_ip"
sinkhole_ipv4 = "0.0.0.0"
sinkhole_ipv6 = "::"
sinkhole_ttl = 60
max_inflight = 8192

[upstream]
servers = ["127.0.0.1:{stub_port}"]
timeout_ms = 1500
max_concurrent = 256
max_qps = 100000

[cache]
max_entries = 500000
db_path = "cache.redb"
flush_ms = 500
purge_interval_secs = 300
min_ttl = 30
max_ttl = 86400
negative_ttl = 60

[web]
bind = "127.0.0.1:{web_port}"
admin_token = "{TOKEN}"
blocklist_path = "blocklist.txt"

[qlog]
enabled = false
dir = "logs"
max_bytes = 104857600
flush_secs = 5
flush_rows = 10000
log_client_ip = true
mem_limit_mb = 64
"#
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_rust-dns");
    let child = std::process::Command::new(bin)
        .arg(dir.join("config.toml"))
        .current_dir(&dir)
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn rust-dns");
    let _server = Server(child);

    let deadline = Instant::now() + Duration::from_secs(30);
    while !http_ready(web_port) {
        assert!(Instant::now() < deadline, "server did not become ready");
        std::thread::sleep(Duration::from_millis(200));
    }

    let dns: SocketAddr = format!("127.0.0.1:{dns_port}").parse().unwrap();

    // ---- Phase 1: single-flight under a thundering herd ----
    // Fire HERD identical queries for one fresh name at the same instant; moka's
    // single-flight should collapse them into exactly one upstream lookup.
    let herd_name = "herd.example";
    let before = hits.load(Ordering::Relaxed);
    let barrier = Arc::new(Barrier::new(HERD));
    let mut handles = Vec::with_capacity(HERD);
    for _ in 0..HERD {
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.set_read_timeout(Some(QUERY_TIMEOUT)).unwrap();
            b.wait();
            query_once(&sock, dns, herd_name)
        }));
    }
    let mut herd_ok = 0usize;
    for h in handles {
        if h.join().unwrap() == Some(STUB_IP) {
            herd_ok += 1;
        }
    }
    let herd_hits = hits.load(Ordering::Relaxed) - before;
    assert!(
        herd_ok > 0,
        "thundering herd got no valid responses ({herd_ok}/{HERD})"
    );
    assert_eq!(
        herd_hits, 1,
        "single-flight failed: {HERD} concurrent misses caused {herd_hits} upstream queries (want 1)"
    );

    // ---- Phase 2: warm the forward set so the timed phase is pure hot path ----
    // Steady state for a resolver is cache hits; warm a pool of names once.
    let forward_names: Vec<String> = (0..256).map(|i| format!("cached{i}.example")).collect();
    {
        let warm = UdpSocket::bind("127.0.0.1:0").unwrap();
        warm.set_read_timeout(Some(QUERY_TIMEOUT)).unwrap();
        for n in &forward_names {
            let _ = query_once(&warm, dns, n);
        }
    }

    // ---- Phase 3: timed burst ----
    // Each thread loops sending a mix of blocked (sinkhole) and warmed-forward
    // (cache hit) queries, recording per-query latency and any drop/mismatch.
    let per_thread = queries / threads;
    let total = per_thread * threads;
    let start = Instant::now();
    let workers: Vec<_> = (0..threads)
        .map(|t| {
            let forward_names = forward_names.clone();
            std::thread::spawn(move || {
                let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
                sock.set_read_timeout(Some(QUERY_TIMEOUT)).unwrap();
                let mut lat = Vec::with_capacity(per_thread);
                let mut drops = 0usize;
                let mut bad = 0usize;
                for i in 0..per_thread {
                    let blocked = (i + t) % 4 == 0;
                    let (name, want) = if blocked {
                        ("blocked.test", Ipv4Addr::UNSPECIFIED)
                    } else {
                        (
                            forward_names[(i + t) % forward_names.len()].as_str(),
                            STUB_IP,
                        )
                    };
                    let q0 = Instant::now();
                    match query_once(&sock, dns, name) {
                        Some(ip) if ip == want => lat.push(q0.elapsed().as_micros() as u64),
                        Some(_) => bad += 1,
                        None => drops += 1,
                    }
                }
                (lat, drops, bad)
            })
        })
        .collect();

    let mut all_lat: Vec<u64> = Vec::with_capacity(total);
    let mut drops = 0usize;
    let mut bad = 0usize;
    for w in workers {
        let (lat, d, b) = w.join().unwrap();
        all_lat.extend(lat);
        drops += d;
        bad += b;
    }
    let elapsed = start.elapsed();

    all_lat.sort_unstable();
    let ok = all_lat.len();
    let qps = ok as f64 / elapsed.as_secs_f64();
    let drop_rate = drops as f64 / total as f64;

    let report = format!(
        "## DNS load test\n\n\
         | metric | value |\n\
         |---|---|\n\
         | queries | {total} |\n\
         | client threads | {threads} |\n\
         | wall time | {:.2} s |\n\
         | throughput | {:.0} qps |\n\
         | latency p50 | {:.3} ms |\n\
         | latency p95 | {:.3} ms |\n\
         | latency p99 | {:.3} ms |\n\
         | latency max | {:.3} ms |\n\
         | dropped | {drops} ({:.3}%) |\n\
         | bad responses | {bad} |\n\
         | single-flight (herd) | {herd_hits} upstream / {HERD} concurrent |\n",
        elapsed.as_secs_f64(),
        qps,
        percentile(&all_lat, 50.0),
        percentile(&all_lat, 95.0),
        percentile(&all_lat, 99.0),
        all_lat.last().copied().unwrap_or(0) as f64 / 1000.0,
        drop_rate * 100.0,
    );
    println!("\n{report}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(report.as_bytes());
        }
    }

    // ---- Correctness invariants (perf numbers above are NOT asserted) ----
    assert_eq!(bad, 0, "{bad} responses had the wrong answer");
    assert!(
        drop_rate <= MAX_DROP_RATE,
        "drop rate {:.3}% exceeded ceiling {:.3}% ({drops}/{total})",
        drop_rate * 100.0,
        MAX_DROP_RATE * 100.0
    );

    let _ = std::fs::remove_dir_all(&dir);
}
