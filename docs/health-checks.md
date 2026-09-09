# Health Checks

A pool with a `health_check` block is probed continuously. A backend that
fails `unhealthy_threshold` consecutive probes stops receiving new traffic;
one that passes `healthy_threshold` consecutive probes receives traffic
again. The check applies to every algorithm, including `least_connections`,
and to HTTP, TCP, and UDP listeners alike. A pool without `health_check`
treats every backend as healthy.

`type` selects the probe. Each type accepts only its own fields on top of the
common ones; an unknown field, or a field that belongs to a different type,
is a validation error at startup.

## Common fields

```yaml
pools:
  web:
    health_check:
      type: http
      interval: 10s            # time between rounds
      timeout: 2s              # per-probe limit
      healthy_threshold: 2     # consecutive successes to mark healthy
      unhealthy_threshold: 3   # consecutive failures to mark unhealthy
      port: 9000               # optional: probe this port instead of the traffic port
```

| Field | Default | Notes |
|---|---|---|
| `type` | required | `tcp`, `udp`, `http`, `dns`, `ntp`, `icmp`, `tls` |
| `interval` | `10s` | Time between rounds. `500ms`, `10s`, `2m`, `1h` |
| `timeout` | `2s` | A probe that has not finished by then fails with `timeout` |
| `healthy_threshold` | `2` | Consecutive successes before an unhealthy backend returns to service |
| `unhealthy_threshold` | `3` | Consecutive failures before a healthy backend is removed |
| `port` | traffic port | Probe a different port on the same address, for services with a separate health or admin port |

Backends start healthy. The first round runs within a second of startup;
each round after that is spaced by `interval` ±10% so that worker processes
do not probe in lockstep. Every worker runs its own checks, so a backend
receives one probe per worker per interval.

## Probe types

### tcp

A TCP connection to the backend succeeds. Proves that something accepts
connections on the port, nothing about the service behind it.

```yaml
health_check:
  type: tcp
```

### udp

Sends an empty datagram and waits `timeout` for an ICMP port-unreachable
reply. Silence counts as healthy; port-unreachable marks the backend down.

UDP has no handshake, so this probe can only prove a port closed, never
open. A host that drops or rate-limits ICMP errors (Linux rate-limits them
by default) makes a dead port look alive to this probe. Prefer a protocol
probe such as `dns` where the service offers one; use `udp` for services
without a request/response exchange.

```yaml
health_check:
  type: udp
  timeout: 1s
```

### http

A `GET` to `path`. Healthy when the status is 2xx, or one of `expect_status`
when set, and when `expect_body` (if set) appears in the first 64 KiB of the
body.

```yaml
health_check:
  type: http
  path: /health
  host: app.internal        # Host header and TLS SNI; default: backend address
  tls: false                # HTTPS to the backend
  expect_status: [200, 204] # default: any 2xx
  expect_body: '"ok"'       # substring match
```

| Field | Default | Notes |
|---|---|---|
| `path` | `/health` | Must start with `/` |
| `host` | backend `ip:port` | Sent as the Host header and, with `tls: true`, as SNI |
| `tls` | `false` | Connect with TLS. The backend certificate is not verified: the probe tests liveness, not identity |
| `expect_status` | any 2xx | Exact list of acceptable codes |
| `expect_body` | none | Substring the body must contain |

### dns

One query for `query` and reads the answer. Healthy when the response has
RCODE `NOERROR`; with `expect`, that address must also appear in the answer
section. Any other RCODE (`NXDOMAIN`, `SERVFAIL`, `REFUSED`), a malformed
reply, or no reply within `timeout` fails the probe. Choose a name the
resolver is expected to resolve; for an authoritative server, a name in one
of its zones.

```yaml
health_check:
  type: dns
  query: example.com
  record: A              # A or AAAA; default A
  transport: udp         # udp or tcp; default udp
  expect: 93.184.216.34  # optional; must match the record family
  interval: 5s
  timeout: 1s
```

| Field | Default | Notes |
|---|---|---|
| `query` | required | Name to resolve. Validated as a DNS name |
| `record` | `A` | `A` or `AAAA` |
| `transport` | `udp` | `udp` or `tcp` |
| `expect` | none | IPv4 for `A`, IPv6 for `AAAA`. Without it, `NOERROR` alone passes, including an empty answer section |

Reasons: `rcode NXDOMAIN`, `rcode SERVFAIL`, `answer lacks 10.0.0.1`,
`response id mismatch`, `short response`.

### ntp

One client-mode request (RFC 5905). Healthy on a server-mode reply whose
originate timestamp matches the request and whose stratum is not zero.
Stratum zero is a kiss-o'-death packet: the server answered but refuses to
serve time, and the reason shows its code, for example `kiss-of-death RATE`.

```yaml
health_check:
  type: ntp
  interval: 30s
  timeout: 2s
```

