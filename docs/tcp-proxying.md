# TCP Proxying (L4)

A listener with `tcp_pool` proxies raw TCP to a backend pool. Routing is
listener → pool: L4 has no Host header, so vhosts and routes do not apply.
The pool is an ordinary pool — algorithms, weights, health checks, drain, and
connection counting behave the same as for HTTP.

## TLS handling — three modes

`tls_mode` on the listener selects one:

| | Client ↔ Keel | Keel ↔ backend | Certificate lives at |
|---|---|---|---|
| `passthrough` (default) | opaque bytes (TLS end-to-end if used) | same stream, untouched | the backend |
| `terminate` | TLS, terminated by Keel | plaintext TCP | Keel |
| `reencrypt` | TLS, terminated by Keel | new TLS connection | Keel (client side), backend (upstream side) |

In passthrough Keel splices bytes without inspecting the stream: if the
client and backend speak TLS, the handshake, certificate, and verification
are between them, and Keel holds no key. In the other two modes Keel serves
a certificate from its store, selected by the client's SNI and falling back
to the listener's `tls_host`; the certificate comes from a `certificates:`
entry (ACME-issued or bring-your-own files) or from a vhost with `tls`.
Re-encryption does not verify the backend certificate unless
`tls_verify: true`; by default it provides wire encryption, not backend
authentication, the same trade-off a cloud NLB makes.

Which mode a protocol can use depends on where its TLS handshake starts:

- TLS-on-connect protocols (Redis `tls-port`, LDAPS, MQTT-over-TLS, HTTPS)
  put the handshake at byte zero. All three modes apply.
- STARTTLS-style protocols (PostgreSQL, SMTP) begin in plaintext and upgrade
  mid-stream. A protocol-agnostic proxy cannot terminate these — passthrough
  is the only mode that applies.

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
| `tcp_pool` | Pool to proxy to. Must exist in `pools`. Makes the listener L4-only: vhosts and routes are ignored |
| `tls_mode` | `passthrough` (default), `terminate`, or `reencrypt` |
| `tls_host` | Required with `terminate` and `reencrypt`: host of a `certificates:` entry or of a vhost with `tls`. Served when the client sends no SNI or an SNI without a certificate of its own |
| `tls_verify` | `reencrypt` only, default `false`: verify the backend certificate against the system trust store (plus `tls_ca`) and the backend's configured hostname |
| `tls_ca` | `reencrypt` with `tls_verify`: PEM bundle of additional trusted CAs, for backends with an internal CA |
| `tls` | Rejected together with `tcp_pool` — that field is HTTP termination; use `tls_mode` |
| `proxy_protocol` | Reserved; `true` is a startup error until parsing is implemented |

Config validation fails at startup for an unknown `tcp_pool`, a
`tcp_pool` + `tls: true` combination, `terminate`/`reencrypt` without a
`tls_host` that names a known certificate, or `tls_verify`/`tls_ca` outside
`reencrypt`.

When re-encrypting, Keel sends the backend's configured hostname as SNI
(`address: cache1.internal:6379` sends `cache1.internal`); a backend given
by IP receives no SNI. With `tls_verify`, that hostname is also what the
backend certificate must match.

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

`sslmode=verify-full` works the same as it would against the database
directly — the client sees the backend's certificate, not Keel's.

