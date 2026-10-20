# Access Logging

One NDJSON line per HTTP request, TCP connection or UDP flow, in one file per vhost or pool.

```hcl
access_log {
  enabled = true
  dir     = "/var/log/keel"
}
```

| Attribute | Default | Notes |
|---|---|---|
| `enabled` | `true` | |
| `dir` | `/var/log/keel` | Absolute directory, or `-` for stdout |

## Containers

```hcl
access_log {
  dir = "-"
}
```

stdout carries the access log, one JSON object per line; stderr carries Keel's own log, as text. A log stack tells them apart by stream, and the access entries by their `type`: `http`, `tcp`, `tls` or `udp`.

## Files

| Traffic | File |
|---|---|
| HTTP | `access_<vhost>.log`; entries with an `error` also in `error_<vhost>.log` |
| HTTP answered by Keel itself, unrouted | `access_unknown.log`, `error_unknown.log` |
| TCP | `access_tcp_<pool>.log` |
| UDP | `access_udp_<pool>.log` |

A file is created with its first entry. Each worker opens it once and keeps it open; Keel never reopens or rotates it. With logrotate, use `copytruncate` (files are opened for append).

## HTTP

```json
{
  "timestamp":           "2026-04-28T12:34:56.789Z",
  "type":                "http",
  "method":              "GET",
  "uri":                 "/api/v1/users?page=1",
  "protocol":            "HTTP/1.1",
  "status":              200,
  "client_addr":         "1.2.3.4:12345",
  "vhost":               "api.example.com",
  "pool":                "web",
  "backend_addr":        "10.0.0.1:8080",
  "bytes_in":            0,
  "bytes_out":           1842,
  "duration_ms":         12.5,
  "backend_duration_ms": 10.2,
  "user_agent":          "curl/7.79.1",
  "tls":                 false,
  "error":               null
}
```

| Field | Notes |
|---|---|
| `timestamp` | ISO 8601 UTC, milliseconds |
| `type` | `http` |
| `uri` | Path and query string |
| `protocol` | `HTTP/1.1` or `HTTP/2.0` |
| `status` | Status sent to the client; `0` if none was sent |
| `client_addr` | `ip:port`, from the PROXY protocol header when the listener expects one |
| `vhost` | The matched vhost as configured |
| `backend_addr` | `null` if no backend was selected |
| `bytes_in` | Request body bytes |
| `bytes_out` | Response body bytes |
| `duration_ms` | First byte in to last byte out |
| `backend_duration_ms` | Backend selected to response complete |
| `tls` | Client connection used TLS |
| `error` | `null` or a reason below |

| `error` | Meaning |
|---|---|
| `no_route` | No vhost or route matched, or Keel answered itself |
| `no_backend` | No available backend in the pool; Keel answered 503 |
| `denied` | An access list turned the client away; Keel answered 403 |
| `header_too_large` | Request line and headers over `keel.max_header_size`; Keel answered 431 |
| `body_too_large` | Request body over `max_body_size`; Keel answered 413 |
| `upstream_connect` | Connecting to the backend failed; Keel answered 502, or 504 when it timed out |
| `upstream_timeout` | The backend did not answer within `read_timeout` or `write_timeout`; Keel answered 504 |
| `upstream` | The backend's connection failed after it was established |
| `downstream` | The client's connection failed or closed early |
| `internal` | Any other failure |

Requests Keel answers itself — ACME HTTP-01 challenges, `redirect_http` and `default_action` — have empty `vhost`, `method` and `uri`, `error` `no_route`, and the status Keel sent.

## TCP

One entry per connection, written when it closes:

```json
{
  "timestamp":    "2026-07-06T12:52:00.812Z",
  "type":         "tls",
  "client_addr":  "203.0.113.42:57811",
  "listener":     "0.0.0.0:6379",
  "pool":         "redis",
  "backend_addr": "10.0.0.21:6379",
  "bytes_in":     6420,
  "bytes_out":    182034,
  "duration_ms":  84210.5,
  "tls":          true,
  "tls_sni":      "cache.example.com",
  "tls_version":  "TLSv1.3",
  "tls_cipher":   "TLS13_AES_256_GCM_SHA384",
  "error":        null
}
```

| Field | Notes |
|---|---|
| `type` | `tcp` in passthrough, `tls` when Keel terminates TLS |
| `listener` | Address that accepted the connection |
| `bytes_in`, `bytes_out` | Application bytes from and to the client |
| `duration_ms` | Accept to close |
| `tls_sni`, `tls_version`, `tls_cipher` | When Keel terminates TLS; `null` otherwise |
| `error` | `null`, `denied`, `idle_timeout`, `no_backend`, `proxy_protocol`, `tls_handshake`, `upstream_connect`, `upstream_tls`, `io`, `shutdown` |

## UDP

One entry per flow, written when it ends:

```json
{
  "timestamp":    "2026-07-06T12:52:00.812Z",
  "type":         "udp",
  "client_addr":  "203.0.113.42:41230",
  "listener":     "0.0.0.0:53",
  "pool":         "dns",
  "backend_addr": "10.0.0.11:53",
  "bytes_in":     64,
  "bytes_out":    128,
  "packets_in":   1,
  "packets_out":  1,
  "duration_ms":  10005.2,
  "error":        null
}
```

| Field | Notes |
|---|---|
| `backend_addr` | `null` with `no_backend` and `flow_limit` |
| `packets_in`, `packets_out` | Datagrams from and to the client |
| `duration_ms` | First datagram to expiry, so it includes `keel.udp_flow_timeout` |
| `error` | `null` (expired), `denied`, `no_backend`, `flow_limit`, `upstream_bind`, `upstream_send`, `upstream_recv` (ICMP port-unreachable from the backend), `downstream_send`, `shutdown` |

Datagrams dropped without a flow, `no_backend` and `flow_limit`, are logged once a second per listener and reason: `packets_in` and `bytes_in` are the datagrams and bytes dropped in that time, `client_addr` the last client, and `duration_ms` the time since the first drop.
