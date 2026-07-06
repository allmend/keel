# Metrics Reference

Keel exposes Prometheus-format metrics at `GET /metrics` on
`metrics.address` (default `127.0.0.1:9090`; see
[Configuration](configuration.md#metrics)). Each node exposes its own
metrics; there is no cluster aggregation — use your Prometheus setup's
federation if needed.

A metric series appears after its first event: `keel_tcp_*` series exist
once the first L4 connection arrives, `keel_requests_total` once the first
HTTP request does, and so on.

## Labels

| Label | Values |
|---|---|
| `pool` | Configured pool name |
| `backend` | Backend `address` as configured (`ip:port`) |
| `vhost` | The configured vhost label serving the request: the exact host, `"*"` for wildcard matches, or `"unmatched"`. Never the raw `Host` header — cardinality stays bounded by config |
| `status` | HTTP status code of the response |
| `reason` | Short error cause, per metric below |

## HTTP request metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_requests_total` | counter | `pool`, `vhost`, `status` | Requests proxied |
| `keel_request_duration_seconds` | histogram | `pool`, `vhost` | End-to-end duration, first byte in to last byte out |
| `keel_request_bytes_in_total` | counter | `pool`, `vhost` | Request body bytes from clients |
| `keel_request_bytes_out_total` | counter | `pool`, `vhost` | Response body bytes to clients |
| `keel_lb_errors_total` | counter | `pool`, `vhost`, `reason` | Errors before a backend was reached. Reasons: `no_route` (no vhost matched; `pool` is empty), `no_backend` (pool has no usable backend) |

Histogram buckets: 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s.

## Backend metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_backend_requests_total` | counter | `pool`, `backend`, `status` | HTTP requests forwarded per backend |
| `keel_backend_response_duration_seconds` | histogram | `pool`, `backend` | Backend selected to response complete |
| `keel_backend_connection_errors_total` | counter | `pool`, `backend` | Failed upstream connections |
| `keel_active_connections` | gauge | `pool`, `backend` | Current active connections — HTTP and TCP combined; the same counter drain waits on |
| `keel_backend_healthy` | gauge | `pool`, `backend` | `1` healthy, `0` unhealthy. Set on health-check transitions; pools without a `health_check` never emit it |
| `keel_backend_drain_state` | gauge | `pool`, `backend` | `0` active, `1` draining, `2` removed |

## TCP (L4) metrics

One connection is one unit — there is no request concept at L4.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_tcp_connections_total` | counter | `pool`, `backend` | Connections accepted and spliced |
| `keel_tcp_bytes_in_total` | counter | `pool`, `backend` | Bytes received from clients over connection lifetimes |
| `keel_tcp_bytes_out_total` | counter | `pool`, `backend` | Bytes sent to clients over connection lifetimes |
| `keel_tcp_errors_total` | counter | `pool`, `reason` | Connections ending in error. Reasons: `no_backend`, `upstream_connect`, `io`, `shutdown` |

## Useful queries

```promql
# Request rate per vhost
sum by (vhost) (rate(keel_requests_total[5m]))

# p99 request latency per pool
histogram_quantile(0.99, sum by (pool, le) (rate(keel_request_duration_seconds_bucket[5m])))

# Non-2xx ratio
sum(rate(keel_requests_total{status!~"2.."}[5m])) / sum(rate(keel_requests_total[5m]))

# Unhealthy backends right now
keel_backend_healthy == 0

# TCP throughput per pool
sum by (pool) (rate(keel_tcp_bytes_out_total[5m]))

# Backends stuck draining (connections still open)
keel_backend_drain_state == 1 and keel_active_connections > 0
```
