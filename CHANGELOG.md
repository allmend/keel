# Changelog

All notable changes to Keel are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

_Nothing yet._

---

## [0.2.0-alpha] — 2026-07-03

### Added

- **ACME certificates replicate through Raft in cluster mode.** Only the
  leader talks to the CA; issued/renewed certificates are committed to the
  Raft log and every node (including late joiners, via snapshot) writes them
  to its own storage and hot-swaps them. Disk and Raft state reconcile
  continuously — the valid certificate with the most remaining lifetime wins
  and overwrites the other, so a full-cluster restart recovers certificates
  from disk without re-issuing.
- **Top-level `certificates:` section** for standalone certs (hosts Keel
  fronts as TCP/TLS-passthrough without terminating TLS). Mergeable from
  conf.d files like vhosts, so teams declare cert needs next to their pools.
  Replaces `acme.issuers.<name>.domains`.

### Changed

- **BREAKING: ACME config restructured around named issuers.** The flat
  `acme: { email, directory, root_ca, domains }` block becomes
  `acme.issuers.<name>` entries; `tls.acme` accepts `true` (issuer `default`,
  implicitly Let's Encrypt) or an issuer name, so different vhosts can use
  different CAs (Let's Encrypt, staging, an internal CA) side by side. One
  ACME account per issuer, stored under `storage/<issuer>/account.json`.
- **Renewal threshold is now relative by default.** New `acme.renew_before`
  (default `30%`): renew when less than that share of the certificate's
  total lifetime remains — correct for both 90-day and short-lived (6-day)
  certificates. Absolute form (`20d`) also accepted; per-issuer override
  available. Replaces the fixed 30-day rule.

---

## [0.1.0-alpha] — 2026-07-03

First tagged alpha. Everything in 0.1.0 plus the changes below.

### Added

- **Automatic TLS via ACME (Let's Encrypt).** `tls: { acme: true }` on a vhost
  obtains and renews certificates automatically (HTTP-01, renewal 30 days before
  expiry, hot-swapped without restart). Global `acme:` block for account email,
  directory URL (default Let's Encrypt), and storage. `acme.domains` issues
  standalone cert files for hostnames Keel fronts as TCP/TLS-passthrough —
  Keel answers the challenge, backends consume the files (Lego standalone
  style). ACME vhosts redirect HTTP→HTTPS implicitly (challenge path exempt).
- **`keel cluster stepdown [--force]`** — gracefully remove the local node from the
  cluster. Hands leadership over if the node is the leader, commits the removal to
  the Raft log so all remaining nodes accept it, and refuses (without `--force`)
  when the remaining voters would lose quorum.
- **Automatic voter promotion.** Joining nodes start as learners and are promoted
  to voters by the leader once their log catches up, making the documented quorum
  model (3 nodes = 2 of 3, etc.) actually hold. `keel cluster status` now shows
  each member's Raft role (`voter` / `learner`).
- **Release pipeline.** Version tags build fully static binaries (Linux
  x86_64/arm64 MUSL), publish a multi-arch `FROM scratch` container to
  `ghcr.io/allmend/keel`, and create a GitHub Release with checksums. New
  `vendored-openssl` cargo feature for static builds. FreeBSD binaries are
  deferred: pingora-core pins nix 0.24, which no longer compiles for FreeBSD
  against current libc — revisit when upstream updates.
- **LICENSE file** (Apache-2.0, matching the Cargo.toml declaration).

### Fixed

- **Cluster RPC deserialization of membership entries.** The peer RPC envelope used
  an internally-tagged serde enum, which cannot round-trip the integer map keys
  inside Raft membership entries — replicating any membership change failed with
  `invalid type: string "1", expected u64`. The envelope is now externally tagged.
- **Cluster join retries with backoff.** A joiner racing the bootstrap node's
  listener (the normal case under systemd / Compose / K8s) previously failed its
  single join attempt and kept running outside the cluster while looking healthy.
  Joins now retry with exponential backoff (1s → 30s) indefinitely; errors that
  retrying cannot fix (wrong secret, protocol mismatch, explicit rejection) are
  fatal and terminate the process so supervisors notice. The join responder now
  sends a sealed rejection on decrypt failure so a wrong secret is detected
  deterministically instead of surfacing as a connection error.

### Security

- **Host header no longer used as a filesystem or metrics key.** A crafted `Host`
  header could traverse outside the access-log directory, exhaust file descriptors
  and inodes, and explode Prometheus metric cardinality. Requests now map to a
  bounded, operator-configured vhost label for logs and metrics.
- **Cluster join channel is now encrypted.** The join request and response (which
  carries the new node's private key and the cluster CA) were sent in cleartext
  over plain TCP before mTLS existed. They are now AEAD-encrypted with a key
  derived from the shared secret; the secret itself is never transmitted.
- **Cluster mode requires a non-empty shared secret.** Keel refuses to start
  bootstrap/join without one, closing an open-join takeover path.
- **Control socket permissions restricted** to `0660` (dir `0750`).
- **Worker privilege drop hardened** — drops supplementary groups and aborts
  rather than continuing as root on failure.
- **Minimum TLS 1.2** enforced on proxy listeners.
- **Metrics endpoint defaults to `127.0.0.1`** and serves only `GET /metrics`.
- **Length-prefixed cluster reads are capped** before allocation (DoS guard).
- **Corrupt Raft snapshots surface as errors** instead of silently resetting state.

### Changed

- **BREAKING:** clusters previously bootstrapped without `--secret` will no longer
  start. Set `cluster.secret` (or `--secret`) to a high-entropy token.
- **BREAKING:** the metrics endpoint default changed from `0.0.0.0:9090` to
  `127.0.0.1:9090`. Set `metrics.address: 0.0.0.0:9090` to restore remote scrape.

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
