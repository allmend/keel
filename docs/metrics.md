# Metrics

Prometheus format at `GET /metrics` on `metrics.address` (default
`127.0.0.1:10790`; see [Configuration](configuration.md#metrics)), one
endpoint per node.

## One endpoint per node

The master binds `metrics.address` and the control worker serves it. The
workers count into one shared memory region, each into its own part, and the
endpoint adds them up: every value is a node total, and no metric has a
`worker` label.

- Counters count from the master's start (`keel_start_time_seconds`). A
  replaced worker's successor keeps counting where it left off.
- Gauges are merged across the workers: `keel_active_connections` is
  the sum, `keel_backend_ejected` is `1` if any worker ejected it, and
  `keel_backend_drain_state` is the least drained state of any worker.
- Series stay until the master restarts, including those of vhosts, routes
  and pools removed by a reload (with their last value).
- The region is sized from the config with room for reloads. When it is
  full, new series are not recorded: a warning is logged once and
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
| `vhost` | The configured vhost that served the request: the exact host, a `*.example.com` wildcard, or `"*"`; `"unmatched"` in `keel_lb_errors_total` when no vhost matched. Never the raw `Host` header |
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
| `keel_backend_connection_errors_total` | counter | `pool`, `backend` | Requests that failed on the backend's side: access-log errors `upstream_connect`, `upstream_timeout`, `upstream` |
| `keel_active_connections` | gauge | `pool`, `backend` | Current active connections — HTTP and TCP combined; the same counter drain waits on |
| `keel_backend_healthy` | gauge | `pool`, `backend` | `1` healthy, `0` unhealthy: the control worker's verdict, which every worker follows. Pools without a `health_check` never emit it |
| `keel_backend_drain_state` | gauge | `pool`, `backend` | `0` active, `1` draining, `2` removed |
| `keel_backend_ejected` | gauge | `pool`, `backend` | `1` while passively ejected after consecutive traffic failures |
| `keel_backend_ejections_total` | counter | `pool`, `backend` | Passive ejections |
| `keel_proxy_protocol_errors_total` | counter | `listener` | Connections or datagrams rejected for a missing or malformed PROXY protocol header |

## Cache metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_cache_requests_total` | counter | `pool`, `vhost`, `result` | Requests on a route with caching on: `hit` (answered from the cache) or `miss` (from the backend) |
| `keel_cache_hits_total` | counter | `tier` | Hits by the tier that served them: `memory` or `disk` |

A route's series appear with its first cached request. Hits also count in `keel_requests_total` and the access log, under their vhost and pool.

```promql
# Hit ratio per vhost
sum by (vhost) (rate(keel_cache_requests_total{result="hit"}[5m]))
  / sum by (vhost) (rate(keel_cache_requests_total[5m]))
```

## Backends per pool

`keel_pool_backends{pool, state}` — gauge, computed at each scrape from the node's view of every backend. Each backend that is not removed counts in exactly one state; the four values add up to the pool's backends. All four are emitted for every pool, zeros included.

| `state` | Backend |
|---|---|
| `draining` | Being drained (by command, or removed from the config by a reload) |
| `unhealthy` | Failing its health check |
| `ejected` | Ejected by passive detection in at least one worker |
| `available` | Gets new connections |

A backend in several states counts in the first that applies, in this order. A pool without a health check has no `unhealthy` backends. A drained backend is removed, and no longer counted, when no worker holds a connection to it.

Share of a pool's backends taking traffic, for a traffic-light panel:

```promql
keel_pool_backends{state="available"}
  / ignoring(state) sum without(state) (keel_pool_backends)
```

The same, leaving planned drains out:

```promql
keel_pool_backends{state="available"}
  / ignoring(state) sum without(state) (keel_pool_backends{state!="draining"})
```

## TCP (L4) metrics

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

Flow expiry is not an error.

## Worker health

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `keel_workers` | gauge | | Worker processes running |
| `keel_workers_configured` | gauge | | `keel.workers` |
| `keel_workers_unresponsive` | gauge | | Workers that did not answer the control worker's last liveness check (every 5s, 5s timeout) |
| `keel_worker_restarts_total` | counter | `reason` | Workers that died and are replaced: `exit` (exited on its own) or `signal` (killed, including by the OOM killer). One that died within 10s of starting is replaced after a delay (1s doubling up to 30s); `keel_workers` is lower meanwhile |
| `keel_start_time_seconds` | gauge | | When the master started, unix time |
| `keel_metrics_series_dropped_total` | counter | | Series not recorded because the shared memory region was full |

Every restart and every unresponsive worker is also logged with the worker's
index and pid.

Grafana dashboards and Prometheus alerting rules are in
[`contrib/`](../contrib/README.md).
