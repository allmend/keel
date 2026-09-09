# Changelog

All notable changes to Keel are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added

- **PROXY Protocol** (`proxy_protocol: true` on a listener): v1 and v2
  headers are read ahead of everything else and the address they carry
  becomes the client address for forwarded headers, access logs, hashing,
  and passive health. Plain HTTP, `tcp_pool` (all TLS modes), and `udp_pool`
  (per datagram) listeners. Connections and datagrams without a valid header
  are rejected and counted in `keel_proxy_protocol_errors_total`. Not
  available on `tls: true` HTTP listeners, where Pingora's handshake runs
  before Keel sees the bytes.

---

## [0.10.0] — 2026-09-09

### Added

- **DNS-01 challenge and wildcard certificates.** An issuer with
  `challenge: dns-01` publishes `_acme-challenge` TXT records through RFC
  2136 dynamic updates with TSIG (`dns: { type: rfc2136, … }`), which BIND,
  Knot, PowerDNS, and most enterprise DNS accept without a vendor API. Keel
  confirms the record is visible on the primary before the CA is told to
  validate and removes it afterwards. Wildcard vhosts and `certificates:`
  entries are accepted on such issuers.
- **Wildcard vhosts.** `host: "*.example.com"` now matches one label below
  it (`api.example.com`, not `a.b.example.com`) for routing, cache rules,
  access-log labels, and certificate selection by SNI, in the order exact →
  wildcard → `"*"`. Previously such a host only matched the literal string.
- **`keel --version` and `keelctl --version`.**

---

## [0.9.0] — 2026-09-09

### Added

- **Control CA replicated through Raft.** The leader publishes its control
  CA; every node, including late joiners, installs it into `ca_dir` and
  re-keys its remote listener without restart. One keelconfig works against
  every node and `keel credentials create` works on any node.

### Fixed

- **Runtime directories are prepared before the privilege drop.** The root
  process now creates the control-socket, control-CA, and ACME directories
  and assigns them to `keel.user`; the remote control listener could not
  create `/var/lib/keel/control` after the drop and never started.
- **Cluster mode drops privileges.** A cluster node started as root stayed
  root for its whole life: cluster mode bypasses the master/worker model, and
  nothing dropped. It now binds its listeners and ICMP sockets as root,
  drops to `keel.user`, and starts everything else unprivileged, the same
  outcome as standalone workers.

---

## [0.8.0] — 2026-09-09

### Added

- **TCP TLS termination and re-encryption** — `tls_mode: terminate |
  reencrypt` on `tcp_pool` listeners. Keel serves a certificate from its
  store (a `certificates:` entry or a vhost with `tls`), selected by SNI with
  `tls_host` as the fallback for clients that send none; `reencrypt` opens a
  new TLS connection to the backend, verified only with `tls_verify: true`
  (system roots plus `tls_ca`). `certificates:` entries accept `cert`/`key`
  paths for bring-your-own certificates. TCP access log entries gain
  `tls`, `tls_sni`, `tls_version`, `tls_cipher`; new error values
  `tls_handshake` and `upstream_tls`. A client that closes without TLS
  close_notify is logged as a normal end.

### Changed

- **`proxy_protocol: true` is a startup error.** The option was accepted and
  ignored; a listener behind a load balancer sending PROXY Protocol logged
  and forwarded the load balancer's address as the client's. The field stays
  reserved for the implementation.

---

## [0.7.0] — 2026-09-09

### Added

- **Health checks reworked** (`docs/health-checks.md`). New probes: `udp`
  (ICMP port-unreachable detection), `dns` (one A/AAAA query over UDP or
  TCP, healthy on NOERROR, optional expected address), `ntp` (client
  request, kiss-o'-death detected), `icmp` (echo request over ICMP datagram
  sockets, opened by the root master and inherited by the workers; host
  liveness only), `tls` (handshake without chain verification, optional
  `min_days_valid` certificate-expiry check). `http` gains `host`, `tls`,
  `expect_status`, and `expect_body`. `port` overrides the probed port for
  every type. Rounds are jittered ±10% and the first round runs within a
  second of startup. `keel status` shows a health column and the reason for
  the last failed probe. `least_connections` pools now honor health checks
  (they were previously unchecked). Health state is owned by Keel rather than
  Pingora's private table.
- **Passive detection** (`pools.<name>.passive`, on by default): a backend
  whose upstream connections or UDP replies fail 5 times in a row is
  ejected for 30s, then re-admitted; the last available backend of a pool
  is never ejected. Shown as `ejected` in `keel status`; metrics
  `keel_backend_ejected`, `keel_backend_ejections_total`.

### Changed

- **`health_check` is validated strictly.** A field that does not belong to
  the chosen `type` (for example `path` on `tcp`), an unknown `type`, a
  malformed duration, or a zero threshold is a startup error. Previously
  such fields were ignored and malformed durations fell back to defaults.

---

## [0.6.0] — 2026-09-09

### Added

