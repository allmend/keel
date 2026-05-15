# Keel

> **Pre-alpha.** Keel is still finding its sea legs. Expect rough edges, breaking config changes between versions, and the occasional existential crisis about whether Raft really needs that many log entries. Core proxy, TLS, clustering, and caching work — but we wouldn't stake a production fleet on it just yet. Early feedback very welcome.

---

## What is Keel?

A fast, modern, self-hosted load balancer, reverse proxy, and API gateway built in Rust on Cloudflare's Pingora framework.

- **Live backend drain** — gracefully remove a backend without dropping connections
- **Runtime pool management** — no reload required to change backend state
- **Balanced workers** — async multithreaded (Tokio), not per-process like Nginx
- **True config hot-swap** — SIGHUP reloads config and TLS certs without restart
- **Clustering built in** — Raft consensus, mTLS peer mesh, distributed drain
- **Written in Rust** — memory safe, single static binary, minimal attack surface

Self-hostable · Apache 2.0 · [github.com/allmend/keel](https://github.com/allmend/keel)

---

## Features

- HTTP/1.1 + HTTP/2 reverse proxy
- Virtual host routing (SNI + Host header)
- Path-based routing
- Load balancing — round robin, weighted, consistent hash, least-conn
- TLS termination with per-vhost certificates
- HTTP → HTTPS redirect
- Health checks — TCP and HTTP
- Backend drain with live connection tracking
- Config hot reload (SIGHUP or `keel config reload`)
- TLS certificate hot-swap
- Two-tier HTTP cache (memory L1 + disk L2)
- PROXY Protocol inbound (cloud LB → Keel)
- Prometheus metrics (`/metrics`)
- NDJSON access logs, per-vhost
- conf.d config splitting
- Raft-based clustering with mTLS
- Distributed config push via `keel config push`
- ACME / automatic TLS - In roadmap
- API gateway (rate limiting, auth, transforms) - In roadmap
- UDP load balancing - In roadmap

---

## Quick Start

### Docker Compose (recommended for trying it out)

```bash
git clone https://github.com/allmend/keel
cd keel
docker compose up --build

# Keel is now proxying :8080 → three whoami backends
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

---

## CLI

```bash
keel status                              # node status + pool overview
keel backend list --pool web             # list backends and connection counts
keel backend drain 10.0.0.1:8080 --wait # drain a backend, stream live status
keel config reload                       # reload config from disk (same as SIGHUP)
keel config push keel.yaml               # push config to entire cluster via Raft
keel cluster status                      # cluster membership and Raft state
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

All inter-node traffic is mTLS. The cluster CA is generated automatically from the shared secret, or you can bring your own CA. Config changes are committed via Raft and applied atomically across all nodes.

---

## Documentation

- [Quickstart](docs/quickstart.md)
- [Configuration reference](docs/configuration.md)
- [Virtual hosts](docs/virtual-hosts.md)
- [Load balancing](docs/load-balancing.md)
- [Clustering](docs/cluster.md)
- [Caching](docs/caching.md)
- [Access logging](docs/access-logging.md)
- [CLI reference](docs/cli.md)

---

## Status

Keel is pre-release (v0.1.0-alpha). Core proxy, TLS, clustering, and caching are implemented and working. See [CHANGELOG.md](CHANGELOG.md) for known limitations before deploying.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
