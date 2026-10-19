# UDP Proxying

A `udp` listener forwards datagrams to a pool. No vhosts or routes apply. Algorithms, weights, health checks, drain and connection counts work as for HTTP and TCP. Payloads are forwarded as they are.

UDP has no connection, so Keel keeps a **flow** per client `ip:port`: the first datagram picks a backend, later ones go to the same backend, and replies go back to the client. A flow ends after `keel.udp_flow_timeout` (default `30s`) without traffic; then its backend is released and its log entry written.

```hcl
listener "udp" "0.0.0.0:53" {
  pool = "resolvers"
}
```

| Listener attribute | Notes |
|---|---|
| `pool` | Required |
| `proxy_protocol` | Every datagram starts with a PROXY protocol v2 header naming the client; datagrams without one are dropped |

| `keel` attribute | Default | Notes |
|---|---|---|
| `udp_flow_timeout` | `30s` | Idle time before a flow ends; every UDP listener |
| `udp_max_flows` | `8192` | Flows per UDP listener at once. Datagrams from further clients are dropped and counted as `flow_limit` until a flow ends |

## DNS

UDP and TCP on one port, one pool:

```hcl
keel {
  udp_flow_timeout = "10s"
}

listener "udp" "0.0.0.0:53" {
  pool = "resolvers"
}

listener "tcp" "0.0.0.0:53" {
  pool = "resolvers"
}

pool "resolvers" {
  health_check "dns" {
    query    = "example.org"
    interval = "5s"
    timeout  = "1s"
  }

  backend "10.0.0.11:53" {}
  backend "10.0.0.12:53" {}
}
```

```bash
dig @dns.example.com example.org A
dig @dns.example.com +tcp example.org AXFR
```

A stub resolver sends each query from a new source port: one flow per query.

## Syslog

One datagram per message, no reply: `bytes_out` and `packets_out` stay 0.

```hcl
listener "udp" "0.0.0.0:514" {
  pool = "syslog"
}

pool "syslog" {
  algorithm = "consistent_hash"

  health_check "udp" {}

  backend "10.0.0.21:514" {}
  backend "10.0.0.22:514" {}
}
```

`consistent_hash` keeps each sender on one collector while the pool is unchanged.

## Affinity

Every datagram of a flow reaches the flow's backend. Across flows, `consistent_hash` hashes the client `ip:port`; the other algorithms pick per flow, and `least_connections` counts open flows.

## Health

`dns` and `ntp` probe the service; `udp` only detects a closed port. A backend that does not listen answers with ICMP port-unreachable: the flow ends within a second as `upstream_recv`, and the client's next datagram opens a new flow.

## Behavior

- A drained backend gets no new flows; open flows run until they expire, so a client that keeps sending keeps the drain open.
- No available backend: the datagram is dropped, one `no_backend` log entry per datagram.
- Shutdown ends every flow as `shutdown`.
- Each flow holds its own upstream socket and local port. The ephemeral port range and the descriptor limit bound the flows to one backend; running out shows as `upstream_bind`.
- On Linux the master binds one `SO_REUSEPORT` socket per worker; the kernel sends each client `ip:port` to one socket, so a flow stays on one worker. A replacement worker takes over its predecessor's socket. Elsewhere each worker binds its own.
- Datagrams up to 65,535 bytes are forwarded whole.
- Packet and byte counters update per datagram. One log entry per flow in `access_udp_<pool>.log`; see [Access logging](access-logging.md#udp). Metrics: `keel_udp_*`, see [Metrics](metrics.md#udp-l4-metrics).
