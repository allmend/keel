# UDP Proxying (L4)

A listener with `udp_pool` forwards datagrams to a backend pool. Routing is
listener → pool: there is no Host header or path at L4, so vhosts and routes
do not apply. The pool is an ordinary pool — algorithms, weights, health
checks, drain, and connection counting behave the same as for HTTP and TCP.

UDP has no connection. Keel tracks a **flow** per client `ip:port`: the first
datagram from a new client selects a backend, and every following datagram
from that client goes to the same backend until the flow has been idle for
`keel.udp_flow_timeout_seconds` (default 30). Replies from the backend are
sent back to the client for as long as the flow exists. A flow's end is the
only point at which its backend is released and its log entry written.

UDP is forwarded as-is. Keel does not terminate DTLS or QUIC and does not
inspect payloads; the listener rejects `tls: true`.

## Configuration

```yaml
keel:
  udp_flow_timeout_seconds: 30   # idle time after which a flow expires

listeners:
  - address: 0.0.0.0:53
    udp_pool: resolvers         # name of a pool below; any UDP protocol is forwarded as-is

pools:
  resolvers:
    algorithm: round_robin
    health_check:
      type: dns                 # a real query; see health-checks.md
      query: example.com
      interval: 10s
      timeout: 2s
    backends:
      - address: 10.0.0.11:53
      - address: 10.0.0.12:53
      - address: 10.0.0.13:53
```

| Listener field | Notes |
|---|---|
| `udp_pool` | Name of the pool to forward to; must exist in `pools`. The value is a pool name, not a protocol: datagrams of any UDP protocol are forwarded unchanged. Makes the listener UDP-only: vhosts and routes are ignored |
| `tls` | Rejected together with `udp_pool` — datagrams are never terminated |
| `tcp_pool` | Rejected on the same listener entry. To serve both protocols on one port, add two listener entries with the same `address` (see the DNS example) |
| `proxy_protocol` | Ignored for UDP |

| `keel` field | Default | Notes |
|---|---|---|
| `udp_flow_timeout_seconds` | `30` | Idle time before a flow expires. Applies to every UDP listener. Must be at least 1 |

Config validation fails at startup for an unknown `udp_pool`, a
`udp_pool` + `tls: true` combination, `udp_pool` and `tcp_pool` on the same
listener, or a zero timeout.

Health checks for UDP pools: `dns` and `ntp` ask the service itself; `udp`
sends an empty datagram and marks a backend down only on ICMP
port-unreachable, so it proves a closed port but not a working service. See
[Health checks](health-checks.md). A pool without a health check keeps
selecting a backend that no longer answers; the flow then ends with
`upstream_recv` (see below) and the client's next datagram re-selects — for
round robin this reaches a different backend, for consistent hashing the
same one.

## Example: DNS

Resolvers answer on 53/udp and 53/tcp (the latter for truncated responses
and zone transfers). One listener entry per protocol, both referencing the
same pool:

```yaml
keel:
  udp_flow_timeout_seconds: 10   # DNS exchanges are short; expire flows early

listeners:
  - address: 0.0.0.0:53
    udp_pool: resolvers
  - address: 0.0.0.0:53
    tcp_pool: resolvers

pools:
  resolvers:
    algorithm: round_robin
    health_check:
      type: dns
      query: example.org
      interval: 5s
      timeout: 1s
    backends:
      - address: 10.0.0.11:53
      - address: 10.0.0.12:53
```

```bash
dig @dns.example.com example.org A
dig @dns.example.com +tcp example.org AXFR
```

A stub resolver typically sends each query from a fresh source port, so each
query is its own flow. A recursive resolver forwarding through Keel reuses
source ports and keeps a flow alive across queries.

