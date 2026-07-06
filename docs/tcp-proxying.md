# TCP Proxying (L4)

A listener with `tcp_pool` proxies raw TCP to a backend pool. Routing is
listener → pool: L4 has no Host header, so vhosts and routes do not apply.
The pool itself is an ordinary pool — algorithms, weights, health checks,
drain, and connection counting behave exactly as for HTTP.

## TLS handling — three modes

| | Client ↔ Keel | Keel ↔ backend | Certificate lives at | Status |
|---|---|---|---|---|
| **passthrough** | opaque bytes (TLS end-to-end if used) | same stream, untouched | the backend | **implemented** |
| **terminate** | TLS, terminated by Keel | plaintext TCP | Keel | planned |
| **reencrypt** | TLS, terminated by Keel | new TLS connection | Keel (client side), backend (upstream side) | planned |

**Passthrough is the only implemented mode**, and it is the default and the
right choice for databases. Keel splices bytes without inspecting the stream:
if the client and backend speak TLS, the handshake, certificate, and
verification are theirs — Keel never holds a key. Because there is only one
mode, the listener has no mode field yet; one arrives together with
`terminate`.

Which mode a protocol *can* use is determined by where its TLS handshake
starts:

- **TLS-on-connect** protocols (Redis `tls-port`, LDAPS, MQTT-over-TLS) put
  the handshake at byte zero. Passthrough works today; `terminate` will work
  for them when it lands.
- **STARTTLS-style** protocols (PostgreSQL, SMTP) begin in plaintext and
  upgrade mid-stream. A protocol-agnostic proxy cannot terminate these —
  passthrough is the only mode that will ever apply.

## Configuration

```yaml
listeners:
  - address: 0.0.0.0:5432
    tcp_pool: postgres      # this listener is L4; splice to this pool

pools:
  postgres:
    algorithm: round_robin
    health_check:
      type: tcp             # TCP connect probe — the natural fit at L4
      interval: 10s
      timeout: 2s
    backends:
      - address: 10.0.0.11:5432
      - address: 10.0.0.12:5432
      - address: 10.0.0.13:5432
```

| Listener field | Notes |
|---|---|
| `tcp_pool` | Pool to splice to. Must exist in `pools`. Makes the listener L4-only: vhosts and routes are ignored |
| `tls` | Rejected together with `tcp_pool` — passthrough never terminates |
| `proxy_protocol` | Accepted in config; PROXY protocol parsing is a separate roadmap item |

Config validation fails at startup for an unknown `tcp_pool` or a
`tcp_pool` + `tls: true` combination.

## Example: PostgreSQL

Postgres negotiates TLS inside its own protocol: the client connects in
plaintext, sends an `SSLRequest` message, and only then does the TLS
handshake start. Keel forwards all of it as opaque bytes. Each database
server presents its own certificate and clients verify it end-to-end:

```yaml
listeners:
  - address: 0.0.0.0:5432
    tcp_pool: postgres
```

```bash
psql "host=db.example.com port=5432 sslmode=verify-full"
```

`sslmode=verify-full` works exactly as it would against the database
directly — the client sees the backend's certificate, not Keel's.

The backend certificates can be provisioned by Keel itself: the
[`certificates:` section](acme.md#certificates-for-tcp--tls-passthrough-backends)
makes Keel answer the ACME HTTP-01 challenge for `db.example.com` on port 80
and write `db.example.com.crt`/`.key` to disk for the database servers to
load. The result is publicly valid backend certificates with zero manual
renewal, behind a passthrough proxy that never touches them.

## Example: Redis

Redis with TLS enabled (`tls-port`) does TLS-on-connect. `least_connections`
suits long-lived client connections — a backend holding many open
connections receives fewer new ones:

```yaml
listeners:
  - address: 0.0.0.0:6379
    tcp_pool: redis

pools:
  redis:
    algorithm: least_connections
    health_check:
      type: tcp
      interval: 5s
      timeout: 1s
    backends:
      - address: 10.0.0.21:6379
      - address: 10.0.0.22:6379
```

Clients configure their own TLS (`redis-cli --tls --cacert …`) and verify
the backend's certificate through Keel unchanged. When `terminate` mode
lands, Redis is the kind of service that can move to it — TLS at byte zero
means Keel *could* hold the certificate instead; today both examples run
passthrough and differ in balancing, not TLS handling.

Plain TCP without any TLS proxies the same way — `tcp_pool` makes no
assumption that the stream contains TLS at all.

## Session affinity

With `algorithm: consistent_hash`, the hash key is the client `IP:port` — a
client keeps reaching the same backend while the pool composition is stable.
Round robin, random, and least-connections apply per connection.

## Drain

TCP connections register in the same per-backend counters as HTTP requests:

```bash
keel status                                   # shows TCP connections per backend
keel backend drain 10.0.0.11:5432 --wait      # works remotely via keelctl too
```

Draining stops new TCP connections to the backend immediately; existing
connections run until the client or backend closes them, and the backend
moves to `removed` when the last one ends. Long-lived connections (database
sessions, replication streams) hold the drain open until they disconnect —
`--wait` shows the live count.

## Observability

One NDJSON entry per connection in `access_tcp_<pool>.log` — fields and
error values in [Access logging](access-logging.md#tcp-log-format).

Metrics (see the [metrics reference](metrics.md) for the full list):

| Metric | Type | Labels |
|---|---|---|
| `keel_tcp_connections_total` | counter | `pool`, `backend` |
| `keel_tcp_bytes_in_total` / `keel_tcp_bytes_out_total` | counter | `pool`, `backend` |
| `keel_tcp_errors_total` | counter | `pool`, `reason` (`no_backend`, `upstream_connect`, `io`, `shutdown`) |
| `keel_active_connections` | gauge | `pool`, `backend` — shared with HTTP |
| `keel_backend_healthy`, `keel_backend_drain_state` | gauge | `pool`, `backend` — shared with HTTP |

## Behavior notes

- **No healthy backend** — the connection is closed immediately; the log
  entry records `"error": "no_backend"` and `keel_tcp_errors_total`
  increments.
- **Graceful shutdown closes L4 connections.** There is no request boundary
  to wait for at L4, so on shutdown the splice is closed. Use backend drain
  for zero-impact maintenance.
- **No TLS fields in logs or metrics** for passthrough: the stream is opaque,
  so Keel cannot know whether TLS was negotiated inside it. SNI, cipher, and
  version fields arrive with the TLS-aware modes.