Probing at short intervals from many workers can itself trigger `RATE`
kiss-o'-death replies from public servers; keep `interval` at 30 seconds or
more for those.

### icmp

One ICMP echo request to the backend's address; healthy on an echo reply
within `timeout`. This checks the host, not the service: a machine whose
daemon has died still answers ping. Use it for pools where nothing else is
possible, such as one-way UDP services (syslog collectors), or as a coarse
check alongside passive detection.

```yaml
health_check:
  type: icmp
  interval: 5s
  timeout: 1s
```

Keel uses ICMP datagram sockets, not raw sockets. When started as root on
Linux, the master opens one IPv4 and one IPv6 socket per worker before
dropping privileges and the workers inherit them, like the listeners. When
started unprivileged, each worker opens its own; that works on macOS, and on
Linux when the worker's group is inside `net.ipv4.ping_group_range`. If no
socket can be opened, Keel logs an error once and `icmp` probes for that
address family pass unconditionally rather than take every backend out of
service. Reasons: `no echo reply within 1000ms`, `unreachable`.

### tls

Completes a TLS handshake with the backend. With `min_days_valid`, the
certificate the backend presented must also stay valid for at least that
many days. The chain is not verified: the probe answers "does this backend
speak TLS, and is its certificate about to expire", not whether the
certificate is trusted — in passthrough mode that is the client's decision.
Meant for TLS-on-connect services behind `tcp_pool` listeners (Redis
`tls-port`, LDAPS, MQTT over TLS) and for HTTPS backends where `http` is
not wanted.

```yaml
health_check:
  type: tls
  sni: cache.example.com   # optional; without it no SNI is sent
  min_days_valid: 14       # optional
```

| Field | Default | Notes |
|---|---|---|
| `sni` | none | Server name for the handshake, needed by backends that pick their certificate by SNI |
| `min_days_valid` | none | Fail when the presented certificate expires sooner. Also fails on an already expired certificate |

Reasons: `handshake: …` (with rustls's description), `certificate expires
in 3 days`, `certificate expired 2 days ago`, `no certificate presented`.

A certificate about to expire makes the backend unhealthy, which removes
it from rotation. Set `min_days_valid` below the renewal lead time of the
backend's certificates so a healthy renewal cycle never trips it, and read
`keel status` for the countdown in the reason.

## Passive detection

Independently of the active probe, every pool watches its real traffic. A
backend whose upstream connections (HTTP and TCP) or UDP replies fail
`failures` times in a row is ejected for `eject_for`, then re-admitted.
This catches a backend that dies between probe rounds, and it works for
pools without a `health_check` block at all. On by default.

```yaml
pools:
  web:
    passive:
      enabled: true    # default
      failures: 5      # consecutive failures that eject; default 5
      eject_for: 30s   # default
```

What counts as a failure is deliberately narrow: an upstream connection
that could not be established (HTTP and TCP pools), or a UDP flow whose
backend answered with ICMP port-unreachable. Slow responses, HTTP 5xx, and
errors after the connection was established do not count; they can be the
client's or the request's fault. A successful connection or a UDP reply
resets the streak.

The last available backend of a pool is never ejected, whatever its
failures: with nothing else to send traffic to, ejecting it would fail every
request instead of some. Ejection and active health are separate
states; an ejected backend keeps being probed, and its ejection expires on
schedule regardless of the probe result.

Ejections show in `keel status` as `ejected` with the streak that caused
them, in the log at `warn` (with `info` on re-admission), and in the
metrics `keel_backend_ejected` and `keel_backend_ejections_total`.

## Status output

`keel status` and `keel backend list` show the health column and, for an
unhealthy backend, the reason from its last failed probe:

```
  web (2 backends)
    10.0.0.11:8080             active      healthy     3 conn
    10.0.0.12:8080             active      unhealthy   0 conn  (connect: ConnectRefused)
```

`unchecked` means the pool has no `health_check`; `ejected` means passive
detection took the backend out (the reason gives the streak, for example
`3 consecutive connect failures`). Reasons are short and stable: `connection refused`, `timeout after 2000ms`, `port unreachable`,
`status 503`, `body lacks "ok"`, and for HTTP connect failures Pingora's
error type such as `connect: ConnectTimedout`.

The transitions are logged at `warn` (unhealthy, with the reason) and `info`
(healthy) and drive `keel_backend_healthy` (see [Metrics](metrics.md)).

## Behavior notes

- If every backend of a pool is unhealthy, HTTP requests to that pool get a
  502, TCP connections are closed, and UDP datagrams are dropped, all logged
  as `no_backend`.
- Health, ejection, and drain are independent: a draining backend keeps
  being probed, an unhealthy backend can be drained, and an ejected backend
  is re-admitted on schedule.
- Backend selection consults the health state at selection time, so a flip
  takes effect on the next request, connection, or UDP flow.
- Changing `health_check` requires a restart.
