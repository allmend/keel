# Changelog

All notable changes to Keel are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

---

## [0.1.0] — 2026-05-15

Initial public release. Pre-alpha — core functionality is working but the project
is young. Expect rough edges, breaking config changes between minor versions, and
missing features listed under Known Limitations below.

### Added

**Proxy & routing**
- HTTP/1.1 and HTTP/2 reverse proxy via Cloudflare's Pingora framework
- Virtual host routing — SNI-based TLS cert selection + Host header matching
- Wildcard vhost support (`host: "*"`)
- Path-based routing within a vhost (`routes:` with prefix matching)

**Load balancing**
- Round robin (weighted)
- Consistent hashing (Ketama)
- Least connections
- Per-backend weights

**TLS**
- TLS termination with per-vhost certificates
- SNI-based certificate selection via hot-swappable cert store
- HTTP → HTTPS redirect per vhost (`redirect_http: true`)
- mTLS for cluster peer communication (rustls)

**Reliability**
- Health checks — TCP and HTTP, configurable interval/timeout/thresholds
- Backend drain state machine — graceful removal with live connection tracking
- `keel backend drain --wait` streams connection count until drain completes
- Backends transition through `Active → Draining → Removed`

**Operations**
- Config hot reload — SIGHUP or `keel config reload`, no connection drops
- TLS certificate hot-swap on reload
- conf.d config splitting — `include:` globs or `--conf-dir`, alphabetical merge
- PROXY Protocol inbound — real client IP when behind a cloud LB (v1 and v2)
- Forwarded headers — `X-Forwarded-For`, `X-Real-IP`, `X-Forwarded-Proto`, RFC 7239 `Forwarded`

**Caching**
- Two-tier HTTP cache: in-memory L1 (LRU) + disk L2 (sharded, atomic writes, LRU)
- Cache rules per vhost and per route: TTL override, status filter, content-type filter
- RFC-compliant `Cache-Control` / `ETag` / `Vary` handling via Pingora
- `X-Cache: HIT` / `X-Cache: MISS` response header

**Observability**
- Prometheus metrics endpoint (`GET /metrics` on configurable port, default 9090)
- Per-request metrics: method, status, latency, bytes, backend selected
- NDJSON access logs, one file per vhost in `/var/log/keel/`
- Separate error log per vhost for upstream failures
- Structured app logs to stderr via `tracing`

**Clustering**
- Raft consensus via openraft — strong consistency for config changes
- Shared-secret bootstrap: auto-generated cluster CA, node certs issued on join
- Bring-your-own CA bootstrap for existing PKI
- Config replication: `keel config push keel.yaml` commits to Raft, applies on all nodes
- Distributed drain: drain commands flow through Raft log, applied atomically cluster-wide
- `keel cluster status` — membership, role, term, leader, last committed index
- AP operation (traffic always flows) with CP writes (config changes require quorum)
- 2-node leader-follower mode: follower never auto-promotes, cluster goes read-only on leader loss

**CLI**
- `keel status` — uptime, pool summary
- `keel backend list --pool <name>` — backend addresses, states, connection counts
- `keel backend drain <addr> [--wait]` — initiate drain, optionally stream live status
- `keel config reload` — trigger hot reload
- `keel config push <file>` — push config to cluster
- `keel cluster status` — cluster health

**Distribution**
- Single static binary, mode determined by flags
- Multi-stage Dockerfile (`debian:bookworm-slim` runtime, `libssl3` for OpenSSL TLS)
- Docker Compose with three whoami backends for local testing

### Known Limitations

- **Zero-downtime binary upgrade (`--upgrade`) not implemented.** The flag has been
  removed in this release. Upgrading Keel currently requires a brief restart.
  Zero-downtime upgrade via USR2 + fd passing is planned for v0.2.

- **ACME / automatic TLS not implemented.** Certificates must be provisioned manually
  and placed on disk. Let's Encrypt / HTTP-01 support is planned for a future release.

- **API gateway features not implemented.** Rate limiting, authentication, and request
  transformation are on the roadmap but not in scope for v0.1.

- **UDP load balancing not implemented.** HTTP/TCP only for v0.1. UDP requires a
  separate code path and is planned post-v1.

- **Raft log is in-memory only.** The cluster re-forms from config on restart.
  Disk-persistent log storage is planned for a future release.

- **Container image is not fully static.** The current image uses `debian:bookworm-slim`
  with a dynamic OpenSSL dependency. A fully static MUSL build (`FROM scratch`) is
  planned once Pingora's rustls support for SNI hot-swap is available.

- **No integration test suite.** Unit tests cover config parsing. End-to-end tests
  for drain, clustering, and cache behaviour are on the roadmap.

- **Default vhost action not implemented.** A wildcard `host: "*"` vhost currently
  requires a `pool`. Redirect-to-URL and static-response actions without a backend
  pool are planned.

---

[Unreleased]: https://github.com/allmend/keel/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/allmend/keel/releases/tag/v0.1.0
