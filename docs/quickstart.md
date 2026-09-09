# Quickstart

## Build from source

```bash
cargo build --release
```

The binary is at `target/release/keel`.

## Minimal configuration

Create `keel.yaml`:

```yaml
listeners:
  - address: 0.0.0.0:8080

pools:
  web:
    backends:
      - address: 127.0.0.1:3000

vhosts:
  - host: "*"
    pool: web
```

This listens on port 8080 and proxies all traffic to a single backend at `127.0.0.1:3000`.

## Run

```bash
./keel --config keel.yaml
```

Keel forks a root master process that binds the listening ports and spawns worker processes running as the `keel` user. For local testing as the current user, set `keel.user` and `keel.group` to your username, or run as root.

## Verify

```bash
curl http://localhost:8080/
```

Check that the request reaches your backend and returns a response.

## Next steps

- [Configuration reference](configuration.md) — full schema for all sections
- [Virtual hosts](virtual-hosts.md) — host-based routing, TLS, path routing
- [Automatic TLS (ACME)](acme.md) — Let's Encrypt / ACME v2 certificates and renewal
- [Load balancing](load-balancing.md) — algorithms, weights, backend drain
- [Health checks](health-checks.md) — tcp, udp, and http probes, thresholds, status output
- [TCP proxying](tcp-proxying.md) — L4 passthrough for databases and TLS-on-connect services
- [UDP proxying](udp-proxying.md) — per-client flows for DNS, syslog, and other datagram services
- [Caching](caching.md) — memory and disk cache
- [Cluster](cluster.md) — multi-node HA deployment
- [CLI reference](cli.md) — `keel` subcommands
- [Remote control](keelctl.md) — keelctl over mTLS from a workstation or CI
- [Access logging](access-logging.md) — NDJSON request logs
- [Metrics](metrics.md) — Prometheus metrics reference
- [Security](security.md) — security properties and operator checklist
