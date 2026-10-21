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

> **Alpha.** Configuration may change between versions.

Keel is a self-hosted load balancer and reverse proxy for HTTP, TCP and UDP, written in Rust on Cloudflare's [Pingora](https://github.com/cloudflare/pingora). One binary, clustered or standalone.

Part of the [Allmend](https://github.com/allmend) suite of open-source tools.

---

## Features

- HTTP/1.1 and HTTP/2 reverse proxy; vhosts by Host header and SNI, path routing
- TCP proxying: `passthrough`, `terminate` and `reencrypt`
- UDP proxying: one flow per client, idle expiry
- Load balancing: round robin, random, least connections, consistent hash; weights
- Health checks: `tcp`, `udp`, `http`, `dns`, `ntp`, `icmp`, `tls`; passive ejection
- Backend drain with connection tracking
- TLS with per-vhost certificates, hot-swapped on reload
- ACME (HTTP-01): named issuers, renewal, certificates for TCP backends
- PROXY protocol v1/v2 on every listener
- HTTP cache: memory and disk
- Config reload without dropped connections
- Clustering: Raft over mTLS; config, drains and certificates replicated
- Prometheus metrics, NDJSON access logs
- `keelctl`: remote control over mTLS

---

## Quick start

Keel and three test backends with Docker Compose:

```bash
git clone https://github.com/allmend/keel
cd keel
docker compose up --build

curl http://localhost:8080
curl http://localhost:10790/metrics

docker compose exec keel keel status
docker compose exec keel keel backend drain 172.29.100.11:80 --wait
```

---

## Install

### Container image

```bash
docker run -v /etc/keel:/etc/keel -p 80:80 -p 443:443 ghcr.io/allmend/keel:<version>
```

`FROM scratch`: a static binary at `/keel`, CA certificates, and users `keel` (10001) and `keel-control` (10002). Started as root it drops to them; see [Users](docs/security.md#users). Client commands run without a shell: `docker exec <container> /keel status`.

### Binaries

Linux (x86_64, arm64) and FreeBSD (x86_64) on the [releases page](https://github.com/allmend/keel/releases), with example config files and `SHA256SUMS`.

### From source

```bash
# Rust 1.90 or later. Linux: libssl-dev. macOS: OPENSSL_DIR=$(brew --prefix openssl)
cargo build --release
./target/release/keel --config node.hcl
```

---

## Configuration

A node file, `/etc/keel/node.hcl`, and the config directory it names, `/etc/keel/config/`. Both are HCL.

```hcl
# /etc/keel/node.hcl
keel {
  workers = 4
}
```

```hcl
# /etc/keel/config/keel.hcl
listener "http" "0.0.0.0:80" {}

listener "https" "0.0.0.0:443" {}

pool "web" {
  backend "10.0.0.1:8080" {}
  backend "10.0.0.2:8080" {}
}

vhost "example.com" {
  pool = "web"

  tls {
    acme = true
  }
}
```

```bash
keel config validate          # exit 0: the files can go live
keel config validate --diff   # what changes, and whether it needs a restart
```

See [Configuration](docs/configuration.md).

---

## CLI

```bash
keel status
keel backend list --pool web
keel backend drain 10.0.0.1:8080 --wait
keel config reload
keel config push /etc/keel/config
keel cluster status
```

The same commands run remotely with `keelctl`. See [CLI](docs/cli.md) and [keelctl](docs/keelctl.md).

---

## Clustering

Every node is a cluster, of one or more. Nodes join through a shared secret and then talk mTLS. A config push, a drain or an ACME certificate is committed through Raft and applied on every node. Traffic does not depend on the cluster.

```hcl
cluster {
  advertise = "10.0.0.2:7654"
  secret    = "change-me"
  join      = ["10.0.0.1:7654"]
}
```

See [Cluster](docs/cluster.md).

---

## Documentation

- [Quickstart](docs/quickstart.md)
- [Architecture](docs/architecture.md)
- [Configuration](docs/configuration.md)
- [Virtual hosts](docs/virtual-hosts.md)
- [Load balancing](docs/load-balancing.md)
- [Health checks](docs/health-checks.md)
- [TCP proxying](docs/tcp-proxying.md)
- [UDP proxying](docs/udp-proxying.md)
- [ACME](docs/acme.md)
- [Caching](docs/caching.md)
- [Cluster](docs/cluster.md)
- [Access logging](docs/access-logging.md)
- [Metrics](docs/metrics.md)
- [CLI](docs/cli.md)
- [keelctl](docs/keelctl.md)
- [Security](docs/security.md)

---

## Contributing

Branch off `main`, use [Conventional Commits](https://www.conventionalcommits.org), keep `main` releasable.

```bash
cargo test --workspace                              # unit tests
cargo test -p keel-e2e --no-fail-fast -- --ignored  # end-to-end, needs Docker
```

The e2e suite builds an image of the working tree first. `KEEL_E2E_IMAGE=<tag>` uses an existing image; `KEEL_E2E_KEEP=1` keeps a failed test's containers.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
