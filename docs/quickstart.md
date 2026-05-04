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
- [Load balancing](load-balancing.md) — algorithms, health checks, backend drain
- [Caching](caching.md) — memory and disk cache
- [Cluster](cluster.md) — multi-node HA deployment
- [CLI reference](cli.md) — `keel` subcommands
- [Access logging](access-logging.md) — NDJSON request logs
