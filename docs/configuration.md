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
| `pools` | Backend pools with health checks and load balancing | [Load balancing](load-balancing.md), [Health checks](health-checks.md) |
| `vhosts` | Virtual host routing rules, TLS, cache rules, gateway rules | [Virtual hosts](virtual-hosts.md), [API gateway](gateway.md) |
| `acme` | ACME issuers: directory, contact, challenge type, DNS provider | [ACME](acme.md) |
| `certificates` | Certificates obtained or loaded without a vhost, for TCP listeners and backends | [ACME](acme.md#certificates-for-tcp--tls-passthrough-backends) |
| `include` | Glob patterns for conf.d-style config splitting | [below](#config-splitting) |
| `cluster` | Cluster mode: Raft, mTLS, peer address | [Cluster](cluster.md) |
| `control` | Remote control listener (keelctl, mTLS) | [Remote control](keelctl.md) |

---

## keel

Process-level settings.

```yaml
keel:
  workers: 4              # number of worker processes; default: CPU count (max 16)
  user: keel              # drop to this user after binding privileged ports
  group: keel             # drop to this group
  control_socket: /var/run/keel/keel.sock   # Unix socket for CLI commands
  grace_period_seconds: 10   # graceful shutdown: time for in-flight requests
  udp_flow_timeout_seconds: 30   # idle time before a UDP flow expires
```

| Field | Type | Default |
|---|---|---|
| `workers` | integer | CPU count, max 16 |
| `user` | string | `keel` |
| `group` | string | `keel` |
| `control_socket` | string | `/var/run/keel/keel.sock` |
| `grace_period_seconds` | integer | `10` |
| `udp_flow_timeout_seconds` | integer | `30` |

On `SIGTERM`, `SIGINT`, or `SIGQUIT`, Keel stops accepting new connections, lets in-flight requests finish for up to `grace_period_seconds`, then exits. Keep the value below the supervisor's kill timeout (`docker stop` defaults to 10s, K8s `terminationGracePeriodSeconds` to 30s). L4 TCP connections and UDP flows are closed at shutdown; use [backend drain](load-balancing.md#backend-drain) for zero-impact maintenance.

`udp_flow_timeout_seconds` is the idle time after which a UDP flow (one client `ip:port` on a `udp_pool` listener) expires and releases its backend; it must be at least 1. See [UDP proxying](udp-proxying.md).

Changing `workers` or `udp_flow_timeout_seconds` requires a process restart. All other settings can be changed via hot reload.

---

## listeners

One entry per port. When Keel starts as root on Linux, the master binds every listener — TCP and UDP — before forking the workers, and the workers inherit the sockets after dropping to `keel.user`. Workers never need the capability to bind ports below 1024. When started unprivileged (dev), each worker binds its own listeners.

```yaml
listeners:
  - address: 0.0.0.0:80              # HTTP
  - address: 0.0.0.0:443             # HTTPS — TLS terminated, certs per-vhost
    tls: true
  - address: 0.0.0.0:5432            # L4 — raw TCP spliced to a pool
    tcp_pool: postgres
  - address: 0.0.0.0:6379            # L4 — TLS terminated by Keel, plaintext to the pool
    tcp_pool: redis
    tls_mode: terminate
    tls_host: cache.example.com
  - address: 0.0.0.0:53              # L4 — UDP datagrams forwarded to the named pool
    udp_pool: resolvers
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `address` | string | required | `host:port` |
| `tls` | bool | `false` | TLS termination; certs configured per-vhost |
| `proxy_protocol` | bool | `false` | Expect a PROXY Protocol v1/v2 header from the upstream load balancer and take the client address from it. Plain HTTP, `tcp_pool`, and `udp_pool` listeners; rejected with `tls: true`. See [Virtual hosts](virtual-hosts.md#proxy-protocol) |
| `tcp_pool` | string | none | Makes the listener L4: raw TCP is spliced to this pool (passthrough — the stream is never inspected). Vhosts and routes do not apply, and `tls` is rejected on the same listener. See [TCP proxying](tcp-proxying.md) |
| `udp_pool` | string | none | Makes the listener UDP: datagrams are forwarded to this pool, one flow per client `ip:port` until idle for `keel.udp_flow_timeout_seconds`. Vhosts and routes do not apply; `tls` and `tcp_pool` are rejected on the same listener. See [UDP proxying](udp-proxying.md) |
| `tls_mode` | string | `passthrough` | `tcp_pool` only: `passthrough`, `terminate`, or `reencrypt`. See [TCP proxying](tcp-proxying.md#tls-handling--three-modes) |
| `tls_host` | string | none | `terminate`/`reencrypt`: host of a `certificates:` entry or a vhost with `tls`, served when the client's SNI has no certificate of its own |
| `tls_verify` | bool | `false` | `reencrypt`: verify the backend certificate (system roots plus `tls_ca`) against the backend's configured hostname |
| `tls_ca` | string | none | `reencrypt` with `tls_verify`: PEM bundle of extra trusted CAs |

A `tcp_pool` or `udp_pool` listener references an ordinary entry in `pools` — health checks, weights, algorithms, and drain behave the same as for HTTP. Validation fails at startup for an unknown pool name, a `tcp_pool` or `udp_pool` + `tls` combination, or `tcp_pool` and `udp_pool` on the same listener entry (use two entries with the same `address` to serve both protocols on one port).

For UDP, the master binds one socket per worker in an `SO_REUSEPORT` group and hands each worker its own; a replacement worker takes over the socket of the one it replaces.

The worker passes its inherited TCP sockets to Pingora over a private Unix socket, `upgrade-<index>.sock` in the directory of `keel.control_socket`. The master creates that directory and assigns it to `keel.user` before forking. Pingora polls for the hand-off once a second and logs `No incoming socket transfer, sleep 1s and try again` at error level once per worker while it waits; the worker's next line, `handed inherited listeners to pingora`, confirms the transfer. Workers therefore start serving about one second after the master forks them.

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

Metrics are exposed at `GET /metrics` on this address (any other method or path returns `404`). Each node exposes its own metrics independently; federation is handled externally. Every exposed metric is listed in the [metrics reference](metrics.md).

> **Security:** metrics expose backend addresses, pool/vhost names, and traffic
> volumes. The default binds to `127.0.0.1` so they are not world-readable. To
> scrape from another host, set `address: 0.0.0.0:9090` and restrict the port
> with firewall rules, or keep the default and run a local scrape agent that
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
| `health_check` | object | none | Omit to disable health checks. Fields per probe type in [Health checks](health-checks.md) |
| `health_check.type` | string | required | `tcp`, `udp`, `http`, `dns`, `ntp`, `icmp`, `tls` |
| `health_check.interval` | string | `10s` | Time between rounds, ±10% jitter |
| `health_check.timeout` | string | `2s` | Per-probe timeout |
| `health_check.healthy_threshold` | integer | `2` | Consecutive successes to mark healthy |
| `health_check.unhealthy_threshold` | integer | `3` | Consecutive failures to mark unhealthy |
| `health_check.port` | integer | traffic port | Probe a different port on the backend |
| `health_check.path`, `.host`, `.tls`, `.expect_status`, `.expect_body` | | | `http` only |
| `health_check.query`, `.record`, `.transport`, `.expect` | | | `dns` only |
| `health_check.sni`, `.min_days_valid` | | | `tls` only |
| `passive.enabled` | bool | `true` | Eject a backend after consecutive traffic failures. See [Health checks](health-checks.md#passive-detection) |
| `passive.failures` | integer | `5` | Consecutive upstream connect (or UDP reply) failures that eject |
| `passive.eject_for` | string | `30s` | Time out of rotation before re-admission |
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
| `tls.acme` | bool or string | `false` | `true` = ACME via issuer `default`; a string names an issuer from `acme.issuers` — see [ACME](acme.md) |
| `redirect_http` | bool | `false` (`true` when `tls.acme`) | 301 plain HTTP to HTTPS |
| `forwarded_headers.mode` | string | `replace` | `replace`, `append`, or `off` |
| `forwarded_headers.trusted_proxies` | list | none | CIDRs trusted in `append` mode |
| `cache.enabled` | bool | `false` | Enable caching for this vhost |
| `cache.ttl` | integer | none | Seconds; fallback TTL when origin omits `Cache-Control` |
| `default_action` | object | none | Answer directly without a pool: `redirect:` or `status:`/`body:` — see [Virtual hosts](virtual-hosts.md#default-action). Excludes `pool`/`routes` |
| `rate_limit` | object | none | Per-client-IP token bucket: `requests`, `per` (default `1s`), `burst` (default `requests`). Also per route. See [API gateway](gateway.md) |
| `headers` | object | none | `request` and `response` blocks, each with `set` (map) and `remove` (list). Also per route |
| `rewrite` | object | none | `strip_prefix`, `add_prefix` applied to the backend path. Also per route |
| `auth.jwt` | object | none | JWT validation: one of `secret`, `secret_file`, `public_key`; optional `issuer`, `audience`, `header`, `leeway`, `claim_headers`. Also per route. See [API gateway](gateway.md#authentication-jwt) |

See [Virtual hosts](virtual-hosts.md) for host matching rules, path routing, and TLS hot-swap.

---

## acme

Automatic TLS via any ACME v2 CA, organized as named
issuers. Vhosts reference an issuer with `tls: { acme: true }` (the issuer
named `default`) or `tls: { acme: <name> }`. See [ACME](acme.md).

```yaml
acme:
  storage: /var/lib/keel/acme
  renew_before: 30%
  issuers:
    default:
      email: ops@example.com
    internal:
      directory: https://ca.corp.internal/acme/directory
      root_ca: /etc/keel/corp-root.pem
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `storage` | string | `/var/lib/keel/acme` | Certs, keys, accounts, challenge tokens |
| `renew_before` | string | `30%` | Renew when less than this remains: `%` of lifetime or absolute `Nd` |
| `issuers.<name>.email` | string | none | ACME account contact for this issuer |
| `issuers.<name>.directory` | string | `https://acme-v02.api.letsencrypt.org/directory` | ACME v2 directory URL |
| `issuers.<name>.root_ca` | string | none | Extra trust root for the ACME API (internal or self-signed CAs) |
| `issuers.<name>.renew_before` | string | global value | Per-issuer renewal override |
| `issuers.<name>.challenge` | string | `http-01` | `http-01` or `dns-01`. Wildcard hosts need `dns-01` |
| `issuers.<name>.dns` | object | none | `dns-01` provider: `type: rfc2136` with `server`, `zone`, `tsig_name`, `tsig_key` or `tsig_key_file`, `tsig_algorithm`, `propagation_wait`, `ttl`. See [ACME](acme.md#dns-01-challenge) |

---

## certificates

Standalone certificate requests: hostnames Keel obtains certificates for
without terminating TLS itself (TCP / TLS-passthrough backends). Keel answers
the HTTP-01 challenge and writes `{host}.crt` / `{host}.key` to
`acme.storage`. Allowed in conf.d files (appended like vhosts). See
[ACME](acme.md).

```yaml
certificates:
  - host: db.example.com
    issuer: default        # optional; "default" when omitted
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `host` | string | required | Hostname to issue for (no wildcards) |
| `issuer` | string | `default` | Issuer name from `acme.issuers`. Ignored when `cert`/`key` are set |
| `cert`, `key` | string | none | Bring-your-own PEM files instead of ACME issuance; see [TCP proxying](tcp-proxying.md) |

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

## control

Remote control for `keelctl`. Off unless `remote` is set. mTLS is mandatory;
see [Remote control](keelctl.md).

```yaml
control:
  remote:
    address: 0.0.0.0:10789
    allow:
      - 10.1.2.0/24
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `remote.address` | string | none | TCP listen address for keelctl |
| `remote.allow` | list | none | Optional source-CIDR restriction; empty = any source. mTLS stays mandatory |
| `remote.ca_dir` | string | `/var/lib/keel/control` | Control CA storage (`ca.crt` / `ca.key`) |

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
- `certificates`: appended in load order
- `keel`, `metrics`, `access_log`, `include`, `cluster`, `acme`: root file only; error if present in included files

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
- Backends removed from a pool — they are moved to `draining`
- Health check parameters
- Virtual host routing rules
- TLS certificates

What requires a process restart:
- Backends added to a pool, and backend weight changes — both are logged as a warning and otherwise ignored
- Listener ports (`listeners[].address`)
- Worker count (`keel.workers`)
- Process user/group (`keel.user`, `keel.group`)

A backend is matched by its resolved address, so a hostname that resolves to a different IP than it did at startup reads as one backend removed (drained) and another added (needs a restart). If any backend address in a pool cannot be resolved at all during a reload, that pool is left untouched — a DNS failure never drains a serving pool.

In cluster mode, use `keel config push <file>` to distribute a new config to all nodes via Raft. See [Cluster](cluster.md).
