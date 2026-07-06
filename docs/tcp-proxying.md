# TCP Proxying (L4)

A listener with `tcp_pool` proxies raw TCP to a backend pool. Keel splices bytes between the client and the selected backend without inspecting the stream — this is **passthrough** mode. If the client and backend speak TLS, the handshake and certificates are theirs; encryption is end-to-end and Keel never holds a key.

`terminate` (Keel holds the certificate, forwards plaintext) and `reencrypt` (Keel terminates and opens its own TLS to the backend) are planned.

## Configuration

```yaml
listeners:
  - address: 0.0.0.0:5432
    tcp_pool: postgres      # L4 passthrough to this pool

pools:
  postgres:
    algorithm: round_robin
    health_check:
      type: tcp
      interval: 10s
      timeout: 2s
    backends:
      - address: 10.0.0.11:5432
      - address: 10.0.0.12:5432
      - address: 10.0.0.13:5432
```

A `tcp_pool` listener is L4-only: it ignores vhosts and routes, and cannot combine with `tls: true` (passthrough never terminates). The pool is a regular pool — algorithms, weights, health checks, and drain all behave exactly as for HTTP.

## Example: PostgreSQL

Postgres negotiates TLS inside its own protocol (a plaintext `SSLRequest` exchange before the handshake), so a protocol-agnostic proxy cannot terminate it — passthrough is the correct mode. Each database server presents its own certificate; clients verify it as if they were connected directly:

```yaml
listeners:
  - address: 0.0.0.0:5432
    tcp_pool: postgres
```

```bash
psql "host=db.example.com port=5432 sslmode=verify-full"
```

The backend certificates can be provisioned by Keel itself via the [`certificates:` section](acme.md#certificates-for-tcp--tls-passthrough-backends) — Keel answers the ACME HTTP-01 challenge for `db.example.com` and writes the cert files for the database servers to load.

## Example: Redis

Redis with TLS enabled (`tls-port`) does TLS-on-connect, which passes through the same way — client and Redis handshake directly:

```yaml
listeners:
  - address: 0.0.0.0:6379
    tcp_pool: redis

pools:
  redis:
    algorithm: least_connections
    backends:
      - address: 10.0.0.21:6379
      - address: 10.0.0.22:6379
```

## Load balancing and session affinity

All pool algorithms work at L4. With `consistent_hash`, the hash key is the client `IP:port`, so a client keeps reaching the same backend while the pool composition is stable.

Connections are counted per backend, identically to HTTP:

```bash
keel status
keel backend drain 10.0.0.11:5432 --wait
```

Draining a backend stops new TCP connections immediately; existing connections run until the client or backend closes them, and the backend moves to `removed` when the last one ends. Note that long-lived connections (database sessions, replication streams) hold the drain open until they disconnect.

## Access logging

One NDJSON entry per connection in `access_tcp_<pool>.log` — see [Access logging](access-logging.md#tcp-log-format).

## Behavior notes

- **Health checks** use the pool's configured check (`tcp` connect probe is the natural fit). A pool with no healthy backends refuses new connections; the log entry records `"error": "no_backend"`.
- **Graceful shutdown closes L4 connections.** There is no request boundary to wait for at L4, so on shutdown the splice is closed. Use backend drain for zero-impact maintenance.
- **No TLS fields in the log** for passthrough: the stream is opaque, so Keel cannot know whether TLS was negotiated inside it.
