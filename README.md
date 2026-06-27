# rust-dns

A small, fast DNS resolver in Rust that blocks domains. Point your routers at
it: blocked names get a dummy answer (`0.0.0.0`/`::` or `NXDOMAIN`), and
everything else is forwarded to an upstream resolver and cached. You manage it
from a built-in web portal.

## What it does

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
- Ships a web portal with token login, a stats dashboard, and screens for the
  blocklist and upstreams.
- Runs hot: multi-threaded Tokio, one `SO_REUSEPORT` socket per core, and
  lock-free reads on the query path (`arc-swap`, `moka`).

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

Config path resolution: `argv[1]`, then `$RUST_DNS_CONFIG`, then `./config.toml`.
A missing `config.toml` or `blocklist.txt` is created with defaults on first run.

To test without root, set `dns.bind = "127.0.0.1:5353"`:

```sh
dig @127.0.0.1 -p 5353 facebook.com   # -> 0.0.0.0 (blocked)
dig @127.0.0.1 -p 5353 example.com    # -> forwarded + cached
```

## The portal

Open `http://<server>:8080` and sign in with the admin token from
`config.toml`. The sidebar has four screens:

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
- **Settings** holds the sinkhole mode and addresses, plus the read-only DNS and
  web bind addresses.

### API (under `/api`, token required)

`Authorization: Bearer <token>` (or an `X-Admin-Token:` header, or `?token=`).

| Method | Path | Body / Notes |
|---|---|---|
| GET  | `/api/stats` | counters + cache hit rate |
| GET  | `/api/blocklist` | `{count, domains[]}` |
| POST | `/api/blocklist` | `{"text":"facebook.com\n*.example.com"}` — replaces the list, writes the file, hot-reloads |
| GET  | `/api/config` | upstream + sinkhole view |
| POST | `/api/config` | update upstream servers, timeout, qps, concurrency, sinkhole; persists to `config.toml` |

The portal manages domains and resolvers "one at a time" on the surface, but
each add or remove just POSTs the whole list back; the server normalizes and
dedupes it. Upstream servers and sinkhole settings apply live. The other tuning
fields are saved to `config.toml` and take effect on the next restart.

## Configuration

`config.example.toml` lists every field. The ones you'll touch most:

- `dns.sinkhole_mode` — `"zero_ip"` (return `sinkhole_ipv4`/`ipv6`) or `"nxdomain"`.
- `dns.workers` — `0` means one `SO_REUSEPORT` socket per core.
- `upstream.servers` — plain `host:port` resolvers, tried in order.
- `upstream.max_qps` / `max_concurrent` — the upstream rate and burst limits.
- `cache.max_entries` — caps in-RAM memory (500k is roughly 150 MB).
- `cache.db_path` — the embedded redb file used for durability.
- `cache.flush_ms` — write-behind flush interval; lower loses fewer entries on
  a hard crash.
- `cache.purge_interval_secs` — how often expired rows are deleted from the store.

## Deploy (systemd)

`deploy/rust-dns.service` runs the binary unprivileged but still binds port 53
through `CAP_NET_BIND_SERVICE`.

```sh
sudo mkdir -p /opt/rust-dns
sudo cp target/release/rust-dns config.toml blocklist.txt /opt/rust-dns/
sudo cp deploy/rust-dns.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now rust-dns
```

Do the same on the second server. To sync the blocklist, copy `blocklist.txt`
over with `scp` or `rsync` and it reloads itself, or POST the list to that
server's API.


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
