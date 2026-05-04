# Load Balancing

## Algorithms

Set `algorithm` on a pool. Default is `round_robin`.

```yaml
pools:
  web:
    algorithm: round_robin
    backends:
      - address: 10.0.0.1:8080
      - address: 10.0.0.2:8080
      - address: 10.0.0.3:8080
```

### round_robin

Distributes requests across all healthy backends in order. Respects weights: a backend with `weight: 2` receives twice as many requests as one with `weight: 1`.

Use for stateless services where any backend can handle any request. This is the right default for most workloads.

### random

Selects a backend at random on each request, weighted by `weight`. Produces approximately the same distribution as round-robin over time with less coordination overhead.

Use as an alternative to round-robin when strict ordering doesn't matter and you want to reduce thundering herd effects.

### least_connections

Routes each request to the backend with the fewest active connections at the time the request arrives. Respects weights.

Use for workloads where requests have variable processing time — long-lived connections or slow backends will receive fewer new requests automatically.

### consistent_hash

Hashes a property of the request (typically client IP) to select a backend. The same client always reaches the same backend as long as the pool composition doesn't change. Respects weights.

Use when backend-side caching or session affinity matters and you cannot use cookies.

---

## Weighted backends

All algorithms support per-backend weights. A backend with `weight: 2` receives twice as many requests as one with `weight: 1`.

```yaml
pools:
  web:
    algorithm: round_robin
    backends:
      - address: 10.0.0.1:8080
        weight: 1
      - address: 10.0.0.2:8080
        weight: 1
      - address: 10.0.0.3:8080
        weight: 2   # receives 50% of traffic
```

Weight defaults to `1` if omitted.

---

## Health checks

Health checks run continuously in the background. Unhealthy backends are excluded from routing until they recover.

```yaml
pools:
  web:
    health_check:
      type: http
      path: /health
      interval: 10s
      timeout: 2s
      healthy_threshold: 2
      unhealthy_threshold: 3
    backends:
      - address: 10.0.0.1:8080
```

| Field | Default | Notes |
|---|---|---|
| `type` | required | `tcp` or `http` |
| `path` | `/health` | HTTP path; ignored for `tcp` type |
| `interval` | `10s` | Time between checks |
| `timeout` | `2s` | Per-check connect and response timeout |
| `healthy_threshold` | `2` | Consecutive successes before marking healthy |
| `unhealthy_threshold` | `3` | Consecutive failures before marking unhealthy |

`type: tcp` — checks that a TCP connection can be established. Fast and protocol-agnostic. Use when the service doesn't have an HTTP health endpoint.

`type: http` — sends a `GET` request to `path` and expects a 2xx response. More meaningful than TCP because it validates that the application is responding.

Omit `health_check` entirely to disable health checks for a pool. All backends in that pool are always considered healthy.

If all backends in a pool are unhealthy, Keel returns 502 for requests routed to that pool.

---

## Backend drain

Draining removes a backend from rotation without dropping active connections. New requests stop being routed to the backend immediately; existing connections are allowed to finish.

```bash
# Begin drain (returns immediately)
keel backend drain 10.0.0.1:8080

# Drain and block until complete, streaming live connection count
keel backend drain 10.0.0.1:8080 --wait
```

With `--wait`:

```
Draining 10.0.0.1:8080 from pools: web
  connections: 14
  connections: 9
  connections: 3
  connections: 0
Drain complete (22s elapsed).
```

Drain identifies the backend across all pools — you do not specify a pool name. If the address appears in multiple pools it is drained from all of them simultaneously.

After drain completes the backend is in the `Removed` state. To bring it back, update `keel.yaml` and reload config, or in cluster mode use `keel config push`.

In cluster mode, drain is committed to the Raft log and applied on all nodes. All nodes stop sending new requests to the backend. The drain is not considered complete until all nodes report zero active connections. See [Cluster](cluster.md).

---

## Pool reference in vhosts

A pool must be defined in `pools` before it can be referenced in a vhost. Keel validates all pool references at startup and on hot reload, and will reject a config where a vhost references a nonexistent pool.

See [Virtual hosts](virtual-hosts.md) for routing configuration.
