# Health Checks

A pool with a `health_check` block is probed continuously. A backend that fails `unhealthy_threshold` consecutive probes gets no new traffic; one that passes `healthy_threshold` consecutive probes gets traffic again. This applies to every algorithm and to HTTP, TCP and UDP listeners. Without `health_check`, every backend is healthy.

The block's label is the probe type. Each type takes its own attributes on top of the common ones; any other attribute is a load error.

## Common attributes

```hcl
pool "web" {
  health_check "http" {
    interval            = "10s"
    timeout             = "2s"
    healthy_threshold   = 2
    unhealthy_threshold = 3
    port                = 9000
  }

  backend "10.0.0.1:8080" {}
}
```

| Attribute | Default | Notes |
|---|---|---|
| label | required | `tcp`, `udp`, `http`, `dns`, `ntp`, `icmp`, `tls` |
| `interval` | `10s` | Time between rounds, ±10% jitter; at least `1s` |
| `timeout` | `2s` | A probe not finished by then fails with `timeout` |
| `healthy_threshold` | `2` | Consecutive successes to return to service |
| `unhealthy_threshold` | `3` | Consecutive failures to leave service |
| `port` | traffic port | Probe another port on the same address |

Backends start healthy. The first round runs within a second of startup. Every worker runs its own checks: a backend gets one probe per worker per interval.

## Probe types

### tcp

A TCP connection succeeds.

```hcl
health_check "tcp" {}
```

### udp

Sends an empty datagram and waits `timeout` for ICMP port-unreachable, which marks the backend down; silence is healthy. It detects a closed port only, and not at all when the host drops or rate-limits ICMP errors.

```hcl
health_check "udp" {
  timeout = "1s"
}
```

### http

A `GET` to `path`. Healthy on 2xx (or a code in `expect_status`) with `expect_body`, if set, in the first 64 KiB of the body.

```hcl
health_check "http" {
  path          = "/health"
  host          = "app.internal"
  tls           = false
  expect_status = [200, 204]
  expect_body   = "\"ok\""
}
```

| Attribute | Default | Notes |
|---|---|---|
| `path` | `/health` | Starts with `/` |
| `host` | backend `ip:port` | Host header, and SNI with `tls` |
| `tls` | `false` | HTTPS; the backend certificate is not verified |
| `expect_status` | any 2xx | Acceptable codes |
| `expect_body` | none | Substring the body must contain |

### dns

One query. Healthy on RCODE `NOERROR` and, with `expect`, that address in the answer section. Any other RCODE, a malformed reply or no reply within `timeout` fails.

```hcl
health_check "dns" {
  query     = "example.com"
  record    = "A"
  transport = "udp"
  expect    = "93.184.216.34"
}
```

| Attribute | Default | Notes |
|---|---|---|
| `query` | required | Name to resolve |
| `record` | `A` | `A` or `AAAA` |
| `transport` | `udp` | `udp` or `tcp` |
| `expect` | none | IPv4 for `A`, IPv6 for `AAAA`. Without it `NOERROR` passes, also with an empty answer |

Reasons: `rcode NXDOMAIN`, `rcode SERVFAIL`, `answer lacks 10.0.0.1`, `response id mismatch`, `short response`.

### ntp

One client-mode request (RFC 5905). Healthy on a server-mode reply with a matching originate timestamp and a non-zero stratum. Stratum zero (kiss-o'-death) fails with its code, e.g. `kiss-of-death RATE`.

```hcl
health_check "ntp" {
  interval = "30s"
}
```

### icmp

One ICMP echo request to the backend's address; healthy on a reply within `timeout`. It checks the host, not the service.

```hcl
health_check "icmp" {}
```

Keel uses ICMP datagram sockets, not raw sockets. On Linux the master opens one IPv4 and one IPv6 socket per worker before forking and dropping privileges; without root, the user's group must be inside `net.ipv4.ping_group_range`. On other platforms each worker opens its own. If no socket can be opened, Keel logs an error once and `icmp` probes of that address family pass. Reasons: `no echo reply within 1000ms`, `unreachable`.

### tls

A TLS handshake completes. With `min_valid`, the presented certificate must also stay valid at least that long. The chain is not verified.

```hcl
health_check "tls" {
  sni       = "cache.example.com"
  min_valid = "14d"
}
```

| Attribute | Default | Notes |
|---|---|---|
| `sni` | none | Server name to send; none when unset |
| `min_valid` | none | Duration. Fails when the certificate expires sooner, or has expired |

Reasons: `handshake: …`, `certificate expires in 3 days`, `certificate expired 2 days ago`, `no certificate presented`.

## Passive detection

Every pool also watches its traffic. A backend whose upstream connections (HTTP, TCP) or UDP replies fail `failures` times in a row is ejected for `eject_for`, then re-admitted. Each worker counts and ejects on its own. On by default, with or without `health_check`.

```hcl
pool "web" {
  passive {
    enabled   = true
    failures  = 5
    eject_for = "30s"
  }

  backend "10.0.0.1:8080" {}
}
```

A failure is an upstream connection that could not be established, or ICMP port-unreachable for a UDP flow. Slow responses, HTTP 5xx and errors after connecting do not count. A successful connection or a UDP reply resets the count.

The last available backend of a pool is never ejected. An ejected backend keeps being probed; its ejection ends on schedule whatever the probe says.

Ejections show in `keel status` as `ejected` with the streak, in the log at `warn` (`info` on re-admission), and in `keel_backend_ejected` and `keel_backend_ejections_total`.

## Status

`keel status` and `keel backend list` show health and, for an unhealthy backend, the reason of its last failed probe:

```
  web (2 backends)
    10.0.0.11:8080             active      healthy     3 conn
    10.0.0.12:8080             active      unhealthy   0 conn  (connect: ConnectRefused)
```

`unchecked`: the pool has no `health_check`. `ejected`: passive detection took the backend out, e.g. `3 consecutive connect failures`. Other reasons: `connection refused`, `timeout after 2000ms`, `port unreachable`, `status 503`, `body lacks "ok"`, `connect: ConnectTimedout`.

Transitions are logged at `warn` (unhealthy, with the reason) and `info` (healthy), and set `keel_backend_healthy`.

## Behavior

- With no available backend, HTTP gets 503, TCP connections are closed and UDP datagrams are dropped, all as `no_backend`.
- Health, ejection and drain are independent: a draining backend is still probed, an unhealthy one can be drained.
- Selection reads the health state each time: a change applies to the next request, connection or flow.
- `health_check` and `passive` changes take a restart.
