# Dashboards and alerts

Starting points for monitoring Keel with Prometheus and Grafana. Every
metric they use is described in [docs/metrics.md](../docs/metrics.md).

## Grafana dashboards

| File | Shows |
|---|---|
| `grafana/keel-overview.json` | Requests, 5xx ratio, p99, connections, available backends, throughput, and the process: uptime, workers, restarts |
| `grafana/keel-http.json` | Per vhost and pool: requests by status class and code, 4xx/5xx ratios, latency quantiles and distribution, bytes, errors before a backend |
| `grafana/keel-backends.json` | Per backend: drain, health and ejection as a table and over time, connections, requests, p95, 5xx, connection errors, ejections |
| `grafana/keel-l4.json` | TCP connections, bytes and errors; UDP flows, datagrams, bytes and errors |

Import them in Grafana (Dashboards → New → Import), or provision the
directory. Each dashboard asks for a Prometheus data source and filters by
`job` and `instance`, plus pool, vhost or backend where it applies; the
dashboards link to each other and keep the selection.

## Prometheus alerting rules

`prometheus/keel-alerts.yaml`, loaded with `rule_files:`:

| Alert | Severity | Fires when |
|---|---|---|
| `KeelDown` | critical | The metrics endpoint cannot be scraped for 1m |
| `KeelWorkerUnresponsive` | critical | A worker misses liveness checks for 1m |
| `KeelWorkersMissing` | critical | Fewer workers run than configured for 2m |
| `KeelWorkerRestarted` | warning | A worker was replaced in the last 10m |
| `KeelMetricsSeriesDropped` | warning | The metrics region is full |
| `KeelPoolNoAvailableBackend` | critical | No backend of a pool is available for 1m, unless the pool is being drained |
| `KeelBackendUnhealthy` | warning | A backend fails its health check for 5m |
| `KeelBackendEjectedRepeatedly` | warning | A backend is ejected 3 times in 15m |
| `KeelBackendDrainStuck` | info | A drain still has connections after 30m |
| `KeelHigh5xxRatio` | warning | Over 5% 5xx on a vhost for 5m |
| `KeelHighLatency` | warning | p99 of a vhost over 1s for 10m |
| `KeelBackendConnectionErrors` | warning | Over 0.1/s failed upstream connections to a backend for 5m |
| `KeelNoRoute` | info | Over 1/s requests match no vhost for 10m |
| `KeelL4NoBackend` | critical | TCP or UDP traffic of a pool finds no backend for 2m |

The thresholds are starting points; tune them to your traffic. `KeelDown`
assumes the scrape job is called `keel`.

## Try them

The development stack in the repository root runs Keel, three backends, a
little traffic, Prometheus with these rules and Grafana with these
dashboards:

```bash
docker compose up --build
# Grafana:    http://localhost:3000  (dashboards in the Keel folder)
# Prometheus: http://localhost:9090/alerts
```
