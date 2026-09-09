<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/keel-wordmark-dark.svg">
    <img src="docs/assets/keel-wordmark-light.svg" alt="Keel" width="310">
  </picture>
</p>

<p align="center"><sub>Made in Sweden 🇸🇪 with Claude &amp; Love</sub></p>

<p align="center">
  <a href="https://github.com/allmend/keel/releases"><img src="https://img.shields.io/github/v/release/allmend/keel?sort=semver&label=release&color=6366f1" alt="Release"></a>
  <a href="https://github.com/allmend/keel/actions"><img src="https://img.shields.io/github/actions/workflow/status/allmend/keel/release.yml?label=build" alt="Build"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue" alt="License"></a>
  <img src="https://img.shields.io/badge/Rust-000000?logo=rust&logoColor=white" alt="Rust">
</p>

> ⚠️ **Alpha.** Core proxy, TLS + ACME, clustering, and caching work. Expect rough edges and breaking config changes between versions. Not production-ready yet — feedback welcome.

Keel is a fast, modern, self-hosted load balancer, reverse proxy, and API gateway written in Rust on Cloudflare's [Pingora](https://github.com/cloudflare/pingora). It does the things open-source proxies make you pay for or restart for — live backend drain, runtime pool management, automatic TLS, true config hot-swap — from a single static binary.

Part of the [Allmend](https://github.com/allmend) suite of open-source tools.

---

## Features

- HTTP/1.1 + HTTP/2 reverse proxy
- Virtual host routing (SNI + Host header)
- Path-based routing
- Load balancing — round robin, weighted, consistent hash, least-conn
- TCP (L4) proxying — `tcp_pool` listeners with `passthrough`, `terminate`, and `reencrypt` TLS modes; certificates from ACME or files, SNI selection, optional backend verification
- UDP (L4) load balancing — `udp_pool` listeners, per-client flows with idle expiry, shared drain and connection counting
- PROXY Protocol v1/v2 — real client addresses behind an NLB or another proxy, on HTTP, TCP, and UDP listeners
- TLS termination with per-vhost certificates
- ACME / automatic TLS — named issuers (public or internal CAs), HTTP-01 and DNS-01 (RFC 2136 + TSIG, wildcards), renewal at 30% remaining lifetime, standalone certs for TCP/passthrough backends
- HTTP → HTTPS redirect (implicit for ACME vhosts)
- Default vhost action — redirect or static response for unknown hosts, no pool needed
- Graceful shutdown on SIGTERM/SIGINT/SIGQUIT with configurable grace period
- Health checks — `tcp`, `udp`, `http`, `dns`, `ntp`, `icmp`, and `tls` probes; status/body matching, port override, failure reason in `keel status`
- Passive detection — backends ejected after consecutive upstream failures, re-admitted on a timer; never the last one
- Backend drain with live connection tracking
- Config hot reload (SIGHUP or `keel config reload`)
- TLS certificate hot-swap
- Two-tier HTTP cache (memory L1 + disk L2)
- Gateway rules per vhost or route — per-IP rate limiting, request/response header set/remove, path prefix rewriting
- Prometheus metrics (`/metrics`)
- NDJSON access logs, per-vhost
- conf.d config splitting — vhosts, pools, and certificates per team file
- Raft-based clustering with mTLS, encrypted join, automatic voter promotion
- Distributed config push via `keel config push`
- Cluster-replicated ACME certificates and HTTP-01 challenges — leader issues, every node serves and answers validation
- Graceful node removal — `keel cluster stepdown` with quorum-loss protection
- keelctl — remote control over mTLS from mac/Linux/FreeBSD; kubeconfig-style credentials file, per-operator audit log

In the roadmap: gateway authentication (JWT), HTTP/3.

---

## Tech stack

Rust · [Pingora](https://github.com/cloudflare/pingora) · Tokio · rustls + OpenSSL · [openraft](https://github.com/databendlabs/openraft) · Prometheus

Single static binary. Async multithreaded, CPU balanced across cores. Minimal attack surface.

---

## Quick start

The fastest way to try Keel — proxy `:8080` across three test backends with Docker Compose:

```bash
git clone https://github.com/allmend/keel
cd keel
docker compose up --build

curl http://localhost:8080          # round-robins across backend1/2/3
curl http://localhost:9090/metrics  # Prometheus metrics

# Control commands
docker compose exec keel keel status
docker compose exec keel keel backend list --pool web
docker compose exec keel keel backend drain backend1:80 --wait
```

---

## Install

### Container image

```bash
docker pull ghcr.io/allmend/keel:0.12.0
docker run -v /etc/keel:/etc/keel -p 80:80 -p 443:443 ghcr.io/allmend/keel:0.12.0
```

### Prebuilt binaries

Download a Linux binary (x86_64 or arm64) from the
[releases page](https://github.com/allmend/keel/releases) — each release
includes the binary, an example config, and `SHA256SUMS`.

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

---

## Configuration

Minimal config — listen on port 80, proxy to two backends:

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
keel backend drain 10.0.0.1:8080 --wait  # drain a backend, stream live status
keel config reload                       # reload config from disk (same as SIGHUP)
keel config push keel.yaml               # push config to entire cluster via Raft
keel cluster status                      # cluster membership and Raft roles
keel cluster stepdown                    # gracefully leave the cluster (--force to override quorum guard)
```

The same commands work remotely with keelctl over mTLS — create credentials
once on the node, then control the node or cluster from a workstation or CI:

```bash
# on the node (once)
keel credentials create john --endpoint lb1.example.com:10789 > keelconfig

# from anywhere with the keelconfig
keelctl status
keelctl backend drain 10.0.0.1:8080 --wait
keelctl config push keel.yaml
```

See [docs/keelctl.md](docs/keelctl.md).

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
- [Health checks](docs/health-checks.md)
- [TCP proxying (L4)](docs/tcp-proxying.md)
- [UDP proxying (L4)](docs/udp-proxying.md)
- [Clustering](docs/cluster.md)
- [Caching](docs/caching.md)
- [API gateway](docs/gateway.md)
- [Access logging](docs/access-logging.md)
- [Metrics reference](docs/metrics.md)
- [Automatic TLS / ACME](docs/acme.md)
- [CLI reference](docs/cli.md)
- [Remote control / keelctl](docs/keelctl.md)
- [Security hardening](docs/security.md)

---

## Status

Keel is at v0.12.0, alpha quality. Core proxy, TLS + ACME, clustering, caching, TCP and UDP (L4) proxying, and protocol health checks with passive detection are implemented and working. See [CHANGELOG.md](CHANGELOG.md) for known limitations before deploying.

---

## Contributing

Issues and pull requests welcome. Branch off `main`, use [Conventional Commits](https://www.conventionalcommits.org), and keep `main` releasable.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
