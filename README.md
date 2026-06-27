# rust-dns

A small, high-performance **blocking DNS resolver** in Rust. Point your routers
at it; queries for blocked domains get a dummy answer (`0.0.0.0`/`::` or
`NXDOMAIN`), everything else is forwarded to an upstream resolver and cached.
Manage the blocklist and upstream from a built-in web UI.

Built for two independent, standalone servers — each runs the full binary and is
managed on its own. To replicate, copy `blocklist.txt` to the other box; it
hot-reloads automatically.

## Features

- **Blocking** — block a domain and all its subdomains (`facebook.com` also
  blocks `www.facebook.com`). Subdomain match is O(number of labels).
  Wildcard entries (`*.example.com`) block subdomains only — the apex
  `example.com` keeps resolving.
- **In-RAM cache** with per-entry TTL, plus a disk snapshot for warm restarts
  (bounded well under 1 GB).
- **Upstream protection** so you never overrun the resolver:
  1. **single-flight** — concurrent identical misses collapse to one query,
  2. **concurrency cap** (semaphore),
  3. **hard QPS ceiling** (token bucket).
- **Plain UDP/TCP** upstream, with automatic TCP fallback on truncated answers.
- **Web admin UI** (single page) + JSON API, behind a shared admin token.
- **High throughput** — multi-threaded Tokio, `SO_REUSEPORT` sockets (one
  receive loop per core), lock-free reads on the hot path (`arc-swap`, `moka`).

## Build

```sh
cargo build --release
# binary: target/release/rust-dns
```

## Run

```sh
cp config.example.toml config.toml   # then edit admin_token, upstream, etc.
sudo ./target/release/rust-dns config.toml   # port 53 needs privilege
```

Config path resolution: `argv[1]` → `$RUST_DNS_CONFIG` → `./config.toml`.
A missing `config.toml`/`blocklist.txt` is created with defaults on first run.

For local testing without root, set `dns.bind = "127.0.0.1:5353"`:

```sh
dig @127.0.0.1 -p 5353 facebook.com   # -> 0.0.0.0 (blocked)
dig @127.0.0.1 -p 5353 example.com    # -> forwarded + cached
```

## Web UI

Open `http://<server>:8080`, enter the admin token (from `config.toml`), and you
can edit the blocklist and the upstream/sinkhole settings live.

### API (all under `/api`, require the token)

`Authorization: Bearer <token>` (or `X-Admin-Token:` header, or `?token=`).

| Method | Path | Body / Notes |
|---|---|---|
| GET  | `/api/stats` | counters + cache hit-rate |
| GET  | `/api/blocklist` | `{count, domains[]}` |
| POST | `/api/blocklist` | `{"text":"facebook.com\ntiktok.com"}` — replaces the list, writes the file, hot-reloads |
| GET  | `/api/config` | upstream + sinkhole view |
| POST | `/api/config` | update upstream servers, timeout, qps, concurrency, sinkhole; persists to `config.toml` |

Upstream-server and sinkhole changes apply live. The other tuning fields persist
and take effect on the next restart.

## Configuration

See `config.example.toml` for every field. Key ones:

- `dns.sinkhole_mode` — `"zero_ip"` (return `sinkhole_ipv4`/`ipv6`) or `"nxdomain"`.
- `dns.workers` — `0` = one `SO_REUSEPORT` socket per core.
- `upstream.servers` — plain `host:port` resolvers, tried in order.
- `upstream.max_qps` / `max_concurrent` — upstream rate/burst limits.
- `cache.max_entries` — caps memory and snapshot size (500k ≈ ~150 MB).
- `cache.snapshot_*` — warm-restart persistence.

## Deploy (systemd)

`deploy/rust-dns.service` runs the binary unprivileged while still binding port
53 via `CAP_NET_BIND_SERVICE`, and sends `SIGINT` on stop so the cache snapshot
flushes.

```sh
sudo mkdir -p /opt/rust-dns
sudo cp target/release/rust-dns config.toml blocklist.txt /opt/rust-dns/
sudo cp deploy/rust-dns.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now rust-dns
```

Repeat on the second server. To sync the blocklist, copy `blocklist.txt` over
(`scp`/`rsync`) — it reloads on its own, or POST the list to that server's API.

## Two-server note

The servers are deliberately independent — no shared database, no coordination.
Each is a single static binary plus two text files (`config.toml`,
`blocklist.txt`) and a regenerable `cache.snapshot.json`. That keeps each router
DNS fully self-contained: if one box is down, the other is unaffected.

## Design / tradeoffs

- Cached responses store the upstream wire bytes; the transaction ID is patched
  per client on serve. The first requester's EDNS options are baked into the
  cached entry — fine for a homogeneous internal fleet.
- TTL is taken from the minimum answer TTL (negative answers use `negative_ttl`),
  clamped to `[min_ttl, max_ttl]`. Entries serve until expiry without per-second
  TTL decrement — acceptable for short internal TTLs.
