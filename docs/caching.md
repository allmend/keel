# Caching

An HTTP cache with two optional tiers: memory (L1) and disk (L2). Storage is set once in `keel.hcl`; what is cached is set per vhost or route. Off by default.

## Storage

```hcl
cache {
  memory = "256M"

  disk {
    path = "/var/cache/keel"
    size = "10G"
  }
}
```

Either tier may be left out. Without a `cache` block nothing is cached.

| Tier | Behaviour |
|---|---|
| Memory | LRU in process memory, lost on restart. Per worker: total use up to `memory` × workers, and one worker's object is a miss in the others |
| Disk | Files at `{path}/{hash[0:2]}/{hash}.keel`, created on first write, shared by all workers. Each worker counts and evicts (LRU) only what it wrote, so total use is up to `size` × workers. Files from an earlier run are served but not counted or evicted |
| Both | Reads try memory, then disk. A miss is written to both. A failed disk write is not logged; memory still stores the response |

| Size suffix | Unit |
|---|---|
| `K`, `KB` | KiB |
| `M`, `MB` | MiB |
| `G`, `GB` | GiB |
| none, `B` | bytes |

## Rules

A `cache` block on a vhost or route decides what is cached.

```hcl
vhost "example.com" {
  cache {
    enabled = true
    ttl     = "1m" # every route without a cache block of its own
  }

  route "/static/" {
    pool = "assets"

    cache {
      enabled       = true
      ttl           = "24h"
      statuses      = [200, 301]
      content_types = ["image/*", "text/css", "application/javascript", "font/*"]
    }
  }

  route "/api/" {
    pool = "api"

    cache {
      enabled = false # never cached
    }
  }

  route "/" { pool = "web" } # the vhost's rule
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | |
| `ttl` | duration | none | Fallback when the origin gives no freshness |
| `statuses` | list of numbers | `[200]` | Status codes to cache |
| `content_types` | list of strings | all | Content types to cache |

A route's `cache` block replaces the vhost's, with no merging. A route without one uses the vhost's.

## What is stored

Only `GET` and `HEAD`. A response is stored when all of these hold:

1. Its status is in `statuses`.
2. Its `Content-Type` matches `content_types`, if set.
3. It sets no cookie and has no `Vary: *`.
4. `Cache-Control` does not say `private` or `no-store`. A request with `Authorization` is stored only with `public`, `s-maxage` or `must-revalidate` (RFC 9111 §3.5).
5. It is fresh: `s-maxage`, `max-age` or `Expires` give the lifetime. Without any of them, and when storing is not forbidden, `ttl` does — except for requests with `Authorization` or `Cookie`.

A response with `Vary` is stored per combination of the named request headers' values, and served only to a request with the same values.

Content types are matched without parameters (`text/html; charset=utf-8` is `text/html`). A trailing `*` is a prefix match: `image/*` matches `image/png`.

## Metrics

`keel_cache_requests_total{pool, vhost, result}` counts hits and misses per route, `keel_cache_hits_total{tier}` hits per tier. See [Metrics](metrics.md#cache-metrics).

## X-Cache

Responses on a vhost or route with caching enabled carry `X-Cache: HIT` (from cache) or `X-Cache: MISS` (from the backend). Others carry none.
