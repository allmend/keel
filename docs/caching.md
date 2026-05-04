# Caching

Keel provides an HTTP cache with two optional storage tiers: in-memory (L1) and disk (L2). Cache rules are configured per vhost or per route within a vhost. Caching is disabled by default everywhere.

## Storage configuration (global)

Storage is configured once at the root level and shared across all vhosts.

### Memory only

```yaml
cache:
  memory: 256M
```

Stores objects in process memory using LRU eviction. Fast, no I/O, lost on restart.

### Disk only

```yaml
cache:
  disk:
    path: /var/cache/keel
    size: 10G
```

Stores objects as files. Layout: `{path}/{hash[0:2]}/{hash}.keel`. The directory is created on first write. Objects are evicted by LRU when total size approaches `size`.

### Tiered (memory + disk)

```yaml
cache:
  memory: 256M
  disk:
    path: /var/cache/keel
    size: 10G
```

When both tiers are configured:

- Reads check L1 (memory) first. On a hit, no disk I/O occurs.
- On an L1 miss, L2 (disk) is checked.
- On a full cache miss, the response is written to both tiers simultaneously. Subsequent requests find the object in L1.
- If an L2 write fails (disk full, I/O error), L1 still stores the response. The failure is logged and does not affect the response.

This is the recommended production configuration. Memory absorbs the hot working set; disk holds everything else.

### Size format

| Suffix | Meaning |
|---|---|
| `K` or `KB` | kibibytes (1024 bytes) |
| `M` or `MB` | mebibytes |
| `G` or `GB` | gibibytes |
| none or `B` | bytes |

---

## Cache rules

Rules control *which* responses are cached and for how long. They live on the vhost or on individual routes within a vhost — not on pools.

### Vhost-level rules

```yaml
vhosts:
  - host: example.com
    pool: web
    cache:
      enabled: true
      ttl: 60
      statuses: [200, 301]
      content_types:
        - text/html
        - text/css
        - application/javascript
        - image/*
        - font/*
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Must be `true` to cache any response for this vhost |
| `ttl` | integer | none | Fallback TTL in seconds when origin sends no `Cache-Control` |
| `statuses` | list of integers | `[200]` | HTTP status codes to cache |
| `content_types` | list of strings | *(all)* | Content-type patterns to cache; empty means no restriction |

### Route-level rules

Routes inherit the vhost-level cache config. A route can override it, restrict it, or disable it entirely.

```yaml
vhosts:
  - host: example.com
    cache:
      enabled: true
      ttl: 60                  # default for all routes

    routes:
      - path: /static/
        pool: assets
        cache:
          enabled: true
          ttl: 86400           # static files: cache for 24h
          statuses: [200, 301]
          content_types:
            - image/*
            - text/css
            - application/javascript
            - font/*

      - path: /api/
        pool: api
        cache:
          enabled: false       # API responses: never cache

      - path: /
        pool: web
        # no cache block — inherits vhost default (ttl: 60, all statuses/content_types)
```

Resolution order for a request: **route-level → vhost-level**. If a route defines a `cache` block, it is used in full — it does not merge with the vhost-level config. If the route has no `cache` block, the vhost-level config applies.

A route with `enabled: false` will not cache even if the vhost-level config has `enabled: true`.

---

## How responses are evaluated

When a cacheable request arrives (GET or HEAD, cache enabled for the matching route/vhost):

1. **Status filter** — the response status must be in `statuses`. If `statuses` is empty, only `200` is cached.
2. **Content-type filter** — if `content_types` is non-empty, the response `Content-Type` header must match at least one pattern.
3. **Cache-Control** — if the origin sends a valid `Cache-Control` max-age, it is respected. `no-store` and `private` are always honoured — Keel will not cache them regardless of the rule config.
4. **TTL fallback** — if the origin sends no usable `Cache-Control` and `ttl` is set, the response is cached for `ttl` seconds.

All conditions must pass. A response that matches the status and content-type filters but carries `Cache-Control: no-store` is not cached.

### Content-type pattern matching

Patterns match the base content type, ignoring parameters (`text/html; charset=utf-8` → `text/html`).

A trailing `*` means prefix match:

| Pattern | Matches |
|---|---|
| `image/*` | `image/png`, `image/jpeg`, `image/webp`, … |
| `text/*` | `text/html`, `text/css`, `text/plain`, … |
| `application/javascript` | `application/javascript` only |
| `font/*` | `font/woff`, `font/woff2`, … |

---

## X-Cache header

When caching is enabled for a vhost or route, Keel adds an `X-Cache` header to every response:

| Value | Meaning |
|---|---|
| `HIT` | Response served from cache |
| `MISS` | Response fetched from upstream |

The header is omitted on requests where caching is not configured.

---

## Disabling cache entirely

Omit the root `cache:` section. No storage is allocated and `X-Cache` headers are never added.

Even with storage configured, individual vhosts and routes only cache when `enabled: true` is set on their cache block.
