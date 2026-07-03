# Configuration

Keel is configured via a YAML file, defaulting to `keel.yaml` in the working directory. Pass a different path with `--config`.

## Top-level sections

| Section | Purpose | Reference |
|---|---|---|
| `keel` | Process settings: worker count, user/group, control socket | [below](#keel) |
| `listeners` | Network ports to bind | [below](#listeners) |
| `metrics` | Prometheus metrics endpoint | [below](#metrics) |
| `access_log` | NDJSON access log output | [Access logging](access-logging.md) |
| `cache` | Memory and disk HTTP cache | [Caching](caching.md) |
| `pools` | Backend pools with health checks and load balancing | [Load balancing](load-balancing.md) |
| `vhosts` | Virtual host routing rules | [Virtual hosts](virtual-hosts.md) |
| `include` | Glob patterns for conf.d-style config splitting | [below](#config-splitting) |
| `cluster` | Cluster mode: Raft, mTLS, peer address | [Cluster](cluster.md) |

---

## keel

Process-level settings.

```yaml
keel:
  workers: 4              # number of worker processes; default: CPU count (max 16)
  user: keel              # drop to this user after binding privileged ports
  group: keel             # drop to this group
  control_socket: /var/run/keel/keel.sock   # Unix socket for CLI commands
```

| Field | Type | Default |
|---|---|---|
| `workers` | integer | CPU count, max 16 |
| `user` | string | `keel` |
| `group` | string | `keel` |
| `control_socket` | string | `/var/run/keel/keel.sock` |

Changing `workers` requires a process restart. All other settings can be changed via hot reload.

---

## listeners

One entry per port. Keel binds all listeners before dropping privileges.

```yaml
listeners:
  - address: 0.0.0.0:80
  - address: 0.0.0.0:443
    tls: true
  - address: 0.0.0.0:8080
    proxy_protocol: true
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `address` | string | required | `host:port` |
| `tls` | bool | `false` | TLS termination; certs configured per-vhost |
| `proxy_protocol` | bool | `false` | Accept PROXY Protocol v1/v2 from upstream LBs |

Changing listener ports requires a process restart. Adding new listeners via hot reload is not supported.

---

## metrics

Prometheus metrics endpoint.

```yaml
metrics:
  address: 127.0.0.1:9090
```

| Field | Type | Default |
|---|---|---|
| `address` | string | `127.0.0.1:9090` |

Metrics are exposed at `GET /metrics` on this address (any other method or path returns `404`). Each node exposes its own metrics independently; federation is handled externally.

> **Security:** metrics reveal backend addresses, pool/vhost names, and traffic
> volumes. The default binds to `127.0.0.1` so they are not world-readable. To
> scrape from another host, set `address: 0.0.0.0:9090` **and** restrict the port
> with firewall rules — or keep the default and run a local scrape agent that
> reads `127.0.0.1:9090`.

---

## access_log

```yaml
access_log:
  enabled: true
  dir: /var/log/keel
```

| Field | Type | Default |
|---|---|---|
| `enabled` | bool | `true` |
| `dir` | string | `/var/log/keel` |

Set `dir: "-"` to write to stdout. See [Access logging](access-logging.md) for the full log format.

---

## cache

```yaml
cache:
  memory: 256M
  disk:
    path: /var/cache/keel
    size: 10G
```

Size values use binary prefixes: `K`, `M`, `G` (case-insensitive). A bare number is bytes. Examples: `256M`, `1G`, `512K`.

| Field | Type | Default | Notes |
|---|---|---|---|
| `memory` | string | none | Memory budget; omit to disable memory cache |
| `disk.path` | string | none | Directory for disk cache files |
| `disk.size` | string | none | Disk budget |

Omit `cache` entirely to disable caching globally. See [Caching](caching.md) for tier behavior and per-vhost configuration.

---

## pools

Named backend pools. Each pool has a load balancing algorithm, optional health checks, and a list of backends.

```yaml
pools:
  web:
    algorithm: round_robin
    health_check:
      type: http
      path: /health
      interval: 10s
      timeout: 2s
      healthy_threshold: 2
      unhealthy_threshold: 3
    backends:
      - address: 10.0.0.1:8080
        weight: 1
      - address: 10.0.0.2:8080
        weight: 2
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `algorithm` | string | `round_robin` | `round_robin`, `random`, `least_connections`, `consistent_hash` |
| `health_check` | object | none | Omit to disable health checks |
| `health_check.type` | string | required | `tcp` or `http` |
| `health_check.path` | string | `/health` | HTTP path (http type only) |
| `health_check.interval` | string | `10s` | Check frequency |
| `health_check.timeout` | string | `2s` | Per-check timeout |
| `health_check.healthy_threshold` | integer | `2` | Consecutive successes to mark healthy |
| `health_check.unhealthy_threshold` | integer | `3` | Consecutive failures to mark unhealthy |
| `backends[].address` | string | required | `host:port` |
| `backends[].weight` | integer | `1` | Relative weight for weighted algorithms |

See [Load balancing](load-balancing.md) for details on algorithms and drain behavior.

---

## vhosts

Virtual host routing. Evaluated in order; first match wins.

```yaml
vhosts:
  - host: api.example.com
    pool: api
    tls:
      cert: /etc/keel/certs/api.crt
      key: /etc/keel/certs/api.key
    forwarded_headers:
      mode: replace
    cache:
      enabled: true
      ttl: 60

  - host: "*"
    pool: default
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `host` | string | required | Exact hostname or `*` wildcard |
| `pool` | string | none | Default pool; required if no `routes` |
| `routes` | list | none | Path-prefix routing; see [Virtual hosts](virtual-hosts.md) |
| `tls.cert` | string | none | Path to PEM certificate (BYO cert) |
| `tls.key` | string | none | Path to PEM private key (BYO cert) |
| `tls.acme` | bool | `false` | Obtain/renew the certificate automatically via ACME — see [ACME](acme.md) |
| `redirect_http` | bool | `false` (`true` when `tls.acme`) | 301 plain HTTP to HTTPS |
| `forwarded_headers.mode` | string | `replace` | `replace`, `append`, or `off` |
| `forwarded_headers.trusted_proxies` | list | none | CIDRs trusted in `append` mode |
| `cache.enabled` | bool | `false` | Enable caching for this vhost |
| `cache.ttl` | integer | none | Seconds; fallback TTL when origin omits `Cache-Control` |

See [Virtual hosts](virtual-hosts.md) for host matching rules, path routing, and TLS hot-swap.

---

## acme

Automatic TLS via Let's Encrypt or any ACME v2 CA. See [ACME](acme.md).

```yaml
acme:
  email: ops@example.com
  directory: https://acme-v02.api.letsencrypt.org/directory
  storage: /var/lib/keel/acme
  domains: []               # standalone certs for TCP/passthrough backends
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `email` | string | none | ACME account contact |
| `directory` | string | Let's Encrypt production | ACME v2 directory URL |
| `storage` | string | `/var/lib/keel/acme` | Certs, keys, account, challenge tokens |
| `domains` | list | `[]` | Hostnames without a TLS vhost to issue cert files for |
| `root_ca` | string | none | Extra trust root for the ACME API (testing only) |

---

## cluster

Required in cluster mode. Omit for standalone.

```yaml
cluster:
  addr: 0.0.0.0:7654
  node_id: 1            # optional; derived from addr hash if absent
  secret: change-me
  # ca_cert: /etc/keel/cluster-ca.crt   # BYO CA
  # ca_key:  /etc/keel/cluster-ca.key
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `addr` | string | `0.0.0.0:7654` | RPC listen address for peer connections |
| `node_id` | integer | derived | Raft node ID; must be unique per cluster |
| `secret` | string | none | Shared secret for join authentication |
| `ca_cert` | string | none | BYO CA certificate path |
| `ca_key` | string | none | BYO CA key path |

See [Cluster](cluster.md) for bootstrap, join, and CA options.

---

## Config splitting

Large deployments can split configuration across multiple files using glob includes. This is useful when different teams manage their own vhosts or pools independently.

```yaml
# keel.yaml
include:
  - conf.d/**/*.yaml
```

Or via CLI:

```bash
keel --config keel.yaml --conf-dir conf.d/
```

Files are loaded in alphabetical order and merged into the root config.

Merge rules:
- `pools`: merged as a map; duplicate pool name is an error
- `vhosts`: appended in load order
- `listeners`: appended in load order
- `keel`, `metrics`, `access_log`, `include`, `cluster`: root file only; error if present in included files

Example layout:

```
/etc/keel/
├── keel.yaml
└── conf.d/
    ├── pools/
    │   ├── api.yaml
    │   └── web.yaml
    └── vhosts/
        ├── api.example.com.yaml
        └── app.example.com.yaml
```

On `SIGHUP` or `keel config reload`, all files including conf.d fragments are re-read and re-merged.

---

## Hot reload

Send `SIGHUP` or run `keel config reload` to reload configuration without dropping connections.

What reloads without restart:
- Backend pool membership and weights
- Health check parameters
- Virtual host routing rules
- TLS certificates

What requires a process restart:
- Listener ports (`listeners[].address`)
- Worker count (`keel.workers`)
- Process user/group (`keel.user`, `keel.group`)

In cluster mode, use `keel config push <file>` to distribute a new config to all nodes via Raft. See [Cluster](cluster.md).
