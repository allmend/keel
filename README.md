# Keel

> **Alpha.** Core proxy, TLS + ACME, clustering, and caching work. Expect rough edges and breaking config changes between versions. Not recommended for production yet. Feedback welcome.

---

## What is Keel?

A fast, modern, self-hosted load balancer, reverse proxy, and API gateway written in Rust.

- **Live backend drain** — gracefully remove a backend without dropping connections
- **Runtime pool management** — no reload required to change backend state
- **Automatic TLS** — ACME v2 (any compatible CA), issuance to renewal, no restarts
- **Balanced workers** — async multithreaded (Tokio), CPU balanced across cores
- **True config hot-swap** — SIGHUP reloads config and TLS certs without restart
- **Clustering built in** — Raft consensus, mTLS peer mesh, distributed drain, replicated certificates
- **Written in Rust** — memory safe, single static binary, minimal attack surface

Self-hostable · Apache 2.0 · [github.com/allmend/keel](https://github.com/allmend/keel)

---

## Features

- HTTP/1.1 + HTTP/2 reverse proxy
- Virtual host routing (SNI + Host header)
- Path-based routing
- Load balancing — round robin, weighted, consistent hash, least-conn
- TLS termination with per-vhost certificates
- **ACME / automatic TLS** — named issuers (public or internal CAs), HTTP-01, renewal at 30% remaining lifetime, standalone certs for TCP/passthrough backends
- HTTP → HTTPS redirect (implicit for ACME vhosts)
- Health checks — TCP and HTTP
- Backend drain with live connection tracking
- Config hot reload (SIGHUP or `keel config reload`)
- TLS certificate hot-swap
- Two-tier HTTP cache (memory L1 + disk L2)
- Prometheus metrics (`/metrics`)
- NDJSON access logs, per-vhost
- conf.d config splitting — vhosts, pools, and certificates per team file
- Raft-based clustering with mTLS, encrypted join, automatic voter promotion
- Distributed config push via `keel config push`
- Cluster-replicated ACME certificates and HTTP-01 challenges — leader issues, every node serves and answers validation
- Graceful node removal — `keel cluster stepdown` with quorum-loss protection

In the roadmap: API gateway features (rate limiting, auth, transforms), TCP/UDP (L4) load balancing, PROXY protocol parsing, DNS-01/wildcards.

---

## Quick Start

### Container / prebuilt binaries

Run the container:

```bash
docker pull ghcr.io/allmend/keel:0.3.0
docker run -v /etc/keel:/etc/keel -p 80:80 -p 443:443 ghcr.io/allmend/keel:0.3.0
```

Or download a Linux binary (x86_64 or arm64) from the
[releases page](https://github.com/allmend/keel/releases) — each release
includes the binary, an example config, and `SHA256SUMS`.

### Docker Compose (recommended for trying it out)

```bash
git clone https://github.com/allmend/keel
cd keel
docker compose up --build

# Keel is now proxying :8080 → three test backends
curl http://localhost:8080          # round-robins across backend1/2/3
curl http://localhost:9090/metrics  # Prometheus metrics

# Control commands
docker compose exec keel keel status
docker compose exec keel keel backend list --pool web
docker compose exec keel keel backend drain backend1:80 --wait
```

### Build from source

```bash
# Prerequisites: Rust 1.75+, libssl-dev (Linux) or Homebrew OpenSSL (macOS)

# macOS (Apple Silicon)
OPENSSL_DIR=/opt/homebrew/opt/openssl cargo build --release

# macOS (Intel)
OPENSSL_DIR=/usr/local/opt/openssl cargo build --release

# Linux
cargo build --release

./target/release/keel --config keel.yaml
```

### Minimal config

```yaml
# keel.yaml
keel:
  workers: 4

listeners:
  - address: 0.0.0.0:80

pools:
  web:
    backends:
      - address: 127.0.0.1:8080
      - address: 127.0.0.1:8081

vhosts:
  - host: example.com
    pool: web
```

### Automatic TLS

Add three lines and Keel obtains and renews the certificate itself:

```yaml
acme:
  issuers:
    default:
      email: ops@example.com

vhosts:
  - host: example.com
    pool: web
    tls:
      acme: true      # HTTP-01 challenge; renews automatically
```

See [docs/acme.md](docs/acme.md) for named issuers (multiple CAs side by
side), renewal tuning, and certificates for TCP/passthrough backends.

---

## CLI

```bash
keel status                              # node status + pool overview
keel backend list --pool web             # list backends and connection counts
keel backend drain 10.0.0.1:8080 --wait # drain a backend, stream live status
keel config reload                       # reload config from disk (same as SIGHUP)
keel config push keel.yaml               # push config to entire cluster via Raft
keel cluster status                      # cluster membership and Raft roles
keel cluster stepdown                    # gracefully leave the cluster (--force to override quorum guard)
```

---

## Clustering

Three-node cluster with shared-secret bootstrap:

```bash
# Node 1 — bootstrap
keel --config keel.yaml --cluster --bootstrap --secret mytoken

# Node 2, 3 — join
keel --config keel.yaml --cluster --join 10.0.0.1 --secret mytoken
```

All inter-node traffic is mTLS and the join exchange itself is encrypted with a key derived from the shared secret. The cluster CA is generated automatically, or bring your own. Joining nodes retry with backoff (safe to start all nodes at once) and are promoted to Raft voters once caught up. Config changes — and ACME certificates — are committed via Raft and applied on every node; `keel cluster stepdown` removes a node gracefully, refusing (without `--force`) when the remaining nodes would lose quorum.

---

## Documentation

- [Quickstart](docs/quickstart.md)
- [Configuration reference](docs/configuration.md)
- [Virtual hosts](docs/virtual-hosts.md)
- [Load balancing](docs/load-balancing.md)
- [Clustering](docs/cluster.md)
- [Caching](docs/caching.md)
- [Access logging](docs/access-logging.md)
- [Automatic TLS / ACME](docs/acme.md)
- [CLI reference](docs/cli.md)
- [Security hardening](docs/security.md)

---

## Status

Keel is at v0.3.0, alpha quality. Core proxy, TLS + ACME, clustering, and caching are implemented and working. See [CHANGELOG.md](CHANGELOG.md) for known limitations before deploying.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
