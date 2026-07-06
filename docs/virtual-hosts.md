# Virtual Hosts

Keel routes incoming requests to backend pools based on the HTTP `Host` header. Each entry in `vhosts` defines one virtual host.

Vhosts are evaluated in the order they appear in config. The first match wins.

## Host matching

```yaml
vhosts:
  - host: api.example.com    # exact match
    pool: api

  - host: "*"                # wildcard — matches anything not matched above
    pool: default
```

Port numbers in the `Host` header are stripped before matching. A request with `Host: api.example.com:8080` matches `host: api.example.com`.

The `*` wildcard matches any host that doesn't match an earlier vhost. Only a bare `*` is supported; partial wildcards like `*.example.com` are not.

---

## Path-prefix routing

Within a vhost, requests can be routed to different pools based on path prefix. Longest prefix wins.

```yaml
vhosts:
  - host: example.com
    routes:
      - path: /api/v2/
        pool: api-v2
      - path: /api/
        pool: api-v1
      - path: /
        pool: web
```

A request to `/api/v2/users` matches `/api/v2/` (longer prefix wins over `/api/`). A request to `/static/logo.png` matches `/`.

When `routes` is specified, `pool` at the vhost level is ignored. If no route matches, the request is rejected with 502. Always include a `/` catch-all route to avoid this.

---

## TLS

TLS termination is configured per-vhost. The listener must have `tls: true`.

```yaml
listeners:
  - address: 0.0.0.0:443
    tls: true

vhosts:
  - host: api.example.com
    pool: api
    tls:
      cert: /etc/keel/certs/api.crt
      key: /etc/keel/certs/api.key
```

Keel selects the certificate for each connection using SNI. Multiple vhosts on the same listener can have different certificates.

Certificate files are PEM-encoded. The `cert` file should contain the full chain (leaf + intermediates).

TLS certificates hot-swap on config reload (`SIGHUP` or `keel config reload`). No connections are dropped during a certificate rotation.

Vhosts with no `tls` block serve plain HTTP even on a TLS listener (SNI will fail to match). All vhosts on a TLS listener should have TLS configured.

---

## Forwarded headers

Keel sets forwarded headers on every upstream request so backends can see the real client IP and protocol.

Headers set:
- `X-Forwarded-For` — client IP address
- `X-Real-IP` — client IP address (single value)
- `X-Forwarded-Proto` — protocol of the client-to-Keel connection (`http` or `https`)
- `X-Forwarded-Host` — original `Host` header value
- `Forwarded` — RFC 7239 standard header

### Modes

`mode: replace` (default) — Keel overwrites any existing forwarded headers with the direct client's IP. Use this when Keel is the first proxy that clients connect to. Prevents clients from spoofing `X-Forwarded-For`.

`mode: append` — Keel preserves any forwarded headers from upstream and appends the direct client's IP. Use this when Keel sits behind another trusted proxy (e.g. a cloud load balancer) and you want to preserve the original client IP chain. Use `trusted_proxies` to restrict which upstream addresses are trusted.

`mode: off` — Keel removes all forwarded headers from the upstream request. Use this when backends should not receive client IP information.

```yaml
vhosts:
  - host: example.com
    pool: web
    forwarded_headers:
      mode: replace          # overwrite — safe default

  - host: internal.example.com
    pool: internal
    forwarded_headers:
      mode: append
      trusted_proxies:       # only trust X-F-F from these upstream CIDRs
        - 10.0.0.0/8
        - 172.16.0.0/12

  - host: private.example.com
    pool: private
    forwarded_headers:
      mode: off              # strip all forwarded headers
```

When `mode: append` is set without `trusted_proxies`, Keel appends to whatever `X-Forwarded-For` value arrives, including values injected by clients. Only use `append` without `trusted_proxies` on listeners that are not reachable by untrusted clients.

Forwarded header configuration defaults to `mode: replace` if the `forwarded_headers` key is absent.

---

## PROXY Protocol

When Keel sits behind a cloud load balancer or fronting proxy that uses PROXY Protocol to convey the real client IP at the TCP level, enable PROXY Protocol on the listener:

```yaml
listeners:
  - address: 0.0.0.0:80
    proxy_protocol: true
  - address: 0.0.0.0:443
    tls: true
    proxy_protocol: true
```

With PROXY Protocol enabled, Keel reads the real client IP from the PROXY Protocol header before processing HTTP. The real client IP is then used in forwarded headers sent to backends.

Do not enable `proxy_protocol` on a listener unless the upstream load balancer is configured to send PROXY Protocol. Enabling it on a plain HTTP listener will cause connection failures.

---

## HTTP → HTTPS redirect

Set `redirect_http: true` on a vhost to have Keel respond to plain HTTP requests for that host with a permanent `301` redirect to the HTTPS equivalent.

```yaml
vhosts:
  - host: example.com
    pool: web
    tls:
      cert: /etc/keel/certs/example.crt
      key: /etc/keel/certs/example.key
    redirect_http: true    # 301 redirect for HTTP requests to this host
```

The redirect preserves the full request path and query string. The `Location` header is set to `https://<host><path>`.

The listener on port 80 must exist for the redirect to be served:

```yaml
listeners:
  - address: 0.0.0.0:80    # serves the redirect
  - address: 0.0.0.0:443
    tls: true
```

`redirect_http` defaults to `false`. Setting it without a TLS vhost for the same host is valid — Keel will redirect regardless of whether it terminates TLS itself or another component does.

The wildcard host (`host: "*"`) supports `redirect_http: true` to redirect all unmatched HTTP hosts.

---

## Default action

A vhost with `default_action` answers requests directly — no backend pool involved. It takes a redirect **or** a static status, and excludes `pool` and `routes` on the same vhost:

```yaml
vhosts:
  - host: example.com
    pool: web

  # Bare-IP access or unknown Host header → send to the real site
  - host: "*"
    default_action:
      redirect: https://example.com    # 301; path + query appended
```

```yaml
  # Or: refuse unknown hosts with a static response
  - host: "*"
    default_action:
      status: 404
      body: "Not found"
```

```yaml
  # Maintenance page for one host, no pool needed
  - host: app.example.com
    default_action:
      status: 503
      body: "Down for maintenance — back shortly"
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `redirect` | string | — | Absolute `http(s)://` URL; responds `301` |
| `preserve_path` | bool | `true` | Append the request path + query to `redirect` |
| `status` | integer | — | Static response status (100–599) |
| `body` | string | empty | Static response body; only valid with `status` |

The action applies on both plain HTTP and TLS listeners, and the ACME challenge path is served before it. On a wildcard vhost, the action fires only for hosts no exact vhost matches — exact vhosts route to their pools as usual.
