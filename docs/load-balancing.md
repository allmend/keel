# Load Balancing

## Algorithms

Set `algorithm` on a pool. Default is `round_robin`.

```hcl
pool "web" {
  algorithm = "round_robin"

  backend "10.0.0.1:8080" {}
  backend "10.0.0.2:8080" {}
  backend "10.0.0.3:8080" {}
}
```

### round_robin

Distributes requests across the available backends in turn, by weight.

Use for stateless services where any backend can handle any request.

### random

Picks a backend at random for each request, by weight. Over time the distribution matches round robin.

### least_connections

Picks the backend with the fewest active connections per unit of weight: with weights 1 and 3, the second takes new connections until it holds three times as many. Ties go to the backend listed first. Each worker counts only its own connections.

Use when requests vary in duration — long-lived connections, slow backends.

### consistent_hash

Hashes the client address to pick a backend: the client IP for HTTP, the client `IP:port` for TCP and UDP. The same key reaches the same backend while the pool is unchanged. A backend with `weight = 2` gets about twice the keys.

At L4 every TCP connection has a new source port, so affinity holds per connection (TCP) or per flow (UDP), not per client.

Use when a backend keeps per-client state, such as a local cache.

---

## Weighted backends

Every algorithm honours weights: `round_robin` exactly, `random` and `consistent_hash` on average, `least_connections` per connection.

```hcl
pool "web" {
  backend "10.0.0.1:8080" {}
  backend "10.0.0.2:8080" {}
  backend "10.0.0.3:8080" { weight = 2 } # receives 50% of traffic
}
```

Weight defaults to `1` and must be 1–1000.

---

## Health checks

A pool with a `health_check` block is probed, and unhealthy backends get no new traffic until they recover. See [Health checks](health-checks.md) for probe types, thresholds and passive ejection.

```hcl
pool "web" {
  health_check "http" {
    path     = "/health"
    interval = "10s"
  }

  backend "10.0.0.1:8080" {}
}
```

When no backend of a pool is available (unhealthy, ejected or drained), requests to it get 503. A backend that is selected but cannot be reached gives 502.

---

## Backend drain

Draining takes a backend out of rotation without dropping its connections: new requests stop at once, open ones finish.

```bash
# Begin drain (returns immediately)
keel backend drain 10.0.0.1:8080

# Drain and wait until no connection is left
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

The address is drained in every pool that has it; a bare IP drains every backend on it. `keel backend activate <address>` returns it to service. A drain is committed through Raft: it applies on every node and stays across reloads, pushes and restarts. See [Cluster](cluster.md#drain).
