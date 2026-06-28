# Changelog

All notable changes to rust-dns are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- One-line installer (`install.sh`) for Ubuntu / x86_64 Linux. It resolves the
  latest release, verifies the checksum, installs into `/opt/rust-dns`, frees
  port 53 from `systemd-resolved` if needed, and starts the service — printing
  the generated admin token. Re-running upgrades in place without touching an
  existing config or blocklist.
- Load/stress harness (`tests/load.rs`, ignored by default) that drives a burst
  of concurrent UDP queries through the real binary. It reports throughput and
  latency percentiles but only asserts correctness invariants (no wrong answers,
  drop rate under a ceiling, single-flight holds under a thundering herd) so it
  doesn't flake on noisy runners. A non-gating CI job runs it and posts the
  numbers to the GitHub Actions job summary.

## [0.1.0] - 2026-06-27

First release: a blocking DNS resolver with a web admin portal.

### Security

- Admin portal binds `127.0.0.1` by default; no usable default credential — an
  empty `web.admin_token` generates a strong random token on first run.
- API auth is header-only (`Authorization: Bearer` / `X-Admin-Token`); no
  `?token=` query param.
- The DNS hot path is bounded against overload: `dns.max_inflight` caps
  concurrent queries (excess dropped, counted as `dropped`), TCP connections
  have read/write timeouts, and the persist + query-log channels are bounded.
- Upstream replies are validated (response bit, transaction id, question match)
  before being cached or served.
- The admin UI builds list rows with DOM APIs instead of interpolated HTML
  (no stored XSS); config and blocklist writes are atomic; `POST /api/config`
  rejects an empty or malformed upstream list.

### Added

- DNS resolver over UDP and TCP, built on Tokio. UDP uses `SO_REUSEPORT` with
  one socket and receive loop per core so the kernel spreads load across CPUs.
  TCP falls back automatically when a UDP reply comes back truncated.
- Domain blocking. A plain entry (`facebook.com`) blocks the apex and every
  subdomain; matching is one hash lookup per label.
- Wildcard blocking. `*.example.com` blocks subdomains only, leaving the apex
  `example.com` resolving.
- In-RAM cache (`moka`) with per-entry TTL and single-flight, so a burst of
  identical misses collapses into one upstream query.
- Durable cache backing with embedded `redb`. New entries are batched to disk by
  a write-behind task (off the query path), and reloaded into RAM on startup for
  a warm cache. A background task purges expired rows to keep the file bounded.
- Upstream forwarding to plain UDP/TCP resolvers, tried in order, with a
  concurrency cap (semaphore) and a queries-per-second ceiling (token bucket) to
  avoid overrunning the upstream.
- Configurable sinkhole response: `zero_ip` (return `0.0.0.0` / `::`) or
  `nxdomain`.
- Web admin portal (axum) behind a shared admin token: token login, a live stats
  dashboard, one-at-a-time management of blocked domains and upstream resolvers,
  and sinkhole settings. JSON API under `/api`.
- Hot reload of the blocklist and of upstream/sinkhole settings without a
  restart.
- Config from `config.toml` (created with defaults on first run), blocklist from
  `blocklist.txt`.
- systemd unit (`deploy/rust-dns.service`) that binds port 53 unprivileged via
  `CAP_NET_BIND_SERVICE`.
- Query logging to columnar Parquet (zstd), written off the DNS hot path by a
  background batch writer with a configurable size cap (default 2 GB; oldest
  segments drop first). A Logs page in the portal and a `/api/logs` endpoint,
  backed by DataFusion with a per-query memory ceiling; filter by domain, client
  IP, and action.
- Unit tests for blocklist matching, plus an end-to-end test (`tests/e2e.rs`)
  that boots the real binary against a local stub upstream and checks blocking,
  wildcards, caching, auth, live blocklist edits, and query logging.
- GitHub Actions CI (fmt, clippy, build, test on Ubuntu) and a tag-triggered
  release workflow that publishes an `x86_64-unknown-linux-gnu` build.
