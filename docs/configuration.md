# Configuration

Keel reads two things: the **node file**, `/etc/keel/node.yaml` (`--config` sets another path), and the **config directory** it names, `/etc/keel/config/` by default. The node file says how this node runs and is reached; the config directory is the load balancer. See [Files and layout](#files-and-layout).

## Top-level sections

| Section | File | Purpose | Reference |
|---|---|---|---|
| `keel` | both | Process settings; which keys go where is [below](#keel) | [below](#keel) |
| `cluster` | node file | Cluster mode: Raft, mTLS, peer address | [Cluster](cluster.md) |
| `control` | node file | Remote control listener (keelctl, mTLS) | [Remote control](keelctl.md) |
| `metrics` | node file | Prometheus metrics endpoint | [below](#metrics) |
| `listeners` | config directory | Network ports to bind | [below](#listeners) |
| `access_log` | config directory | NDJSON access log output | [Access logging](access-logging.md) |
| `cache` | config directory | Memory and disk HTTP cache | [Caching](caching.md) |
| `pools` | config directory | Backend pools with health checks and load balancing | [Load balancing](load-balancing.md), [Health checks](health-checks.md) |
| `vhosts` | config directory | Virtual host routing rules, TLS, cache rules | [Virtual hosts](virtual-hosts.md) |
| `acme` | config directory | ACME issuers: directory, contact | [ACME](acme.md) |
| `certificates` | config directory | Certificates obtained or loaded without a vhost, for TCP listeners and backends | [ACME](acme.md#certificates-for-tcp--tls-passthrough-backends) |

A section in the wrong file is a load error naming the section and where it belongs.

---

## keel

Process-level settings. The ones that describe the node go in the node file; the ones that shape traffic go in the config directory's `keel.yaml`.

```yaml
# /etc/keel/node.yaml
keel:
  workers: 4              # number of worker processes; default: CPU count (max 16)
  user: keel              # drop to this user after binding privileged ports
  group: keel             # drop to this group
  control_socket: /var/run/keel/keel.sock   # Unix socket for CLI commands
  state_dir: /var/lib/keel       # state kept across restarts: node ID, certificates, Raft store, control CA
  config_dir: /etc/keel/config   # the config directory
```

```yaml
# /etc/keel/config/keel.yaml
keel:
  grace_period_seconds: 10       # graceful shutdown: time for in-flight requests
  udp_flow_timeout_seconds: 30   # idle time before a UDP flow expires
```

| Field | File | Type | Default |
|---|---|---|---|
| `workers` | node file | integer | CPU count, max 16 |
| `user` | node file | string | `keel` |
| `group` | node file | string | `keel` |
| `control_socket` | node file | string | `/var/run/keel/keel.sock` |
| `state_dir` | node file | string | `/var/lib/keel` |
| `config_dir` | node file | string | `/etc/keel/config` |
| `grace_period_seconds` | config directory | integer | `10` |
| `udp_flow_timeout_seconds` | config directory | integer | `30` |
| `udp_max_flows` | config directory | integer | `8192` |

On `SIGTERM`, `SIGINT`, or `SIGQUIT`, Keel stops accepting new connections, lets in-flight requests finish for up to `grace_period_seconds`, then exits. Keep the value below the supervisor's kill timeout (`docker stop` defaults to 10s, K8s `terminationGracePeriodSeconds` to 30s). L4 TCP connections and UDP flows are closed at shutdown; use [backend drain](load-balancing.md#backend-drain) for zero-impact maintenance.

`udp_flow_timeout_seconds` is the idle time after which a UDP flow (one client `ip:port` on a `udp_pool` listener) expires and releases its backend; it must be at least 1. `udp_max_flows` caps how many flows each UDP listener holds at once — each costs an upstream socket and a task, and the worker's HTTP and TCP listeners share its descriptors, so a worker's ceiling is this value times the number of `udp_pool` listeners. Both must be at least 1. See [UDP proxying](udp-proxying.md).

Every setting in the `keel` section requires a process restart. See [Hot reload](#hot-reload) for what reloads live.

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
| `proxy_protocol` | bool | `false` | Expect a PROXY Protocol v1/v2 header from the upstream load balancer and take the client address from it. Works on plain HTTP, `tls: true`, `tcp_pool`, and `udp_pool` listeners. See [Virtual hosts](virtual-hosts.md#proxy-protocol) |
| `tcp_pool` | string | none | Makes the listener L4: raw TCP is spliced to this pool (passthrough — the stream is never inspected). Vhosts and routes do not apply, and `tls` is rejected on the same listener. See [TCP proxying](tcp-proxying.md) |
| `udp_pool` | string | none | Makes the listener UDP: datagrams are forwarded to this pool, one flow per client `ip:port` until idle for `keel.udp_flow_timeout_seconds`. Vhosts and routes do not apply; `tls` and `tcp_pool` are rejected on the same listener. See [UDP proxying](udp-proxying.md) |
| `tls_mode` | string | `passthrough` | `tcp_pool` only: `passthrough`, `terminate`, or `reencrypt`. See [TCP proxying](tcp-proxying.md#tls-handling--three-modes) |
| `tls_host` | string | none | `terminate`/`reencrypt`: host of a `certificates:` entry or a vhost with `tls`, served when the client's SNI has no certificate of its own |
| `tls_verify` | bool | `false` | `reencrypt`: verify the backend certificate (system roots plus `tls_ca`) against the backend's configured hostname |
| `tls_ca` | string | none | `reencrypt` with `tls_verify`: PEM bundle of extra trusted CAs |

A `tcp_pool` or `udp_pool` listener references an ordinary entry in `pools` — health checks, weights, algorithms, and drain behave the same as for HTTP. Validation fails at startup for an unknown pool name, a `tcp_pool` or `udp_pool` + `tls` combination, or `tcp_pool` and `udp_pool` on the same listener entry (use two entries with the same `address` to serve both protocols on one port).

For UDP, the master binds one socket per worker in an `SO_REUSEPORT` group and hands each worker its own; a replacement worker takes over the socket of the one it replaces.

The worker passes its inherited TCP sockets to Pingora over a private Unix socket, `upgrade-<index>.sock` in a `workers/` subdirectory of the `keel.control_socket` directory (the worker's own control socket, `worker-<index>.sock`, lives there too). The master creates that subdirectory and assigns it to `keel.user` before forking, keeping the parent directory — which holds its own control socket — root-owned. Pingora polls for the hand-off once a second and logs `No incoming socket transfer, sleep 1s and try again` at error level once per worker while it waits; the worker's next line, `handed inherited listeners to pingora`, confirms the transfer. Workers therefore start serving about one second after the master forks them.

Changing listener ports requires a process restart. Adding new listeners via hot reload is not supported.

---

## metrics

Prometheus metrics endpoint.

```yaml
metrics:
  address: 127.0.0.1:10790
```

| Field | Type | Default |
|---|---|---|
| `address` | string | `127.0.0.1:10790` |

Metrics are exposed at `GET /metrics` on this address (any other method or path returns `404`). Each node exposes its own metrics independently; federation is handled externally. With more than one worker, the endpoint is served by a single worker and shows that worker's numbers only — see [Metrics](metrics.md). Every exposed metric is listed in the [metrics reference](metrics.md).

> **Security:** metrics expose backend addresses, pool/vhost names, and traffic
> volumes. The default binds to `127.0.0.1` so they are not world-readable. To
> scrape from another host, set `address: 0.0.0.0:10790` and restrict the port
> with firewall rules, or keep the default and run a local scrape agent that
> reads `127.0.0.1:10790`.

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

---

## certificates

Standalone certificate requests: hostnames Keel obtains certificates for
without terminating TLS itself (TCP / TLS-passthrough backends). Keel answers
the HTTP-01 challenge and writes `{host}.crt` / `{host}.key` to
`acme.storage`. Allowed in any file of the config directory (appended like vhosts). See
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
  addr: 0.0.0.0:7654         # bind address for peer connections
  advertise: 10.0.0.1:7654   # address the other nodes connect to
  secret: change-me
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `addr` | string | `0.0.0.0:7654` | Peer listen address |
| `advertise` | string | `addr` | Address announced to the other nodes. Required when `addr` is unspecified (`0.0.0.0`, `::`) |
| `secret` | string | none | Shared secret for join authentication |

Unknown keys under `cluster:` are refused. The node ID is not a setting: it is generated on first start and kept in `keel.state_dir` — see [Node identity](cluster.md#node-identity). The Raft store and the node's certificates live there too; see [Restarts](cluster.md#restarts).

See [Cluster](cluster.md) for bootstrap, join, and restarts.

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

`remote.address` takes a literal address and port. A hostname fails config validation at startup.

The listener is served by the root master process. See [Security](security.md#the-remote-control-listener-runs-as-root).

| Field | Type | Default | Notes |
|---|---|---|---|
| `remote.address` | string | none | TCP listen address for keelctl. Must be a literal `ip:port` — a hostname is rejected at startup |
| `remote.allow` | list | none | Optional source-CIDR restriction; empty = any source. mTLS stays mandatory |

The control CA (`ca.crt` / `ca.key`) lives in `control/` under `keel.state_dir`. Unknown keys under `control.remote` are refused.

---

## Files and layout

```
/etc/keel/
├── node.yaml                 this node: keel.workers/user/group, cluster, control, metrics
└── config/                   the load balancer
    ├── keel.yaml             loaded first
    ├── pools/
    │   ├── api.yaml          then every other *.yaml below config/,
    │   └── web.yaml          recursively, in path order
    ├── vhosts/
    │   └── api.example.com.yaml
    └── certs/
        └── api.crt           not parsed; travels with the directory
```

The node file may be missing: at the default path that means every node setting at its default. The config directory must hold `keel.yaml`.

Merge rules for the files after `keel.yaml`:
- `pools`: merged as a map; a duplicate pool name is an error
- `vhosts`, `listeners`, `certificates`: appended in load order
- `keel`, `access_log`, `acme`, `cache`: `keel.yaml` only
- `cluster`, `control`, `metrics`: node file only
- `include:` is refused: every `*.yaml` in the directory is loaded

Names starting with `.` (editor and temporary files) are skipped. Every file must be UTF-8 text.

### In a cluster

The config directory is the replicated config: every node holds the same files. The same content exists in three forms — the committed entry in Raft, its copy in each node's Raft store on disk, and the files under `/etc/keel/config/`, which a node rewrites after it applied a version. The files are for editing and inspection; the node file is never replicated.

Each push is a **version**: the whole file set, keyed by relative path, committed through Raft (the version number is its log index). Every node builds the version against its own node file, applies it, and only then writes it into its config directory — so the directory always holds the last version that worked on that node. Files a version no longer has are deleted. A version that fails on a node leaves that node serving the previous one, logged as `config version cannot be applied`, files untouched.

A new version comes from:
- `keel config push <dir>` or `keelctl config push <dir>` — a directory as it is, or a single file as its `keel.yaml`; a follower forwards the push to the leader
- `SIGHUP` or `keel config reload` on a node — that node's config directory becomes the next version
- starting a node whose config files differ from the version it last applied — they were edited while it was stopped, and the later push wins
- the first start of a new cluster — the bootstrap node's files become the first version

A push is built against the node file before it is committed, so a version that does not load is refused. Without a reachable majority a push fails at once: `no quorum: 1 of 3 members reachable; config not pushed`. Traffic is unaffected either way.

---

## Hot reload

On a single node, send `SIGHUP` or run `keel config reload` to reload the node file and the config directory without dropping connections. The master forwards the signal to every worker and re-reads the config itself, so a worker it restarts later starts from the current config. If the new config fails to load, the master logs the error and retains the previous one for that purpose. In a cluster, the same commands push the node's config directory as the next version; see [In a cluster](#in-a-cluster).

What reloads without restart:
- Virtual host rules: hosts, routes and the pools they reference, `forwarded_headers`, cache rules, `redirect_http`, `default_action`
- TLS certificates: certificate files of vhosts and `certificates:` entries are re-read
- Backends removed from a pool — they are moved to `draining`

What requires a process restart:
- Backends added to a pool — logged as a warning (`hot reload: new backend requires restart to take effect`) and otherwise ignored
- Backend weights and pool algorithms — ignored without a log line
- Pools added to or removed from the config. A new pool has no backends until restart, so a route to it fails with `no_backend`; the backends of a removed pool are not drained
- `health_check` and `passive` settings — the running checks and rules keep their startup values
- ACME: hosts, issuers and `certificates:` entries added on reload are not issued until restart
- `cache` storage, `access_log`, `metrics` and `control`
- Every setting in the node file
- Listeners (`listeners[]`)
- Every setting in the `keel` section, including worker count, user/group and the UDP flow settings

Backends are matched on the address exactly as written in the config, not on the IP it resolved to, so a hostname whose resolution changed since startup is still recognised as the same backend. Reloading issues no DNS queries; a new IP for an existing hostname takes effect on restart, as with any other backend change.

In a cluster, a version applies the same way on every node: what reloads live above takes effect on push, the rest needs a restart of each node.