- **UDP (L4) load balancing** — `udp_pool` on a listener forwards datagrams
  to an ordinary pool. One flow per client `ip:port`, pinned to a backend
  until idle for `keel.udp_flow_timeout_seconds` (default 30). Flows share
  backend drain and `keel_active_connections` with HTTP and TCP. Metrics
  `keel_udp_flows_total`, `keel_udp_packets_in_total` /
  `keel_udp_packets_out_total`, `keel_udp_bytes_in_total` /
  `keel_udp_bytes_out_total`, `keel_udp_errors_total`; one access log entry
  per flow in `access_udp_<pool>.log`. Each worker owns one socket of an
  `SO_REUSEPORT` group, so flows stay on one worker. See
  `docs/udp-proxying.md`.
- **TCP (L4) metrics**: `keel_tcp_connections_total`,
  `keel_tcp_bytes_in_total` / `keel_tcp_bytes_out_total` (per pool and
  backend), and `keel_tcp_errors_total` (per pool and reason).
- **Metrics reference** — `docs/metrics.md` lists every exposed metric with
  labels, types, and example queries.

### Fixed

- **Listeners are bound by the root master before the privilege drop.**
  Workers used to bind after dropping to `keel.user`, so ports below 1024
  only worked with `CAP_NET_BIND_SERVICE` or when started non-root, contrary
  to the documentation. On Linux the master now binds every TCP and UDP
  listener before forking; workers inherit the sockets and pass the TCP ones
  to Pingora over a private Unix socket in the control-socket directory
  (`upgrade-<index>.sock`). A replacement worker reuses the index, and
  therefore the UDP socket, of the worker it replaces. The master also
  creates the control-socket directory and assigns it to `keel.user`, so the
  root-owned default in the container image no longer breaks the control
  socket. Unprivileged runs are unchanged.
- **Dockerfile copies the workspace crates.** The builder stage predated the
  `keel-control` / `keelctl` workspace split and failed on the missing
  members.
- **`keel_backend_healthy` is now emitted.** Health-check state transitions
  set the gauge (and log the transition per pool and backend); the metric
  was registered but never updated.

---

## [0.5.0] — 2026-07-06

### Added

- **keelctl — remote control over mTLS.** A dedicated CLI for controlling a
  Keel node or cluster from a workstation or CI job, with binaries for Linux
  (x86_64/arm64, static), macOS (arm64/x86_64), and FreeBSD (x86_64). Same
  commands and output as the on-node `keel` CLI: `status`, `backend
  list/drain --wait`, `config reload/push`, `cluster status/stepdown`.
- **`control.remote` listener.** TCP control endpoint over mandatory mTLS:
  clients must present a certificate signed by the node's control CA, with
  an optional `allow:` source-CIDR restriction. Every remote command is
  audit-logged as `operator@address command`; the local Unix socket is
  unchanged.
- **`keel credentials create <name> --endpoint <host:port>`** issues an
  operator client certificate from the control CA (generated on first use in
  `control.remote.ca_dir`) and prints a **keelconfig** — endpoint, CA cert,
  client cert, and key in one YAML file, kubeconfig-style. keelctl resolves
  it via `--config`, `$KEEL_CONFIG`, `./keelconfig`, or `~/.keel/config`.
- In cluster mode every node with `control.remote` listens; cluster writes
  are forwarded to the leader internally. Nodes have independent control CAs
  today — share `ca_dir` across nodes for a single cluster-wide keelconfig.
  Control-CA replication via Raft is planned.

### Changed

- The repository is a Cargo workspace: `keel` (root), `keel-control` (shared
  protocol), and `keelctl`.

---

## [0.4.0] — 2026-07-06

### Added

- **TCP (L4) passthrough proxying.** A listener with `tcp_pool: <name>` splices
  raw TCP to a backend pool without inspecting the stream — TLS, if used, is
  end-to-end between client and backend. Pool algorithms, weights, health
  checks, drain (with live connection tracking), and `keel status` all apply
  to TCP connections. One NDJSON access log entry per connection in
  `access_tcp_<pool>.log`. TLS termination and re-encryption at L4 are
  planned.
- **Default vhost action.** `default_action` on a vhost answers requests
  without a backend pool: `redirect: <url>` (301, path + query appended
  unless `preserve_path: false`) or `status:` + `body:` (static response).
  Covers bare-IP redirect, unknown-host 404, and maintenance pages. On a
  wildcard vhost the action fires only for hosts no exact vhost matches.
- **`keel.grace_period_seconds`** (default `10`): how long in-flight requests
  may finish on graceful shutdown before the process exits.

### Fixed

- **SIGTERM and SIGINT trigger graceful shutdown.** The master stops and
  reaps its workers on `SIGTERM`/`SIGINT`/`SIGQUIT`, and the shutdown grace
  period defaults to 10 seconds. `docker stop`, `systemd stop`, and
  Kubernetes pod termination now stop Keel cleanly within their default
  timeouts. (The master previously died on SIGTERM's default action, leaving
  workers running; workers then slept 300 seconds before exiting.)

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

[Unreleased]: https://github.com/allmend/keel/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/allmend/keel/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/allmend/keel/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/allmend/keel/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/allmend/keel/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/allmend/keel/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/allmend/keel/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/allmend/keel/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/allmend/keel/compare/v0.2.0-alpha...v0.3.0
[0.2.0-alpha]: https://github.com/allmend/keel/compare/v0.1.0-alpha...v0.2.0-alpha
[0.1.0-alpha]: https://github.com/allmend/keel/releases/tag/v0.1.0-alpha
