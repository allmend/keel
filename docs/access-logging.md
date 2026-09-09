# Access Logging

Keel writes one NDJSON access log file per virtual host. Each line is a complete JSON object representing a single HTTP request.

## Configuration

```yaml
access_log:
  enabled: true
  dir: /var/log/keel
```

| Field | Default | Notes |
|---|---|---|
| `enabled` | `true` | Set to `false` to disable access logging entirely |
| `dir` | `/var/log/keel` | Directory for log files; set to `-` for stdout |

Access logging is enabled by default. Set `dir: "-"` to write all access logs to stdout instead of files.

---

## File layout

Log files are created in `dir` when the first request arrives for each vhost. The filename uses the vhost hostname:

```
/var/log/keel/access_api.example.com.log
/var/log/keel/access_app.example.com.log
```

Files are named with a sortable prefix (`access_`) followed by the vhost hostname. This makes them easy to list, grep, and feed to logrotate.

Keel does not rotate log files itself. Use logrotate or a similar tool. The filenames are stable and logrotate-friendly — after rotation, Keel creates a new file on the next request.

---

## Log format

One JSON object per line (NDJSON). Example:

```json
{
  "timestamp":           "2026-04-28T12:34:56.789Z",
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

---

## Field reference

| Field | Type | Notes |
|---|---|---|
| `timestamp` | string | ISO 8601, millisecond precision, UTC |
| `method` | string | HTTP method: `GET`, `POST`, etc. |
| `uri` | string | Full path including query string |
| `protocol` | string | `HTTP/1.1`, `HTTP/2.0` |
| `status` | integer | Backend response status code; `0` if no response was sent |
| `client_addr` | string | Client IP and port (`ip:port`) |
| `vhost` | string | Matched virtual host hostname |
| `pool` | string | Backend pool that handled the request |
| `backend_addr` | string or null | Selected backend address; `null` if no backend was chosen |
| `bytes_in` | integer | Request body bytes received from client |
| `bytes_out` | integer | Response body bytes sent to client |
| `duration_ms` | float | End-to-end duration from first byte received to last byte sent |
| `backend_duration_ms` | float | Duration from backend selected to response complete |
| `user_agent` | string | Value of the `User-Agent` request header |
| `tls` | bool | Whether the client connection used TLS |
| `error` | string or null | Error code if the request failed; `null` on success |

`backend_addr` is `null` when Keel could not select a backend — for example, when the pool is empty or all backends are unhealthy.

`status` is `0` when Keel could not send a response to the client, typically due to a connection error before the response was written.

`bytes_in` counts request body bytes only. Request headers are not included.

---

## Error values

When `error` is non-null, it contains one of these short strings:

| Value | Meaning |
|---|---|
| `no_route` | No vhost matched the request's `Host` header |
| `no_backend` | Pool exists but no healthy backend was available |
| `upstream_connect` | Keel could not establish a connection to the backend |
| `upstream_timeout` | Backend did not respond within the timeout |

---

## Querying logs

Because each line is valid JSON, standard tools work well:

```bash
# Count non-2xx responses
jq 'select(.status >= 400)' /var/log/keel/access_api.example.com.log | wc -l

# Find slow requests (>500ms)
jq 'select(.duration_ms > 500)' /var/log/keel/access_api.example.com.log

# All errors
jq 'select(.error != null)' /var/log/keel/access_api.example.com.log

# Requests to a specific path
jq 'select(.uri | startswith("/api/v2/"))' /var/log/keel/access_api.example.com.log
```

---

## TCP log format

TCP listeners ([TCP proxying](tcp-proxying.md)) write one entry per **connection** to `access_tcp_<pool>.log` — L4 has no vhost or request concept, so routing and file naming are pool-based:

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
| `type` | `"tcp"` for passthrough, `"tls"` when Keel terminated TLS (`tls_mode` terminate or reencrypt) |
| `listener` | Local address that accepted the connection |
| `bytes_in` / `bytes_out` | Application bytes from/to the client over the connection lifetime (after TLS in the terminating modes) |
| `duration_ms` | Full connection lifetime, accept to close |
| `tls` | `true` when Keel terminated TLS |
| `tls_sni` | SNI the client sent; `null` when none, or in passthrough |
| `tls_version` | `TLSv1.2` or `TLSv1.3`; `null` in passthrough |
| `tls_cipher` | Cipher suite as named by rustls, for example `TLS13_AES_256_GCM_SHA384`; `null` in passthrough |
| `error` | `null`, `no_backend`, `tls_handshake`, `upstream_connect`, `upstream_tls`, `io`, or `shutdown` |

No `method`, `uri`, `status`, `user_agent`, or `vhost` — HTTP concepts with no meaning at L4. In passthrough mode the stream is opaque to Keel, so the TLS fields stay null even when the client and backend negotiate TLS inside it.

---

## UDP log format

UDP listeners ([UDP proxying](udp-proxying.md)) write one entry per **flow** — a client `ip:port` pinned to one backend until idle for `keel.udp_flow_timeout_seconds` — to `access_udp_<pool>.log`. The entry is written when the flow ends:

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
| `type` | Always `"udp"` |
| `listener` | Local address that received the datagrams |
| `backend_addr` | `null` when no backend was selected (`no_backend`) |
| `bytes_in` / `bytes_out` | Bytes from/to the client over the flow lifetime |
| `packets_in` / `packets_out` | Datagrams from/to the client — no equivalent in TCP or HTTP entries |
| `duration_ms` | Flow lifetime, first datagram to expiry. Includes the idle timeout, so a single request/response exchange shows roughly `udp_flow_timeout_seconds` |
| `error` | `null` (expired after idle timeout), `no_backend`, `upstream_bind`, `upstream_send`, `upstream_recv`, `downstream_send`, or `shutdown` |

`upstream_recv` is what a backend that is not listening looks like: the ICMP port-unreachable reply surfaces as a receive error on the flow's upstream socket. A `no_backend` entry is written per dropped datagram, since no flow exists to aggregate them.

No TLS fields: UDP carries none in v1.

---

## App logs vs access logs

Access logs (NDJSON files) cover per-request data. Application logs — startup, health check transitions, config reloads, errors — are written to stderr as structured text. These two streams are intentionally separate so they can be routed to different destinations, retention policies, and alerting pipelines.