The backend certificates can be provisioned by Keel itself: the
[`certificates:` section](acme.md#certificates-for-tcp--tls-passthrough-backends)
makes Keel answer the ACME HTTP-01 challenge for `db.example.com` on port 80
and write `db.example.com.crt`/`.key` to disk for the database servers to
load. This produces publicly valid backend certificates that renew
automatically, while the proxy itself never terminates their TLS.

## Example: Redis (terminate)

Redis clients speak TLS from byte zero, so Keel can hold the certificate
and talk plaintext to Redis on an internal network. The certificate is an
ACME entry; Keel answers the HTTP-01 challenge on port 80 and serves the
issued certificate on the TCP listener:

```yaml
listeners:
  - address: 0.0.0.0:6379
    tcp_pool: redis
    tls_mode: terminate
    tls_host: cache.example.com

certificates:
  - host: cache.example.com

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

```bash
redis-cli --tls --sni cache.example.com -h cache.example.com PING
```

`least_connections` suits long-lived client connections: a backend holding
many open connections receives fewer new ones. The Redis servers run without
TLS; the client sees a publicly valid certificate and Keel renews it.

## Example: LDAPS (reencrypt)

An OpenLDAP server with an internal CA keeps its own certificate, and Keel
presents a public one to clients. `tls_verify` with the internal CA makes
Keel check the backend certificate against the backend's configured name:

```yaml
listeners:
  - address: 0.0.0.0:636
    tcp_pool: ldap
    tls_mode: reencrypt
    tls_host: ldap.example.com
    tls_verify: true
    tls_ca: /etc/keel/internal-ca.pem

certificates:
  - host: ldap.example.com
    cert: /etc/keel/certs/ldap.example.com.crt   # bring-your-own
    key: /etc/keel/certs/ldap.example.com.key

pools:
  ldap:
    health_check:
      type: tls
      sni: ldap1.internal
      min_days_valid: 14
    backends:
      - address: ldap1.internal:636
      - address: ldap2.internal:636
```

```bash
ldapsearch -H ldaps://ldap.example.com -x -b dc=example,dc=com
```

Without `tls_verify`, the connection to the backend is still encrypted but
any certificate is accepted; use that on networks where the backend's
identity is established otherwise.

`tls_ca` holds the CA that signed the backend certificates. A self-signed
backend certificate can be listed directly only if it is not marked as a CA
(`basicConstraints` absent or `CA:FALSE`); a self-signed certificate with
`CA:TRUE`, the OpenSSL `req -x509` default, is rejected by the verifier when
it appears as the end entity, with `upstream_tls` and the reason
`CaUsedAsEndEntity` in the log.

Plain TCP without any TLS proxies the same way in passthrough — `tcp_pool`
makes no assumption that the stream contains TLS.

## Session affinity

With `algorithm: consistent_hash`, the hash key is the client `IP:port`, so a
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
error values in [Access logging](access-logging.md#tcp-log-format). In
`terminate` and `reencrypt` the entry has `"type": "tls"` and carries the
negotiated SNI, protocol version, and cipher suite.

Metrics (see the [metrics reference](metrics.md) for the full list):

| Metric | Type | Labels |
|---|---|---|
| `keel_tcp_connections_total` | counter | `pool`, `backend` |
| `keel_tcp_bytes_in_total` / `keel_tcp_bytes_out_total` | counter | `pool`, `backend` |
| `keel_tcp_errors_total` | counter | `pool`, `reason` (`no_backend`, `tls_handshake`, `upstream_connect`, `upstream_tls`, `io`, `shutdown`) |
| `keel_active_connections` | gauge | `pool`, `backend` — shared with HTTP |
| `keel_backend_healthy`, `keel_backend_drain_state` | gauge | `pool`, `backend` — shared with HTTP |

## Behavior notes

- No healthy backend — the connection is closed immediately; the log entry
  records `"error": "no_backend"` and `keel_tcp_errors_total` increments.
- Graceful shutdown closes L4 connections. There is no request boundary to
  wait for at L4, so on shutdown the splice is closed. Use backend drain for
  zero-impact maintenance.
- No TLS fields in logs for passthrough: the stream is opaque, so Keel
  cannot know whether TLS was negotiated inside it. Keel does not peek at
  the ClientHello in passthrough; server-first protocols (SMTP, MySQL) would
  stall on a peek.
- A client that fails the TLS handshake on a `terminate` or `reencrypt`
  listener never causes a backend connection; the entry records
  `"error": "tls_handshake"`. A client that closes the TCP connection
  without a TLS close_notify is a normal end, not an error.
- Certificate renewals reach TCP listeners the same way as HTTP listeners:
  the next handshake serves the new certificate, no restart.
