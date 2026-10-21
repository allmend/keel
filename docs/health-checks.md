# Health Checks

A pool with a `health_check` block is probed continuously. A backend that fails `unhealthy_threshold` consecutive probes gets no new traffic; one that passes `healthy_threshold` consecutive probes gets traffic again. This applies to every algorithm and to HTTP, TCP and UDP listeners. Without `health_check`, every backend is healthy.

The block's label is the probe type. Each type takes its own attributes on top of the common ones; any other attribute is a load error.

## Common attributes

```hcl
pool "web" {
  health_check "http" {
    interval            = "10s" # time between rounds
    timeout             = "2s"  # per probe
    healthy_threshold   = 2     # successes to return
    unhealthy_threshold = 3     # failures to leave
    port                = 9000  # optional: probe this port instead
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

The control worker runs the checks, once per node: a backend gets one probe per interval, whatever the number of workers. Every worker acts on the same verdict, read from shared memory, and a worker started later reads the current one. While the control worker is down, the workers keep the last verdicts; a restarted control worker continues from them.

A checked backend starts `initializing` and takes no traffic until its first probe: a pass makes it healthy, a failure unhealthy. Thresholds apply from then on. The first round runs within a second of startup, and workers serve once every checked backend has a verdict, at most the longest `timeout` plus two seconds; connections meanwhile wait in the listen queue. Without a health check a backend is healthy at once.

## Probe types

### tcp

A TCP connection succeeds. Proves that something accepts connections on the port, nothing about the service behind it.

```hcl
health_check "tcp" {}
```

### udp

Sends an empty datagram and waits `timeout` for ICMP port-unreachable, which marks the backend down; silence is healthy. It detects a closed port only, and not at all when the host drops or rate-limits ICMP errors.

Use for UDP services without a request/response exchange; `dns` and `ntp` probe the service itself.

```hcl
health_check "udp" {
  timeout = "1s"
}
```

### http

A `GET` to `path`. Healthy on a status from 200 to 399 (or one `expect_status` allows) with `expect_body`, if set, in the first 64 KiB of the body. Redirects are not followed: a `path` that redirects counts as healthy, so point it at an endpoint that answers itself.

```hcl
health_check "http" {
  path          = "/health"
  host          = "app.internal"
  tls           = false
  expect_status = [200, "204", "300-308"]
  expect_body   = "\"ok\""
}
```

| Attribute | Default | Notes |
|---|---|---|
| `path` | `/health` | Starts with `/` |
| `host` | backend `ip:port` | Host header, and SNI with `tls` |
| `tls` | `false` | HTTPS; the backend certificate is not verified |
| `expect_status` | 200–399 | Codes (`204`, `"204"`) and inclusive ranges (`"300-308"`) |
| `expect_body` | none | Substring the body must contain |

### dns

One query. Healthy on RCODE `NOERROR` and, with `expect`, that address in the answer section. Any other RCODE, a malformed reply or no reply within `timeout` fails. The name should be one the server answers for: for an authoritative server, a name in its zones.

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

One ICMP echo request to the backend's address; healthy on a reply within `timeout`. It checks the host, not the service: a machine whose daemon died still answers.

Use where nothing else can be probed, such as one-way UDP services (syslog collectors).

```hcl
health_check "icmp" {}
```

Keel uses ICMP datagram sockets, not raw sockets. On Linux the master opens one IPv4 and one IPv6 socket for the control worker before forking and dropping privileges; without root, the user's group must be inside `net.ipv4.ping_group_range`. On other platforms the control worker opens them. If no socket can be opened, Keel logs an error once and `icmp` probes of that address family pass. Reasons: `no echo reply within 1000ms`, `unreachable`.

### tls

A TLS handshake completes. With `min_valid`, the presented certificate must also stay valid at least that long. The chain is not verified.

Use for TLS-on-connect services behind `tcp` listeners (Redis `tls-port`, LDAPS). A certificate about to expire takes the backend out of rotation, with the days left in the reason.

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

`keel status` and `keel backend list` show health and, for a checked backend, the reason of its last failed probe, kept by the control worker:

```
  web (2 backends)
    10.0.0.11:8080             active      healthy     3 conn
    10.0.0.12:8080             active      unhealthy   0 conn  (connect: ConnectRefused)
```

`unchecked`: the pool has no `health_check`. `initializing`: no probe has answered yet. `ejected`: passive detection took the backend out, e.g. `3 consecutive connect failures`. Other reasons: `connection refused`, `timeout after 2000ms`, `port unreachable`, `status 503`, `body lacks "ok"`, `connect: ConnectTimedout`.

Transitions are logged at `warn` (unhealthy, with the reason) and `info` (healthy), and set `keel_backend_healthy`.

## Behavior

- With no available backend, HTTP gets 503, TCP connections are closed and UDP datagrams are dropped, all as `no_backend`.
- Health, ejection and drain are independent: a draining backend is still probed, an unhealthy one can be drained.
- Selection reads the health state each time: a change applies to the next request, connection or flow.
- `health_check` and `passive` changes apply on reload. A changed check keeps each backend's verdict and counts its thresholds afresh; a new backend waits for its first probe.
