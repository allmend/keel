# Changelog

All notable changes to Keel are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

_Nothing yet._

---

## [0.3.0] — 2026-07-06

### Added

- **HTTP-01 challenge tokens replicate through Raft in cluster mode.** The
  leader commits each token to the Raft log and confirms every node holds it
  before telling the CA to validate, so validation requests — which may come
  from multiple vantage points and land on any node — are answered wherever
  they arrive. Port 80 for the domain may reach any cluster node during
  issuance. Tokens are retracted cluster-wide when the order completes.

---

## [0.2.0-alpha] — 2026-07-03

### Added

- **ACME certificates replicate through Raft in cluster mode.** Only the
  leader talks to the CA. Issued and renewed certificates are committed to
  the Raft log; every node — including late joiners, via snapshot — writes
  them to its own storage and hot-swaps them. Disk and Raft state reconcile
  continuously: per hostname, the valid certificate with the most remaining
  lifetime is the source of truth. A full-cluster restart recovers
  certificates from disk without re-issuing.
- **Top-level `certificates:` section** for standalone certs — hostnames Keel
  obtains certificates for without terminating TLS itself (TCP /
  TLS-passthrough backends). Mergeable from conf.d files like vhosts, so
  teams declare cert needs next to their pools.

### Changed

- **BREAKING: ACME config is organized as named issuers.** Each entry under
  `acme.issuers.<name>` defines a CA relationship: directory URL, account
  contact, optional trust root. `tls.acme` takes `true` (the issuer named
  `default`) or an issuer name, so different vhosts can use different CAs —
  a public CA, its staging directory, an internal CA — side by side. One
  ACME account per issuer, stored under `storage/<issuer>/account.json`.
  The flat `acme: { email, directory, root_ca, domains }` form is not
  accepted; `domains` is covered by the `certificates:` section.
- **BREAKING: the renewal threshold is `acme.renew_before`, default `30%`.**
  A percentage renews when less than that share of the certificate's total
  lifetime remains — correct for both 90-day and short-lived (6-day)
  certificates. An absolute form (`20d`) is also accepted. Per-issuer
  override available.

---

## [0.1.0-alpha] — 2026-07-03

First tagged alpha.

### Added

- **Automatic TLS via ACME (HTTP-01).** `tls: { acme: true }` on a vhost
  obtains and renews certificates automatically: HTTP-01 challenge, renewal
  30 days before expiry, hot-swap without restart. Global `acme:` block for
  account email, directory URL (default
  `https://acme-v02.api.letsencrypt.org/directory`), and storage.
  `acme.domains` issues standalone cert files for hostnames Keel fronts as
  TCP/TLS-passthrough — Keel answers the challenge, backends consume the
  files. ACME vhosts redirect HTTP→HTTPS implicitly (challenge path exempt).
- **`keel cluster stepdown [--force]`** — gracefully remove the local node from
  the cluster. Hands leadership over if the node is the leader, commits the
  removal to the Raft log so all remaining nodes accept it, and refuses
  (without `--force`) when the remaining voters would lose quorum.
- **Automatic voter promotion.** Joining nodes start as learners and are
  promoted to voters by the leader once their log catches up, so the
  documented quorum model (3 nodes = 2 of 3, etc.) holds. `keel cluster
  status` shows each member's Raft role (`voter` / `learner`).
- **Release pipeline.** Version tags build fully static binaries (Linux
  x86_64/arm64 MUSL), publish a multi-arch `FROM scratch` container to
  `ghcr.io/allmend/keel`, and create a GitHub Release with checksums. New
  `vendored-openssl` cargo feature for static builds. FreeBSD binaries are
  not included — a build dependency does not currently compile for FreeBSD.
- **LICENSE file** (Apache-2.0, matching the Cargo.toml declaration).

### Fixed

- **Replication of Raft membership entries.** The peer RPC envelope is
  externally tagged; membership entries with integer map keys serialize
  correctly. (Internally-tagged envelopes fail with
  `invalid type: string "1", expected u64`.)
- **Cluster join retries with exponential backoff** (1s doubling to 30s,
  indefinitely), so nodes can be started in any order. Errors that retrying
  cannot fix — wrong secret, protocol mismatch, explicit rejection — are
  fatal and terminate the process so supervisors notice. The join responder
  sends a sealed rejection on decrypt failure, so a wrong secret is detected
  deterministically rather than surfacing as a connection error.

### Security

- **The `Host` header is never used as a filesystem or metrics key.**
  Requests map to a bounded, operator-configured vhost label for access-log
  filenames and metrics labels. This closes path traversal via crafted
  `Host` values, file-descriptor/inode exhaustion through unbounded log
  files, and metric cardinality explosion.
