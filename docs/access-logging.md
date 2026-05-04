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

## App logs vs access logs

Access logs (NDJSON files) cover per-request data. Application logs — startup, health check transitions, config reloads, errors — are written to stderr as structured text. These two streams are intentionally separate so they can be routed to different destinations, retention policies, and alerting pipelines.
