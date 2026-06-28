# rust-dns

A small, fast DNS resolver in Rust that blocks domains. Point your routers at
it: blocked names get a dummy answer (`0.0.0.0`/`::` or `NXDOMAIN`), and
everything else is forwarded to an upstream resolver and cached. You manage it
from a built-in web portal.

![status](https://img.shields.io/badge/status-alpha-orange)
![built with Rust](https://img.shields.io/badge/built%20with-Rust-CE412B?logo=rust&logoColor=white)
![license](https://img.shields.io/badge/license-MIT-blue)
![PRs welcome](https://img.shields.io/badge/PRs-welcome-brightgreen)

It's meant to be boring in the good way: a single static binary plus two text
files, fast enough to sit in front of a whole network, and simple enough to read
in an afternoon. Contributions are welcome — see [Contributing](#contributing).

## Contents

- [Features](#features)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [The portal](#the-portal)
- [HTTP API](#http-api)
- [Configuration](#configuration)
- [Deploy with systemd](#deploy-with-systemd)
- [Security](#security)
- [How it works](#how-it-works)
- [Notes and tradeoffs](#notes-and-tradeoffs)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)
- [Acknowledgements](#acknowledgements)

## Features

- Blocks a domain and all its subdomains. Block `facebook.com` and you also
  block `www.facebook.com` and the rest; matching costs one hash lookup per
  label. Wildcards work too: `*.example.com` blocks subdomains while the apex
  `example.com` keeps resolving.
- Caches answers in RAM with a per-entry TTL, backed by an embedded `redb`
  store so a restart comes back warm. The in-RAM cache stays well under 1 GB.
- Doesn't hammer your upstream. Three things hold it back: identical concurrent
  misses collapse into one query (single-flight), a semaphore caps in-flight
  queries, and a token bucket caps queries per second.
- Talks plain UDP/TCP to upstream, and retries over TCP when a reply comes back
  truncated.
- Logs every query to columnar Parquet (off the hot path) and lets you search
  it from the portal with DataFusion. Capped at a size you set; oldest segments
  drop first.
- Ships a web portal with token login, a stats dashboard, and screens for the
  blocklist, upstreams, and query logs.
- Runs hot: multi-threaded Tokio, one `SO_REUSEPORT` socket per core, and
  lock-free reads on the query path (`arc-swap`, `moka`).

## Requirements

- A stable Rust toolchain (2021 edition). Install via [rustup](https://rustup.rs).
- Linux for the production features (`SO_REUSEPORT`, the systemd unit). macOS
  works fine for development.
- Binding port 53 needs root or `CAP_NET_BIND_SERVICE`. For local testing, bind
  a high port instead (see below).

## Quick start

```sh
git clone <your-fork-url> rust-dns && cd rust-dns
cargo build --release            # binary: target/release/rust-dns
cp config.example.toml config.toml   # then edit admin_token, upstream, etc.
sudo ./target/release/rust-dns config.toml
```

Config path resolution: `argv[1]`, then `$RUST_DNS_CONFIG`, then `./config.toml`.
A missing `config.toml` or `blocklist.txt` is created with defaults on first run.

To try it without root, set `dns.bind = "127.0.0.1:5353"` and query it:

```sh
dig @127.0.0.1 -p 5353 facebook.com   # -> 0.0.0.0 (blocked)
dig @127.0.0.1 -p 5353 example.com    # -> forwarded + cached
```

## The portal

Open `http://<server>:8080` and sign in with the admin token from
`config.toml`. The sidebar has these screens:

- **Dashboard** shows live counters (queries, blocked, cache hit rate,
  forwarded, cache entries, upstream errors, uptime) and refreshes every few
  seconds.
- **Blocked Domains** lets you add one domain at a time. Each domain gets its own
  row with a remove button, and there's a filter box for long lists. Wildcard
  rows are tagged "subdomains only". Pasted URLs are cleaned, so
  `https://www.youtube.com/watch?v=1` becomes `www.youtube.com`.
- **Upstream DNS** lets you add and remove resolvers one at a time
  (`host:port`, tried in order, changes apply live), and set the timeout, max
  QPS, and concurrency.
- **Logs** shows recent queries newest-first, with filters for domain, client
  IP, and action (blocked / cached / forwarded / error).
- **Settings** holds the sinkhole mode and addresses, plus the read-only DNS and
  web bind addresses.

## HTTP API

Everything under `/api` requires the token, sent as
`Authorization: Bearer <token>` (or an `X-Admin-Token:` header).

| Method | Path | Body / Notes |
|---|---|---|
| GET  | `/api/stats` | counters + cache hit rate |
| GET  | `/api/blocklist` | `{count, domains[]}` |
| POST | `/api/blocklist` | `{"text":"facebook.com\n*.example.com"}` — replaces the list, writes the file, hot-reloads |
| GET  | `/api/config` | upstream + sinkhole view |
| POST | `/api/config` | update upstream servers, timeout, qps, concurrency, sinkhole; persists to `config.toml` |
| GET  | `/api/logs` | recent queries as JSON; filters: `domain`, `client`, `action`, `limit` |

The portal manages domains and resolvers "one at a time" on the surface, but
each add or remove just POSTs the whole list back; the server normalizes and
dedupes it. Upstream servers and sinkhole settings apply live. The other tuning
fields are saved to `config.toml` and take effect on the next restart.

## Configuration

`config.example.toml` lists every field. The ones you'll touch most:

- `dns.sinkhole_mode` — `"zero_ip"` (return `sinkhole_ipv4`/`ipv6`) or `"nxdomain"`.
- `dns.workers` — `0` means one `SO_REUSEPORT` socket per core.
- `dns.max_inflight` — cap on concurrently-handled queries; excess packets are
  dropped (counted as `dropped`) instead of spawning unbounded work.
- `upstream.servers` — plain `host:port` resolvers, tried in order.
- `upstream.max_qps` / `max_concurrent` — the upstream rate and burst limits.
- `cache.max_entries` — caps in-RAM memory (500k is roughly 150 MB).
- `cache.db_path` — the embedded redb file used for durability.
- `cache.flush_ms` — write-behind flush interval; lower loses fewer entries on
  a hard crash.
- `cache.purge_interval_secs` — how often expired rows are deleted from the store.
- `qlog.enabled` — turn query logging on/off.
- `qlog.dir` / `qlog.max_bytes` — log directory and its hard size cap (default 2 GB).
- `qlog.flush_secs` / `qlog.flush_rows` — how often a Parquet segment is written.
- `qlog.log_client_ip` — set `false` to omit client IPs.
- `qlog.mem_limit_mb` — per-query memory ceiling enforced by DataFusion.

## Deploy with systemd

`deploy/rust-dns.service` runs the binary unprivileged but still binds port 53
through `CAP_NET_BIND_SERVICE`.

```sh
sudo mkdir -p /opt/rust-dns
sudo cp target/release/rust-dns config.toml blocklist.txt /opt/rust-dns/
sudo cp deploy/rust-dns.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now rust-dns
```

Run it on as many boxes as you like. To sync a blocklist between them, copy
`blocklist.txt` over with `scp` or `rsync` and it reloads itself, or POST the
list to that server's API.

## Security

- The admin portal binds `127.0.0.1` by default. Only move it to `0.0.0.0`
  behind a firewall or VPN.
- On first run, if `web.admin_token` is empty it generates a strong random token
  and writes it to the config (logged once at startup). There is no usable
  default credential.
- API auth is via the `Authorization: Bearer` (or `X-Admin-Token`) header only —
  the token is never accepted in the URL.
- Inbound DNS is attacker-controlled, so the resolver bounds its work:
  `dns.max_inflight` caps concurrent queries, TCP connections have read/write
  timeouts, the persist and query-log channels are bounded (drops over capacity,
  surfaced as `dropped`), and upstream replies are validated (response bit,
  transaction id, and question must match) before caching.

## How it works

A query takes one path: parse, check the blocklist, check the RAM cache, and
only forward to upstream on a miss.

```
UDP/TCP :53  ->  parse  ->  blocked?  --yes-->  sinkhole answer
                              | no
                              v
                         cache hit?  --yes-->  serve from RAM
                              | no
                              v
                   forward upstream (single-flight + rate limit)  ->  cache + serve
```

The cache is `moka` in RAM for lookups, with an embedded `redb` store behind it
for durability. New entries stream to a write-behind task that batches them into
redb transactions off the query path, and on startup the store is loaded back
into RAM for a warm cache.

The source is small and split by concern: `dns.rs` (listeners + query pipeline),
`blocklist.rs`, `cache.rs` (moka + redb), `upstream.rs` (forwarding + limits),
`qlog.rs` (Parquet logging + DataFusion queries), `web.rs` + `web_assets/`
(portal), and `config.rs`.

## Notes and tradeoffs

- Cached responses store the raw upstream bytes; the transaction ID is patched
  per client when served. That means the first requester's EDNS options get
  baked into the cached entry, which is fine for a uniform internal fleet.
- TTL comes from the smallest answer TTL (negative answers use `negative_ttl`),
  clamped to `[min_ttl, max_ttl]`. Entries serve until they expire, with no
  per-second TTL countdown. For the short TTLs you see internally, that's fine.
- Durability is write-behind: new entries are batched to redb every `flush_ms`
  (default 500 ms) on a background thread, so the query path never waits on disk.
  A hard crash can lose up to that window of new entries, which a cache simply
  re-fetches from upstream. Reads always come from RAM; redb is only the backing
  store, loaded into the cache on startup.

## Roadmap

Rough, unordered, and open to discussion in an issue:

- Encrypted upstream (DoH / DoT).
- A Prometheus `/metrics` endpoint.
- Per-second TTL countdown on cached answers.
- Optional allowlist / per-client rules.

## Contributing

Issues and pull requests are welcome. To get going:

```sh
cargo build           # debug build
cargo test            # run the tests
cargo fmt             # format
cargo clippy          # lint
```

There's also a load harness, ignored by default so it stays out of `cargo test`:

```sh
cargo test --release --test load -- --ignored --nocapture
# tune the burst: RUST_DNS_LOAD_QUERIES=200000 RUST_DNS_LOAD_THREADS=128 ...
```

It prints throughput and latency percentiles (and a single-flight check) but only
fails on correctness, not on perf numbers — those swing too much between machines.

A few asks: keep changes focused, run `cargo fmt` and `cargo clippy` before
opening a PR, and add a test when you fix a bug or add behavior. If you're
planning something larger, open an issue first so we can talk it through.

## License

Released under the [MIT License](LICENSE). Unless you state otherwise, any
contribution you submit is licensed the same way.

## Acknowledgements

Built on the work of others: [Tokio](https://tokio.rs),
[hickory-dns](https://github.com/hickory-dns/hickory-dns) (DNS wire format),
[moka](https://github.com/moka-rs/moka) (cache),
[redb](https://github.com/cberner/redb) (storage),
[axum](https://github.com/tokio-rs/axum) (web), and
[governor](https://github.com/boinkor-net/governor) (rate limiting).
