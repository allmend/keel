# TCP Proxying

A `tcp` listener proxies TCP connections to a pool. No vhosts or routes apply. Algorithms, weights, health checks, drain and connection counts work as for HTTP.

```hcl
listener "tcp" "0.0.0.0:5432" {
  pool = "postgres"
}
```

| Attribute | Notes |
|---|---|
| `pool` | Required |
| `tls_mode` | `passthrough` (default), `terminate`, `reencrypt` |
| `tls_host` | `terminate`, `reencrypt`: host of a `certificate` block or a vhost with `tls`, served when the client's SNI has no certificate |
| `tls_verify` | `reencrypt`: verify the backend certificate against the system roots plus `tls_ca` and the backend's hostname. Default `false` |
| `tls_ca` | `reencrypt` with `tls_verify`: extra trusted CAs, PEM |
| `proxy_protocol` | Read a PROXY protocol header first, in every mode. See [Virtual hosts](virtual-hosts.md#proxy-protocol) |

## TLS modes

| Mode | Client ↔ Keel | Keel ↔ backend | Certificate |
|---|---|---|---|
| `passthrough` | bytes as sent | the same bytes | the backend's |
| `terminate` | TLS | plaintext | Keel's |
| `reencrypt` | TLS | new TLS connection | Keel's, and the backend's |

In `passthrough` Keel does not read the stream. In `terminate` and `reencrypt` the certificate is picked by SNI, else `tls_host`. `reencrypt` sends the backend's configured hostname as SNI (`backend "cache1.internal:6379"` sends `cache1.internal`; an IP sends none), and without `tls_verify` accepts any backend certificate.

Protocols that start TLS at byte zero (Redis `tls-port`, LDAPS, MQTT over TLS) work in every mode. Protocols that upgrade mid-stream (PostgreSQL, SMTP) work in `passthrough` only.

`tls_ca` may hold a self-signed backend certificate only if it is not a CA (`CA:FALSE` or no `basicConstraints`). One with `CA:TRUE` fails with `upstream_tls` and `CaUsedAsEndEntity`.

## PostgreSQL (passthrough)

```hcl
listener "tcp" "0.0.0.0:5432" {
  pool = "postgres"
}

pool "postgres" {
  health_check "tcp" {}

  backend "10.0.0.11:5432" {}
  backend "10.0.0.12:5432" {}
}
```

```bash
psql "host=db.example.com port=5432 sslmode=verify-full"
```

The client verifies the database's own certificate. A [`certificate` block](acme.md#certificates-without-a-vhost) can issue it.

## Redis (terminate)

```hcl
listener "tcp" "0.0.0.0:6379" {
  pool     = "redis"
  tls_mode = "terminate"
  tls_host = "cache.example.com"
}

certificate "cache.example.com" {}

pool "redis" {
  algorithm = "least_connections"

  health_check "tcp" {
    interval = "5s"
    timeout  = "1s"
  }

  backend "10.0.0.21:6379" {}
  backend "10.0.0.22:6379" {}
}
```

```bash
redis-cli --tls --sni cache.example.com -h cache.example.com PING
```

Keel serves an ACME certificate for `cache.example.com`; Redis runs without TLS.

## LDAPS (reencrypt)

```hcl
listener "tcp" "0.0.0.0:636" {
  pool       = "ldap"
  tls_mode   = "reencrypt"
  tls_host   = "ldap.example.com"
  tls_verify = true
  tls_ca     = "/etc/keel/internal-ca.pem"
}

certificate "ldap.example.com" {
  cert = "/etc/keel/certs/ldap.example.com.crt"
  key  = "/etc/keel/certs/ldap.example.com.key"
}

pool "ldap" {
  health_check "tls" {
    sni       = "ldap1.internal"
    min_valid = "14d"
  }

  backend "ldap1.internal:636" {}
  backend "ldap2.internal:636" {}
}
```

```bash
ldapsearch -H ldaps://ldap.example.com -x -b dc=example,dc=com
```

Clients get `ldap.example.com`; Keel checks each backend's certificate against the internal CA.

## Behavior

- `consistent_hash` hashes the client `IP:port`: one connection stays on one backend, the next may not.
- A drained backend gets no new connections; open ones run until either side closes them. `keel backend drain --wait` shows the count.
- No available backend: the connection is closed, logged as `no_backend`.
- Shutdown closes open connections.
- A failed client handshake in `terminate` or `reencrypt` opens no backend connection and is logged as `tls_handshake`. A close without TLS `close_notify` is a normal end.
- Renewed certificates are served from the next handshake on.
- One log entry per connection in `access_tcp_<pool>.log`; see [Access logging](access-logging.md#tcp). Metrics: `keel_tcp_*`, see [Metrics](metrics.md#tcp-l4-metrics).