- **The cluster join exchange is encrypted.** Both directions are
  AEAD-encrypted with a key derived from the shared secret; the secret
  itself is never transmitted. The response carries the new node's private
  key and the cluster CA, so it must never travel in cleartext.
- **Cluster mode requires a non-empty shared secret.** Keel refuses to start
  bootstrap or join without one; an open join listener would hand a cluster
  identity to any peer that can reach the port.
- **Control socket permissions restricted** to `0660` (dir `0750`).
- **Fail-closed privilege drop** — workers drop supplementary groups, gid,
  then uid, and abort rather than continue as root on failure.
- **Minimum TLS 1.2** enforced on proxy listeners.
- **Metrics endpoint defaults to `127.0.0.1`** and serves only `GET /metrics`.
- **Length-prefixed cluster reads are capped** before allocation (DoS guard).
- **Corrupt Raft snapshots surface as errors** rather than resetting cluster
  state.

### Changed

- **BREAKING:** cluster mode requires `cluster.secret` (or `--secret`). Use a
  high-entropy token, e.g. `openssl rand -hex 32`.
- **BREAKING:** the metrics endpoint default is `127.0.0.1:9090`. Set
  `metrics.address: 0.0.0.0:9090` for remote scrape, and firewall the port.

---

## 0.1.0 — 2026-05-15

Initial public release. Pre-alpha — core functionality is working but the project
is young. Expect rough edges, breaking config changes between minor versions, and
missing features listed under Known Limitations below.

### Added

**Proxy & routing**
- HTTP/1.1 and HTTP/2 reverse proxy
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
- mTLS for cluster peer communication

**Reliability**
- Health checks — TCP and HTTP, configurable interval/timeout/thresholds
- Backend drain state machine — graceful removal with live connection tracking
- `keel backend drain --wait` streams connection count until drain completes
- Backends transition through `Active → Draining → Removed`

**Operations**
- Config hot reload — SIGHUP or `keel config reload`, no connection drops
- TLS certificate hot-swap on reload
- conf.d config splitting — `include:` globs or `--conf-dir`, alphabetical merge
- PROXY Protocol inbound — real client IP when behind an upstream LB (v1 and v2)
- Forwarded headers — `X-Forwarded-For`, `X-Real-IP`, `X-Forwarded-Proto`, RFC 7239 `Forwarded`

**Caching**
- Two-tier HTTP cache: in-memory L1 (LRU) + disk L2 (sharded, atomic writes, LRU)
- Cache rules per vhost and per route: TTL override, status filter, content-type filter
- RFC-compliant `Cache-Control` / `ETag` / `Vary` handling
- `X-Cache: HIT` / `X-Cache: MISS` response header

**Observability**
- Prometheus-format metrics endpoint (`GET /metrics` on configurable port, default 9090)
- Per-request metrics: method, status, latency, bytes, backend selected
- NDJSON access logs, one file per vhost in `/var/log/keel/`
- Separate error log per vhost for upstream failures
- Structured app logs to stderr

**Clustering**
- Raft consensus — strong consistency for config changes
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
- Single binary, mode determined by flags
- Multi-stage Dockerfile
- Docker Compose stack with three test backends

### Known Limitations

- **Zero-downtime binary upgrade not implemented.** Upgrading Keel requires a
  brief restart. Zero-downtime upgrade via USR2 + fd passing is planned.

- **ACME / automatic TLS not implemented.** Certificates must be provisioned manually
  and placed on disk. ACME (HTTP-01) support is planned for a future release.

- **API gateway features not implemented.** Rate limiting, authentication, and request
  transformation are on the roadmap but not in scope for v0.1.

- **UDP load balancing not implemented.** HTTP/TCP only for v0.1. UDP requires a
  separate code path and is planned post-v1.

- **Raft log is in-memory only.** The cluster re-forms from config on restart.
  Disk-persistent log storage is planned for a future release.

- **Container image is not fully static.** The image has a dynamic OpenSSL
  dependency. A fully static MUSL build (`FROM scratch`) is planned.

- **No integration test suite.** Unit tests cover config parsing. End-to-end tests
  for drain, clustering, and cache behaviour are on the roadmap.

- **Default vhost action not implemented.** A wildcard `host: "*"` vhost currently
  requires a `pool`. Redirect-to-URL and static-response actions without a backend
  pool are planned.

---

[Unreleased]: https://github.com/allmend/keel/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/allmend/keel/compare/v0.2.0-alpha...v0.3.0
[0.2.0-alpha]: https://github.com/allmend/keel/compare/v0.1.0-alpha...v0.2.0-alpha
[0.1.0-alpha]: https://github.com/allmend/keel/releases/tag/v0.1.0-alpha
