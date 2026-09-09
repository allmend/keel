# API Gateway

Rules that shape traffic on its way through a vhost: a per-client rate
limit, header changes in both directions, and path rewriting toward the
backend. Each rule is a block on a vhost or on a route; a route's block
overrides the vhost's for that field, the same way `cache` does.

```yaml
vhosts:
  - host: api.example.com
    rate_limit:
      requests: 100          # per client IP
      per: 1m
    headers:
      request:
        set: { X-Env: prod }
        remove: [X-Internal]
      response:
        set: { Strict-Transport-Security: "max-age=63072000" }
        remove: [Server]
    routes:
      - path: /v1/
        pool: api-v1
        rewrite: { strip_prefix: /v1 }
        rate_limit: { requests: 10, per: 1s, burst: 30 }   # overrides the vhost's
      - path: /
        pool: api
```

## Rate limiting

A token bucket per client IP and rule. The bucket holds `burst` tokens
(default: `requests`), refills at `requests` per `per`, and each request
takes one. A request that finds the bucket empty is answered with `429`,
`Retry-After` in whole seconds, and the body `rate limited`. It never
reaches a backend and is counted in `keel_rate_limited_total{vhost}`.

| Field | Default | Notes |
|---|---|---|
| `requests` | required | Tokens added per `per` |
| `per` | `1s` | `500ms`, `1s`, `1m`, `1h` |
| `burst` | `requests` | Bucket capacity: the largest run allowed after idle time |

The client IP is the connection's peer, or the address from a PROXY
protocol header on listeners with `proxy_protocol: true`. Forwarded
headers from an upstream proxy are not consulted.

Buckets live inside one worker process. With `workers: N`, a client whose
connections spread across workers can obtain up to N times the configured
rate in total. Set `workers: 1` where the limit must be exact, or size the
limit for the process group. Buckets idle for ten minutes are dropped.

A rule is identified by its host and path prefix, so two routes with the
same limit count separately, and the vhost-level limit applies to every
route that does not set its own.

## Header rules

`set` replaces (or adds) headers with static values; `remove` deletes them.
`request` applies to what the backend receives, after Keel's own forwarded
headers, so a rule can override `X-Forwarded-For` and friends. `response`
applies to what the client receives, after `X-Cache`.

Names must be valid header names; values may not contain line breaks.
Matching is case-insensitive. Within one block `set` runs before `remove`.
There is no templating: a value is sent as written.

## Path rewriting

`rewrite` changes the path sent to the backend; the client-facing URL and
the route match are unchanged.

| Field | Notes |
|---|---|
| `strip_prefix` | Removed when the path is the prefix or starts with it at a segment boundary: `/api` turns `/api/users` into `/users` and `/api` into `/`, and leaves `/apiary` alone |
| `add_prefix` | Prepended after stripping: `/internal` turns `/users` into `/internal/users` |

The query string is preserved. Cache keys use the client-facing path, so a
rewrite does not merge cache entries.

## Order of operations

For one request: vhost and route match → rate limit → HTTP to HTTPS
redirect → backend selection → forwarded headers → path rewrite → request
header rules → backend → `X-Cache` → response header rules → client.

## Not included

Authentication is the next gateway feature. Body rewriting is out of scope
by decision. Rate limits keyed by header value or API key, and shared
buckets across workers or nodes, are candidates once the per-IP limit
shows its gaps in use.
