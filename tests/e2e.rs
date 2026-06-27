//! End-to-end test: boots the real `rust-dns` binary against a local stub
//! upstream and exercises the DNS path and the admin API over the wire.
//!
//! No external network is used — the test runs its own upstream resolver that
//! answers a fixed A record, so blocking, forwarding, caching, wildcards, auth,
//! live blocklist edits, and query logging are all verified deterministically.

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "testtok";
const STUB_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

/// Kills the server when the test ends.
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

/// A minimal upstream that answers every A query with `STUB_IP` and counts hits.
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
                    60,
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

fn dns_query(server: SocketAddr, name: &str, rtype: RecordType) -> Message {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut msg = Message::new(0x1234, MessageType::Query, OpCode::Query);
    msg.metadata.recursion_desired = true;
    let qname = Name::from_ascii(format!("{name}.")).unwrap();
    msg.add_query(Query::query(qname, rtype));
    sock.send_to(&msg.to_vec().unwrap(), server).unwrap();
    let mut buf = [0u8; 1500];
    let (n, _) = sock.recv_from(&mut buf).unwrap();
    Message::from_vec(&buf[..n]).unwrap()
}

fn first_a(msg: &Message) -> Option<Ipv4Addr> {
    msg.answers.iter().find_map(|r| match &r.data {
        RData::A(a) => Some(a.0),
        _ => None,
    })
}

/// Tiny blocking HTTP/1.1 client (Connection: close), no external deps.
fn http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    (status, body)
}

#[test]
fn end_to_end() {
    let hits = Arc::new(AtomicUsize::new(0));
    let stub_port = start_stub_upstream(hits.clone());
    let dns_port = free_udp_port();
    let web_port = free_tcp_port();

    let dir = std::env::temp_dir().join(format!("rustdns-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("blocklist.txt"), "blocked.test\n*.wild.test\n").unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            r#"
[dns]
bind = "127.0.0.1:{dns_port}"
workers = 1
sinkhole_mode = "zero_ip"
sinkhole_ipv4 = "0.0.0.0"
sinkhole_ipv6 = "::"
sinkhole_ttl = 60

[upstream]
servers = ["127.0.0.1:{stub_port}"]
timeout_ms = 1500
max_concurrent = 64
max_qps = 5000

[cache]
max_entries = 10000
db_path = "cache.redb"
flush_ms = 200
purge_interval_secs = 60
min_ttl = 1
max_ttl = 3600
negative_ttl = 5

[web]
bind = "127.0.0.1:{web_port}"
admin_token = "{TOKEN}"
blocklist_path = "blocklist.txt"

[qlog]
enabled = true
dir = "logs"
max_bytes = 104857600
flush_secs = 1
flush_rows = 1
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

    // Wait until the web API answers.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::panic::catch_unwind(|| http(web_port, "GET", "/api/stats", Some(TOKEN), None).0)
            .map(|s| s == 200)
            .unwrap_or(false)
        {
            break;
        }
        assert!(Instant::now() < deadline, "server did not become ready");
        std::thread::sleep(Duration::from_millis(200));
    }

    let dns: SocketAddr = format!("127.0.0.1:{dns_port}").parse().unwrap();

    // 1) Blocked apex + subdomain -> sinkhole 0.0.0.0
    assert_eq!(
        first_a(&dns_query(dns, "blocked.test", RecordType::A)),
        Some(Ipv4Addr::UNSPECIFIED)
    );
    assert_eq!(
        first_a(&dns_query(dns, "www.blocked.test", RecordType::A)),
        Some(Ipv4Addr::UNSPECIFIED)
    );

    // 2) Wildcard: subdomain blocked, apex resolves via upstream
    assert_eq!(
        first_a(&dns_query(dns, "sub.wild.test", RecordType::A)),
        Some(Ipv4Addr::UNSPECIFIED)
    );
    assert_eq!(
        first_a(&dns_query(dns, "wild.test", RecordType::A)),
        Some(STUB_IP)
    );

    // 3) Forward + cache: two identical lookups hit upstream exactly once
    let before = hits.load(Ordering::Relaxed);
    assert_eq!(
        first_a(&dns_query(dns, "cacheme.example", RecordType::A)),
        Some(STUB_IP)
    );
    assert_eq!(
        first_a(&dns_query(dns, "cacheme.example", RecordType::A)),
        Some(STUB_IP)
    );
    let added = hits.load(Ordering::Relaxed) - before;
    assert_eq!(
        added, 1,
        "second identical query should be served from cache"
    );

    // 4) Unblocked name with no match resolves normally
    assert_eq!(
        first_a(&dns_query(dns, "allowed.example", RecordType::A)),
        Some(STUB_IP)
    );

    // 5) API auth
    assert_eq!(
        http(web_port, "GET", "/api/stats", None, None).0,
        401,
        "no token must be 401"
    );
    let (code, body) = http(web_port, "GET", "/api/stats", Some(TOKEN), None);
    assert_eq!(code, 200);
    assert!(body.contains("\"queries\""), "stats body: {body}");

    // 6) Live blocklist edit takes effect
    let (code, _) = http(
        web_port,
        "POST",
        "/api/blocklist",
        Some(TOKEN),
        Some(r#"{"text":"blocked.test\n*.wild.test\nadded.test"}"#),
    );
    assert_eq!(code, 200);
    assert_eq!(
        first_a(&dns_query(dns, "added.test", RecordType::A)),
        Some(Ipv4Addr::UNSPECIFIED)
    );

    // 7) Query log captured the activity (flush_rows=1 -> available quickly)
    std::thread::sleep(Duration::from_millis(800));
    let (code, logs) = http(web_port, "GET", "/api/logs?limit=500", Some(TOKEN), None);
    assert_eq!(code, 200);
    assert!(logs.contains("blocked.test"), "logs missing blocked entry");
    assert!(
        logs.contains("cacheme.example"),
        "logs missing forwarded entry"
    );
    let (code, blocked_logs) = http(
        web_port,
        "GET",
        "/api/logs?action=blocked&limit=500",
        Some(TOKEN),
        None,
    );
    assert_eq!(code, 200);
    assert!(blocked_logs.contains("blocked.test"));
    assert!(
        !blocked_logs.contains("cacheme.example"),
        "action filter leaked non-blocked rows"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
