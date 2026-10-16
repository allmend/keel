# Metrics Reference

Keel exposes Prometheus-format metrics at `GET /metrics` on
`metrics.address` (default `127.0.0.1:10790`; see
[Configuration](configuration.md#metrics)). Each node exposes its own
metrics; there is no cluster aggregation — use your Prometheus setup's
federation if needed.

## One endpoint per node

The master binds `metrics.address` and the control worker serves it. The
workers count into one shared memory region, each into its own part, and the
endpoint adds them up: every value is a node total, and no metric has a
`worker` label.

- **Counters count from the master's start** (`keel_start_time_seconds`).
  Restarting Keel resets them to zero, which `rate()` and `increase()`
  handle as any exporter restart. A worker that crashes or is replaced does
  not: its replacement keeps counting where it left off.
- **Gauges** are merged across the workers: `keel_active_connections` is
  the sum, `keel_backend_healthy` is `0` if any worker sees the backend
  unhealthy, `keel_backend_ejected` is `1` if any worker ejected it, and
  `keel_backend_drain_state` is the least drained state of any worker.
- **Series stay** until the master restarts, including those of vhosts,
  routes and pools removed by a reload (with their last value).
- The region is sized from the config with room for reloads. If it fills
  up, new series are not recorded: a warning is logged once and
  `keel_metrics_series_dropped_total` counts them.

Series of routes (`keel_request_duration_seconds`, `keel_request_bytes_*`)
and backends (`keel_active_connections`, `keel_backend_drain_state`,
`keel_backend_ejected`, the backend histogram) exist from the start; `keel_requests_total` per status
code once a request gets that status, `keel_tcp_*` once the first L4
connection arrives, `keel_udp_*` once the first flow opens.

## Labels

| Label | Values |
|---|---|
| `pool` | Configured pool name |
| `backend` | Backend `address` as configured (`ip:port`) |
| `vhost` | The configured vhost whose route served the request: the exact host, a `*.example.com` wildcard, or `"*"`; `"unmatched"` in `keel_lb_errors_total` when no vhost matched. Never the raw `Host` header — cardinality stays bounded by config |
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
| `keel_backend_ejected` | gauge | `pool`, `backend` | `1` while passively ejected after consecutive traffic failures |
| `keel_backend_ejections_total` | counter | `pool`, `backend` | Passive ejections |
| `keel_proxy_protocol_errors_total` | counter | `listener` | Connections or datagrams on a `proxy_protocol` listener rejected for a missing or malformed header |

## TCP (L4) metrics

One connection is one unit — there is no request concept at L4.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_tcp_connections_total` | counter | `pool`, `backend` | Connections accepted and spliced |
| `keel_tcp_bytes_in_total` | counter | `pool`, `backend` | Bytes received from clients over connection lifetimes |
| `keel_tcp_bytes_out_total` | counter | `pool`, `backend` | Bytes sent to clients over connection lifetimes |
| `keel_tcp_errors_total` | counter | `pool`, `reason` | Connections ending in error. Reasons: `no_backend`, `proxy_protocol`, `tls_handshake`, `upstream_connect`, `upstream_tls`, `io`, `shutdown` |

## UDP (L4) metrics

One flow (client `ip:port` → backend, until idle timeout) is the unit for `keel_udp_flows_total` and `keel_active_connections`; packet and byte counters update per datagram while the flow is open.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_udp_flows_total` | counter | `pool`, `backend` | Flows opened |
| `keel_udp_packets_in_total` | counter | `pool`, `backend` | Datagrams received from clients |
| `keel_udp_packets_out_total` | counter | `pool`, `backend` | Datagrams sent to clients |
| `keel_udp_bytes_in_total` | counter | `pool`, `backend` | Bytes received from clients |
| `keel_udp_bytes_out_total` | counter | `pool`, `backend` | Bytes sent to clients |
| `keel_udp_errors_total` | counter | `pool`, `reason` | Dropped datagrams and flows ended by error. Reasons: `no_backend` (per dropped datagram), `flow_limit` (per dropped datagram), `proxy_protocol`, `upstream_bind`, `upstream_send`, `upstream_recv` (backend not listening — ICMP unreachable), `downstream_send`, `shutdown` |

Normal flow expiry is not an error and is not counted here; `keel_udp_flows_total` minus `keel_active_connections` gives flows that have ended.

## Worker health

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_workers` | gauge | | Worker processes running |
| `keel_workers_configured` | gauge | | `keel.workers` |
| `keel_workers_unresponsive` | gauge | | Workers that did not answer the control worker's last liveness check (every 5s, 5s timeout) |
| `keel_worker_restarts_total` | counter | `reason` | Workers the master replaced: `exit` (exited on its own) or `signal` (killed, including by the OOM killer) |
| `keel_start_time_seconds` | gauge | | When the master started, unix time; uptime is `time() - keel_start_time_seconds` |
| `keel_metrics_series_dropped_total` | counter | | Series not recorded because the shared memory region was full |

Every restart and every unresponsive worker is also logged with the worker's
index and pid.

```promql
# A worker crashed or was killed
increase(keel_worker_restarts_total[10m]) > 0

# A worker is hung (alert after a minute: a restarting worker misses a check)
keel_workers_unresponsive > 0

# Fewer workers than configured
keel_workers < keel_workers_configured
```

Ready-made Grafana dashboards and Prometheus alerting rules are in
[`contrib/`](../contrib/README.md).

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

# UDP datagram rate per backend, and flows dropped for lack of a backend
sum by (pool, backend) (rate(keel_udp_packets_in_total[5m]))
rate(keel_udp_errors_total{reason="no_backend"}[5m])

# Backends stuck draining (connections still open)
keel_backend_drain_state == 1 and keel_active_connections > 0
```