Port 53 is privileged. Started as root, the master binds it before dropping
privileges, the same as for TCP listeners; see
[Behavior notes](#behavior-notes).

## Example: syslog

Syslog over UDP (RFC 5426) is one datagram per message with no reply.
Flows carry traffic in one direction only; `bytes_out` and `packets_out`
stay zero in the log entry:

```yaml
listeners:
  - address: 0.0.0.0:514
    udp_pool: syslog

pools:
  syslog:
    algorithm: consistent_hash   # one sender keeps reaching the same collector
    health_check:
      type: udp                  # syslog has no reply to check; detect closed ports only
    backends:
      - address: 10.0.0.21:514
      - address: 10.0.0.22:514
```

With consistent hashing, messages from one host stay in order on one
collector while the pool composition is stable.

## Session affinity

Within a flow, affinity is absolute: every datagram from one client `ip:port`
reaches the backend chosen for the first one until the flow expires. Across
flows it depends on the algorithm:

- `consistent_hash` — the hash key is the client `ip:port`, so a client that
  returns after its flow expired reaches the same backend while the pool
  composition is stable.
- `round_robin`, `random`, `least_connections` — apply per flow. A returning
  client may land on a different backend. `least_connections` counts open
  flows.

Clients behind a NAT share a source IP but usually not a source port, so
they are separate flows.

## Drain

Flows register in the same per-backend counters as HTTP requests and TCP
connections:

```bash
keel status                                  # shows open flows per backend
keel backend drain 10.0.0.11:53 --wait       # works remotely via keelctl too
```

Draining stops new flows to the backend immediately; existing flows run
until they expire, and the backend moves to `removed` when the last one
does. A drain therefore completes at most `udp_flow_timeout_seconds` after
the last datagram on the backend's last flow — a client that keeps sending
holds its flow, and the drain, open. `--wait` shows the live count.

## Observability

One NDJSON entry per flow in `access_udp_<pool>.log`, written when the flow
ends — fields and error values in
[Access logging](access-logging.md#udp-log-format).

Metrics (see the [metrics reference](metrics.md#udp-l4-metrics) for the
full list):

| Metric | Type | Labels |
|---|---|---|
| `keel_udp_flows_total` | counter | `pool`, `backend` |
| `keel_udp_packets_in_total` / `keel_udp_packets_out_total` | counter | `pool`, `backend` |
| `keel_udp_bytes_in_total` / `keel_udp_bytes_out_total` | counter | `pool`, `backend` |
| `keel_udp_errors_total` | counter | `pool`, `reason` (`no_backend`, `upstream_bind`, `upstream_send`, `upstream_recv`, `downstream_send`) |
| `keel_active_connections` | gauge | `pool`, `backend` — open flows; shared with HTTP and TCP |
| `keel_backend_healthy`, `keel_backend_drain_state` | gauge | `pool`, `backend` — shared with HTTP and TCP |

Packet and byte counters update per datagram, not at flow end, so
throughput is visible while a long flow is open.

## Behavior notes

- **Privileged ports.** When Keel starts as root on Linux, the master binds
  the UDP sockets before forking and the workers inherit them after dropping
  to `keel.user`; ports below 1024 (53, 514) need nothing further. Started
  unprivileged, each worker binds its own socket, so a port below 1024 then
  needs `CAP_NET_BIND_SERVICE` on the binary. A failed bind is logged at
  error level; the rest of the process keeps running.
- **No healthy backend.** The datagram is dropped; one log entry with
  `"error": "no_backend"` is written per dropped datagram and
  `keel_udp_errors_total` increments. There is no way to signal the failure
  to the client at L4.
- **Backend not listening.** The upstream socket receives ICMP port
  unreachable, surfaced as a receive error. The flow ends with
  `"error": "upstream_recv"` within one second, and the client's next
  datagram opens a new flow.
- **Graceful shutdown** ends every open flow immediately with
  `"error": "shutdown"`. There is no request boundary to wait for at L4.
  Use backend drain for zero-impact maintenance.
- **One socket per flow.** Each flow holds an ephemeral local port toward its
  backend, so the number of concurrent flows to one backend is bounded by
  the ephemeral port range (`net.ipv4.ip_local_port_range` on Linux, about
  28,000 by default) and by the process's file descriptor limit. Exhaustion
  shows as `upstream_bind` errors.
- **Multiple workers.** Each worker owns one socket of an `SO_REUSEPORT`
  group on the listener address (bound by the master when started as root,
  otherwise by the worker itself). On Linux the kernel assigns each client
  `ip:port` to one socket by hash, so a flow's datagrams always reach the
  same worker and the per-worker flow tables never disagree. A replacement
  worker takes over the socket of the one it replaces; datagrams hashed to
  it queue in the kernel meanwhile.
- **Datagram size.** Datagrams up to 65,535 bytes are forwarded whole;
  fragmentation and reassembly happen in the kernel on either side.
- **Hot reload** applies to UDP pools the same way as to TCP: a backend
  removed from config drains, a new backend needs a restart. Changing
  `udp_flow_timeout_seconds` or adding a UDP listener requires a restart.
