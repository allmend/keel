# Configuration

Keel reads two things: the **node file**, `/etc/keel/node.hcl` (`--config` sets another path), and the **config directory** it names, `/etc/keel/config/` by default. The node file says how this node runs and is reached; the config directory is the load balancer. See [Files and layout](#files-and-layout).

Both are [HCL](https://github.com/hashicorp/hcl/blob/main/hclsyntax/spec.md), native syntax, in files ending in `.hcl`. See [The language](#the-language).

## Blocks

| Block | File | Purpose | Reference |
|---|---|---|---|
| `keel` | both | Process settings; which go where is [below](#keel) | [below](#keel) |
| `cluster` | node file | Peers: address, secret, members to join | [Cluster](cluster.md) |
| `control` | node file | Remote control listener (keelctl, mTLS) | [Remote control](keelctl.md) |
| `metrics` | node file | Prometheus metrics endpoint | [below](#metrics) |
| `listener "<protocol>" "<address>"` | config directory | Network ports to bind | [below](#listener) |
| `access_log` | config directory | NDJSON access log output | [Access logging](access-logging.md) |
| `cache` | config directory | Memory and disk HTTP cache | [Caching](caching.md) |
| `pool "<name>"` | config directory | Backend pools with health checks and load balancing | [Load balancing](load-balancing.md), [Health checks](health-checks.md) |
| `vhost "<host>"` | config directory | Virtual host routing rules, TLS, cache rules | [Virtual hosts](virtual-hosts.md) |
| `acme` | config directory | ACME issuers: directory, contact | [ACME](acme.md) |
| `certificate "<host>"` | config directory | Certificates obtained or loaded without a vhost, for TCP listeners and backends | [ACME](acme.md#certificates-without-a-vhost) |
| `locals` | both | Named values to refer to as `local.<name>` | [below](#locals) |

A block in the wrong file is a load error naming the block and where it belongs.

---

## The language

```hcl
# A comment. // and /* */ work too.
pool "web" {                      # block type, then its labels
  algorithm = "least_connections" # attribute = value

  health_check "http" {
    path     = "/health"
    interval = "10s"
  }

  backend "10.0.0.1:8080" { weight = 2 }
  backend "10.0.0.2:8080" {} # a block without settings
}
```

- Strings are quoted. Numbers and `true`/`false` are not.
- Lists are `[1, 2]`, objects `{ a = 1 }`. A multi-line string is a heredoc (`<<EOT` ... `EOT`; `<<-EOT` strips the common indentation).
- Labels are literal strings: no references in a label.
- Named things are blocks with labels, never lists of objects. An attribute where a block belongs (`backends = [...]`, `tls = {...}`) is refused.
- An unknown attribute or block, a value of the wrong type, or an attribute set twice is a load error that names the file, line and column:

```
/etc/keel/config/pools/web.hcl:4:3: unknown attribute `algorithmn`
```

Values are literals, references to [locals](#locals) and string templates (`"${local.host}:8080"`). Functions, operators, conditionals, `for` expressions and template directives (`%{if}`, `%{for}`) are refused.

### locals

```hcl
locals {
  backend_port = 8080
  check_every  = "5s"
  api_host     = "api.internal"
}

pool "api" {
  health_check "http" {
    interval = local.check_every
  }

  backend "10.0.0.1:8080" {}
}

vhost "api.example.com" {
  pool = "api"

  tls {
    cert = "certs/${local.api_host}.crt"
    key  = "certs/${local.api_host}.key"
  }
}
```

- The config directory is one namespace: a local defined in any of its files can be used in all of them. The node file has its own.
- A local can use literals and other locals, in any order. A name defined twice is a load error naming both places; locals that refer to each other in a cycle are a load error naming the chain.
- A template that is only one reference (`"${local.port}"`) is that value, not a string.

### Durations

Every time setting is a string: a whole number and a unit, `s`, `m`, `h`, `d`, `w` or `y`. `30s`, `5m`, `12h`, `30d`. The smallest is `1s`; `0s` and `ms` are refused. A bare number is refused; `keel config validate --write` adds the unit.

Timers start over when Keel restarts: a backend ejected for `30s` is back after a restart, a health check runs its first round within a second.

### Addresses

Every address Keel binds or connects to is an IP and a port: `10.0.0.1:8080`, `[2001:db8::1]:8080`. That covers listeners, backends, `metrics`, `control.remote` and `cluster`. Keel resolves no host names; a name is a load error.

---

## keel

Process-level settings. The ones that describe the node go in the node file; the ones that shape traffic go in the config directory's `keel.hcl`.

```hcl
# /etc/keel/node.hcl
keel {
  workers        = 4                         # worker processes; default: CPU count (max 16)
  user           = "keel"                    # workers drop to this user
  group          = "keel"                    # and this group
  control_user   = "keel-control"            # the control worker: Raft, control listeners, ACME
  control_socket = "/var/run/keel/keel.sock" # Unix socket for CLI commands
  state_dir      = "/var/lib/keel"           # node ID, certificates, Raft store, control CA
  config_dir     = "/etc/keel/config"        # the config directory
}
```

```hcl
# /etc/keel/config/keel.hcl
keel {
  grace_period     = "10s" # graceful shutdown: time for in-flight requests
  udp_flow_timeout = "30s" # idle time before a UDP flow expires
}
```

| Attribute | File | Type | Default |
|---|---|---|---|
| `workers` | node file | number | CPU count, max 16 |
| `user` | node file | string | `keel` |
| `group` | node file | string | `keel` |
| `control_user` | node file | string | `keel-control`; must differ from `user` |
| `control_socket` | node file | string | `/var/run/keel/keel.sock` |
| `state_dir` | node file | string | `/var/lib/keel` |
| `config_dir` | node file | string | `/etc/keel/config` |
| `grace_period` | config directory | duration | `10s` |
| `udp_flow_timeout` | config directory | duration | `30s` |
| `udp_max_flows` | config directory | number | `8192` |
| `keepalive_timeout` | config directory | duration | `75s` |
| `body_timeout` | config directory | duration | `60s` |
| `max_header_size` | config directory | size | `64K` |

`keepalive_timeout` closes an HTTP/1 client connection idle between requests. `body_timeout` is the longest wait between two reads of a request body. A request line and headers over `max_header_size` get `431`. Pingora reads the first request's headers with a fixed 60s timeout.

On `SIGTERM`, `SIGINT`, or `SIGQUIT`, Keel stops accepting new connections, lets in-flight requests finish for up to `grace_period`, then exits. Keep the value below the supervisor's kill timeout (`docker stop` defaults to 10s, K8s `terminationGracePeriodSeconds` to 30s). L4 TCP connections and UDP flows are closed at shutdown; use [backend drain](load-balancing.md#backend-drain) for zero-impact maintenance.

`udp_flow_timeout` is the idle time after which a UDP flow (one client `ip:port` on a `udp` listener) expires and releases its backend. `udp_max_flows` caps how many flows each UDP listener holds at once — each costs an upstream socket and a task, and the worker's HTTP and TCP listeners share its descriptors, so a worker's ceiling is this value times the number of `udp` listeners; it must be at least 1. See [UDP proxying](udp-proxying.md).

Every setting in the `keel` block requires a process restart. See [Hot reload](#hot-reload) for what reloads live.

---

## listener

One block per listener, labelled by its name. Names appear in logs, metrics and `keel status`, the same on every node.

| Protocol | Serves |
|---|---|
| `http` | HTTP, routed by [vhost](virtual-hosts.md) |
| `https` | HTTP over TLS, certificates per vhost |
| `tcp` | Raw TCP to a pool. See [TCP proxying](tcp-proxying.md) |
| `udp` | Datagrams to a pool. See [UDP proxying](udp-proxying.md) |

```hcl
listener "web" {
  protocol  = "https"
  addresses = ["194.56.78.90", "194.56.78.91", "194.56.78.92"]
  port      = 443
}

# Raw TCP spliced to a pool.
listener "postgres" {
  protocol  = "tcp"
  addresses = ["0.0.0.0", "::"]
  port      = 5432
  pool      = "postgres"
}

# TLS terminated by Keel, plaintext to the pool.
listener "redis" {
  protocol  = "tcp"
  addresses = ["0.0.0.0", "::"]
  port      = 6379
  pool      = "redis"
  tls_mode  = "terminate"
  tls_host  = "cache.example.com"
}

listener "dns" {
  protocol  = "udp"
  addresses = ["0.0.0.0", "::"]
  port      = 53
  pool      = "resolvers"
}
```

`addresses` is the same on every node; each node binds the ones it has and skips the rest, logged once at start (`master: addresses not on this node`). Per-node IPs are expressed by listing all of them. A listener none of whose addresses a node has is not served there, which is how a listener runs on some nodes only. Wildcards are written out, both families: `["0.0.0.0", "::"]`; IPv6 sockets are IPv6 only. `keel status` lists what each listener bound and skipped, and `keel_listener_bound{listener,address}` is `1` or `0`.

On Linux the master binds every listener before forking, and the workers inherit the sockets: ports below 1024 need root in the master only, and every worker accepts on the same socket. Elsewhere each worker binds its own with `SO_REUSEPORT`.

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `protocol` | string | required | `http`, `https`, `tcp` or `udp` |
| `addresses` | list | required | IPs, at least one |
| `port` | number | required | 1–65535 |
| `pool` | string | none | `tcp` and `udp`: the pool to forward to; required. Refused on `http` and `https`, which route by vhost |
| `proxy_protocol` | bool | `false` | Expect a PROXY protocol v1/v2 header from the load balancer in front and take the client address from it. Every protocol. See [Virtual hosts](virtual-hosts.md#proxy-protocol) |
| `allow`, `deny` | list | none | `tcp` and `udp`: client networks or addresses. See [Access lists](virtual-hosts.md#access-lists) |
| `tls_mode` | string | `passthrough` | `tcp` only: `passthrough`, `terminate`, or `reencrypt`. See [TCP proxying](tcp-proxying.md#tls-modes) |
| `tls_host` | string | none | `terminate`/`reencrypt`: host of a `certificate` block or a vhost with `tls`, served when the client's SNI has no certificate of its own |
| `tls_verify` | bool | `false` | `reencrypt`: verify the backend certificate (system roots plus `tls_ca`) against the backend's IP |
| `tls_ca` | string | none | `reencrypt` with `tls_verify`: PEM bundle of extra trusted CAs. A relative path is a file of the config directory; see [Files the config names](#files-the-config-names) |
| `idle_timeout` | duration | `1h` | `tcp`: close a connection with no bytes either way for this long; error `idle_timeout` |

`http`, `https` and `tcp` share TCP ports, so two of them on one address and port, or on a wildcard and an address of its family, is a load error; `udp` and `tcp` can share them. A `tcp` or `udp` listener's pool is an ordinary `pool` block — health checks, weights, algorithms, and drain behave the same as for HTTP.

For UDP, the master binds one socket per worker in an `SO_REUSEPORT` group and hands each worker its own; a replacement worker takes over the socket of the one it replaces.

Each worker hands its inherited TCP sockets to Pingora over `workers/upgrade-<index>.sock` beside the control socket. Pingora checks for them once a second and logs `No incoming socket transfer, sleep 1s and try again` once per worker meanwhile; `handed inherited listeners to pingora` follows. Workers serve about one second after the fork.

Changing listeners, their addresses or ports included, requires a restart.

---

## metrics

Prometheus metrics endpoint.

```hcl
metrics {
  address = "127.0.0.1:10790"
}
```

| Attribute | Type | Default |
|---|---|---|
| `address` | string | `127.0.0.1:10790` |

Metrics are served at `GET /metrics` on this address; any other method or path gets `404`. The master binds the address, the control worker serves it, and the values are totals over all workers — see [Metrics](metrics.md#one-endpoint-per-node).

Metrics show backend addresses, pool and vhost names and traffic volumes, which is why the default is loopback. Scraping from another host takes `address = "0.0.0.0:10790"`.

---

## access_log

```hcl
access_log {
  enabled = true
  dir     = "/var/log/keel"
}
```

| Attribute | Type | Default |
|---|---|---|
| `enabled` | bool | `true` |
| `dir` | string | `/var/log/keel` (absolute, or `-` for stdout) |

Set `dir = "-"` to write to stdout. See [Access logging](access-logging.md) for the full log format.

---

## cache

```hcl
cache {
  memory = "256M"

  disk {
    path = "/var/cache/keel"
    size = "10G"
  }
}
```

Size values use binary prefixes: `K`, `M`, `G` (case-insensitive). A bare number is bytes. Examples: `256M`, `1G`, `512K`.

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `memory` | string | none | Memory budget; omit to disable memory cache |
| `disk.path` | string | none | Directory for disk cache files; absolute |
| `disk.size` | string | none | Disk budget |

Omit `cache` entirely to disable caching globally. See [Caching](caching.md) for tier behavior and per-vhost configuration.

---

## pool

A named backend pool: a load balancing algorithm, an optional health check, and its backends, one `backend "<address>"` block each.

```hcl
pool "web" {
  algorithm = "round_robin"

  health_check "http" {
    path                = "/health"
    interval            = "10s"
    timeout             = "2s"
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }

  backend "10.0.0.1:8080" {}
  backend "10.0.0.2:8080" { weight = 2 }
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `algorithm` | string | `round_robin` | `round_robin`, `random`, `least_connections`, `consistent_hash` |
| `health_check "<type>"` | block | none | `tcp`, `udp`, `http`, `dns`, `ntp`, `icmp`, `tls`. Omit to disable health checks. Attributes per type in [Health checks](health-checks.md) |
| `health_check.interval` | duration | `10s` | Time between rounds, ±10% jitter |
| `health_check.timeout` | duration | `2s` | Per-probe timeout |
| `health_check.healthy_threshold` | number | `2` | Consecutive successes to mark healthy |
| `health_check.unhealthy_threshold` | number | `3` | Consecutive failures to mark unhealthy |
| `health_check.port` | number | traffic port | Probe a different port on the backend |
| `health_check.path`, `.host`, `.tls`, `.expect_status`, `.expect_body` | | | `http` only |
| `health_check.query`, `.record`, `.transport`, `.expect` | | | `dns` only |
| `health_check.sni`, `.min_valid` | | | `tls` only |
| `passive.enabled` | bool | `true` | Eject a backend after consecutive traffic failures. See [Health checks](health-checks.md#passive-detection) |
| `passive.failures` | number | `5` | Consecutive upstream connect (or UDP reply) failures that eject |
| `passive.eject_for` | duration | `30s` | Time out of rotation before re-admission |
| `connect_timeout` | duration | `3s` | Opening a connection to a backend, HTTP and TCP; `504` |
| `read_timeout` | duration | `60s` | HTTP: longest wait between two reads from a backend; `504` |
| `write_timeout` | duration | `60s` | HTTP: longest wait between two writes to a backend |
| `idle_timeout` | duration | `60s` | HTTP: how long an unused backend connection is kept for reuse |
| `retry` | bool | `true` | HTTP: a failed connect tries one other available backend. Never once bytes were sent; counted in `keel_upstream_retries_total` |
| `backend "<address>"` | block | required | `ip:port`; at least one |
| `backend.weight` | number | `1` | Relative weight, 1–1000; honoured by every algorithm |

Pools may be defined in any file of the config directory; a name defined twice is a load error naming both places. See [Load balancing](load-balancing.md) for details on algorithms and drain behavior.

---

## vhost

Virtual host routing. The label is the host: an exact hostname, or `*` for everything no other vhost matches.

```hcl
vhost "api.example.com" {
  pool = "api"

  tls {
    cert = "/etc/keel/certs/api.crt"
    key  = "/etc/keel/certs/api.key"
  }

  forwarded_headers {
    mode = "replace"
  }

  cache {
    enabled = true
    ttl     = "1m"
  }
}

vhost "*" {
  pool = "default"
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `pool` | string | none | The pool for every path. Excludes `route` blocks |
| `route "<path>"` | block | none | Path-prefix routing with a `pool` each, and optional `cache`, `allow` and `deny`; see [Virtual hosts](virtual-hosts.md#path-routing). Excludes `pool` |
| `allow`, `deny` | list | none | Client networks or addresses; see [Access lists](virtual-hosts.md#access-lists) |
| `max_body_size` | size | none | Largest request body, e.g. `10M`; larger gets `413`. A route's own replaces the vhost's |
| `tls.cert` | string | none | PEM certificate (BYO cert). A relative path is a file of the config directory; see [Files the config names](#files-the-config-names) |
| `tls.key` | string | none | PEM private key (BYO cert), same resolution as `tls.cert` |
| `tls.acme` | bool or string | `false` | `true` = ACME via issuer `default`; a string names an `acme` issuer — see [ACME](acme.md) |
| `redirect_http` | bool | `false` (`true` when `tls.acme`) | 301 plain HTTP to HTTPS |
| `forwarded_headers.mode` | string | `replace` | `replace`, `append`, or `off` |
| `forwarded_headers.trusted_proxies` | list | none | CIDRs trusted in `append` mode and by access lists |
| `cache.enabled` | bool | `false` | Enable caching for this vhost |
| `cache.ttl` | duration | none | Fallback TTL when the origin gives no freshness; see [Caching](caching.md#what-is-stored) |
| `default_action` | block | none | Answer directly without a pool: `redirect` or `status`/`body` — see [Virtual hosts](virtual-hosts.md#default-action). Excludes `pool` and `route` |

See [Virtual hosts](virtual-hosts.md) for host matching rules, path routing, and TLS hot-swap.

---

## acme

Automatic TLS via any ACME v2 CA, organized as named issuers. Vhosts reference an issuer with `acme = true` (the issuer named `default`) or `acme = "<name>"` in their `tls` block. See [ACME](acme.md).

```hcl
acme {
  storage      = "/var/lib/keel/acme"
  renew_before = "30%"

  issuer "default" {
    email = "ops@example.com"
  }

  issuer "internal" {
    directory = "https://ca.corp.internal/acme/directory"
    root_ca   = "/etc/keel/corp-root.pem"
  }
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `storage` | string | `/var/lib/keel/acme` | Certs, keys, accounts, challenge tokens. Absolute, and outside the config directory |
| `renew_before` | string | `30%` | Renew when less than this remains: `%` of lifetime, or a duration such as `20d` |
| `issuer.email` | string | none | ACME account contact for this issuer |
| `issuer.directory` | string | `https://acme-v02.api.letsencrypt.org/directory` | ACME v2 directory URL |
| `issuer.root_ca` | string | none | Extra trust root for the ACME API (internal or self-signed CAs) |
| `issuer.renew_before` | string | global value | Per-issuer renewal override |

---

## certificate

Standalone certificate requests: hostnames Keel obtains certificates for without terminating TLS itself (TCP / TLS-passthrough backends). Keel answers the HTTP-01 challenge and writes `{host}.crt` / `{host}.key` to `acme.storage`. Allowed in any file of the config directory. See [ACME](acme.md).

```hcl
certificate "db.example.com" {
  issuer = "default" # optional; "default" when omitted
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `issuer` | string | `default` | Issuer name from the `acme` block. Ignored when `cert`/`key` are set |
| `cert`, `key` | string | none | Bring-your-own PEM files instead of ACME issuance, resolved like `tls.cert`; see [TCP proxying](tcp-proxying.md) |

The label is the hostname. ACME cannot issue a wildcard; a `certificate` with `cert` and `key` can be one.

---

## cluster

Needed for peers: without it the node is a cluster of one and opens no peer port.

```hcl
cluster {
  addr      = "0.0.0.0:7654"  # bind address for peer connections
  advertise = "10.0.0.1:7654" # address the other nodes connect to
  secret    = "change-me"
  join      = ["10.0.0.2:7654"] # omit on the node that starts the cluster
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `addr` | string | `0.0.0.0:7654` | Peer listen address; must not be a listener's address |
| `advertise` | string | `addr` | Address announced to the other nodes. Required when `addr` is unspecified (`0.0.0.0`, `::`) |
| `secret` | string | none | Shared secret for join authentication; required |
| `join` | list | none | Members to join through on first start, `ip:port` each |

The node ID is not a setting: it is generated on first start and kept in `keel.state_dir` — see [Node identity](cluster.md#node-identity). The Raft store and the node's certificates live there too; see [Restarts](cluster.md#restarts).

See [Cluster](cluster.md) for starting, joining and restarts.

---

## control

Remote control for `keelctl`. Off unless `remote` is set. mTLS is mandatory; see [Remote control](keelctl.md).

```hcl
control {
  remote {
    address = "0.0.0.0:10789"
    allow   = ["10.1.2.0/24"]
  }
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `remote.address` | string | none | TCP listen address for keelctl, `ip:port`; not a listener's address |
| `remote.allow` | list | none | Optional source-CIDR restriction; empty = any source. mTLS stays mandatory |

The master binds the port and the control worker serves it. See [Security](security.md#processes-and-what-each-can-read). The control CA (`ca.crt` / `ca.key`) lives in `control/` under `keel.state_dir`.

---

## Files and layout

```
/etc/keel/
├── node.hcl                  this node: keel.workers/user/group, cluster, control, metrics
└── config/                   the load balancer
    ├── keel.hcl              loaded first
    ├── pools/
    │   ├── api.hcl           then every other *.hcl below config/,
    │   └── web.hcl           recursively, in path order
    ├── vhosts/
    │   └── api.example.com.hcl
    └── certs/
        └── api.crt           not parsed; travels with the directory
```

The node file may be missing: at the default path that means every node setting at its default. The config directory must hold `keel.hcl`.

- `pool` names are unique across the directory; `listener`, `vhost` and `certificate` blocks add up in load order.
- `keel`, `access_log`, `acme`, `cache`: `keel.hcl` only.
- `cluster`, `control`, `metrics`: node file only.
- A `*.yaml` or `*.yml` file in the directory is a load error.

Names starting with `.` (editor and temporary files) are skipped. Every file must be UTF-8 text.

### Files the config names

`tls.cert`, `tls.key`, `cert` and `key` of `certificate` blocks, a listener's `tls_ca` and an ACME issuer's `root_ca` name files:

| Path | Read from | Distributed by |
|---|---|---|
| relative, e.g. `certs/api.crt` | the config directory | the config directory; in a cluster, every push |
| absolute, e.g. `/etc/keel/certs/int.crt` | each node's disk | the operator |

A relative path the config directory does not hold is a load error, and in a cluster a refused push. Certificate and key files (`tls`, `certificate`) are read and parsed at load, absolute ones from the loading node's disk: a missing or unusable one is a load error naming the vhost or certificate. `tls_ca` and `root_ca` given as absolute paths are read when used. Keys are PEM files, never inline in the config.

`access_log.dir`, `cache.disk.path` and `acme.storage` are directories Keel writes to on each node, and must be absolute. `acme.storage` must be outside the config directory, which a push replaces.

### In a cluster

The config directory is identical on every node. The same content exists three times: the committed entry in Raft, each node's Raft store on disk, and the files under `/etc/keel/config/`. The files are for editing and inspection. The node file is never replicated.

Each push is a version: the whole file set by relative path, committed through Raft. The version number is its log index. Each node builds a version against its own node file, applies it, then writes the files. The directory therefore holds the last version that worked on that node. Files missing from a version are deleted. A version that fails on a node leaves it on the previous one, logs `config version cannot be applied`, and leaves the files unchanged.

A new version comes from:

- `keel config push <dir>` or `keelctl config push <dir>`. A single file is pushed as `keel.hcl`. A follower forwards the push to the leader.
- `SIGHUP` or `keel config reload` on a node: its config directory becomes the next version.
- Starting a node whose files differ from the version it last applied. The later push wins.
- The first start of a new cluster, one node included: the first node's files become version 1.

A push is validated before it is committed. Without a reachable majority it fails at once: `no quorum: 1 of 3 members reachable; config not pushed`. Traffic is unaffected either way.

---

## Checking a configuration

```bash
keel config validate                       # /etc/keel/node.hcl and its config directory
keel config validate /srv/next/node.hcl    # another node file and its config directory
keel config validate --diff                # what changed since the running node's version
```

`keel config validate` loads the node file and its config directory as a start or reload does — syntax, attributes, types, references between blocks, the files the config names — like `nginx -t`. It needs no running node. It is silent: exit 0 when the files can go live, 1 when they cannot, with the error on stderr. Flags add output:

| Flag | |
|---|---|
| `--full` | Every file that is loaded, in load order, each under `# file <path>`; like `nginx -T` |
| `--diff` | A diff against the version the running node serves, then each change marked `reload` or `restart`, by the rules in [Hot reload](#hot-reload) |
| `--list` | Files not in canonical format |
| `--write` | Rewrite them in canonical format, with the unit added to a bare number given to a duration (`ttl = 30` becomes `ttl = "30s"`) |
| `--check` | Exit 3 when a file is not in canonical format |

```
$ keel config validate --diff
--- applied/pools/web.hcl
+++ /etc/keel/config/pools/web.hcl
@@ -1,3 +1,4 @@
 pool "web" {
   backend "10.0.0.1:8080" {}
+  backend "10.0.0.2:8080" {}
 }
restart pool "web": backend "10.0.0.2:8080" added
```

The canonical format is the reference formatter's (`terraform fmt`, `tofu fmt`): two-space indentation, one space around `=`, the `=` of consecutive attributes aligned, trailing comments aligned. Only whitespace changes. Formatting never affects the exit status, except with `--check`. Keel does not format on its own.

Output is colored on a terminal; `--no-color` or `NO_COLOR` turns it off.

---

## Hot reload

`SIGHUP` to the master or `keel config reload` pushes the node's config directory as the next version, one node or many; see [In a cluster](#in-a-cluster). A directory that does not load is refused and nothing changes. Every node's control worker applies the version and hands it to its workers without dropping connections. A worker started later gets the applied version from the control worker. The node file is read at start only.

What reloads without restart, open connections untouched:
- Virtual host rules: hosts, routes, `forwarded_headers`, cache rules, `redirect_http`, `default_action`, access lists, body limits
- Pools: added and removed, backends added and removed, weights, algorithms, `health_check`, `passive`, timeouts and `retry`. A removed backend or pool drains: no new connections, open ones finish. A backend that stays keeps its connections, drain, health and ejection. A new backend in a checked pool gets traffic after its first probe; a changed check keeps the verdicts and counts its thresholds afresh
- TLS certificates: certificate files of vhosts and `certificate` blocks are re-read with every applied version
- ACME: hosts, issuers and `certificate` blocks. A new host is issued at once; a vhost switched from its own certificate to `tls.acme` serves the old one until the issued one arrives

What requires a process restart:
- `cache` storage, `access_log`, `metrics` and `control`
- Every setting in the node file
- Listeners
- Every setting in the `keel` block, including worker count, user/group and the UDP flow settings

Backends are matched on IP and port: `[2001:db8::1]:80` and `[2001:db8:0::1]:80` are the same backend.

A version applies the same way on every node: what reloads live above takes effect on push, the rest needs a restart of each node.
