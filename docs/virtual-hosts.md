# Virtual Hosts

Keel routes incoming requests to backend pools based on the HTTP `Host` header. Each `vhost` block defines one virtual host; its label is the host.

Vhosts are matched by host name, not by their position in the config: exact host first, then a one-label wildcard, then `*` (see below). Several blocks with the same host are merged into one route list; when two of them define the same path prefix, the block that is loaded first wins.

## Host matching

```hcl
vhost "api.example.com" { # exact match
  pool = "api"
}

vhost "*.example.com" { # one label below example.com: www, app, …
  pool = "sites"
}

vhost "*" { # catch-all: anything not matched above
  pool = "default"
}
```

Port numbers in the `Host` header are stripped before matching. A request with `Host: api.example.com:8080` matches `vhost "api.example.com"`. HTTP/2 requests carry the name in `:authority` rather than a `Host` header; either one is matched.

Resolution order is exact host, then the one-label wildcard, then the bare `*`. `*.example.com` covers `www.example.com` but not `a.b.example.com` and not `example.com` itself, the same rule certificates use. The same order selects the certificate by SNI on TLS listeners, so a wildcard vhost with a wildcard certificate serves it to every name it routes — supply that certificate yourself, as ACME (HTTP-01) cannot issue wildcards. Access logs and metrics label such requests with the configured wildcard name, never with the raw host header.

---

## Path routing

Within a vhost, requests can be routed to different pools based on path prefix, one `route` block per prefix. Longest prefix wins, so the order of the blocks does not matter.

```hcl
vhost "example.com" {
  route "/api/v2/" { pool = "api-v2" }
  route "/api/" { pool = "api-v1" }
  route "/" { pool = "web" }
}
```

A request to `/api/v2/users` matches `/api/v2/` (longer prefix wins over `/api/`). A request to `/static/logo.png` matches `/`.

A vhost has either `pool` or `route` blocks; both is a load error, and so is the same path twice in one vhost. If no route matches, the request is rejected with 502. Always include a `/` route to avoid this.

---

## TLS

TLS termination is configured per-vhost, served on `https` listeners.

```hcl
listener "https" "0.0.0.0:443" {}

vhost "api.example.com" {
  pool = "api"

  tls {
    cert = "/etc/keel/certs/api.crt"
    key  = "/etc/keel/certs/api.key"
  }
}
```

Keel selects the certificate for each connection using SNI. Multiple vhosts on the same listener can have different certificates.

Certificate files are PEM-encoded. The `cert` file should contain the full chain (leaf + intermediates).

TLS certificates hot-swap on config reload (`SIGHUP` or `keel config reload`). No connections are dropped during a certificate rotation.

On a TLS listener the certificate is chosen by SNI: exact host, then a one-label wildcard, then the certificate of a `*` vhost. A client asking for a host with no matching certificate gets the `*` certificate if one exists; otherwise the handshake fails. All vhosts served on a TLS listener should have TLS configured.

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

`mode = "replace"` (default) — Keel overwrites any existing forwarded headers with the direct client's IP. Use this when Keel is the first proxy that clients connect to. Prevents clients from spoofing `X-Forwarded-For`.

`mode = "append"` — when the direct client's address is inside `trusted_proxies`, Keel keeps the incoming `X-Forwarded-For` chain, appends the direct client's IP, and takes `X-Real-IP` from the first address in the chain. Use this when Keel sits behind another trusted proxy (e.g. a cloud load balancer). A direct client outside `trusted_proxies` is treated as in `replace` mode.

`mode = "off"` — Keel removes all forwarded headers from the upstream request. Use this when backends should not receive client IP information.

```hcl
vhost "example.com" {
  pool = "web"

  forwarded_headers {
    mode = "replace" # overwrite: safe default
  }
}

vhost "internal.example.com" {
  pool = "internal"

  forwarded_headers {
    mode            = "append"
    trusted_proxies = ["10.0.0.0/8", "172.16.0.0/12"] # only trust X-F-F from these
  }
}

vhost "private.example.com" {
  pool = "private"

  forwarded_headers {
    mode = "off" # strip all forwarded headers
  }
}
```

When `mode = "append"` is set without `trusted_proxies`, no client is trusted and every request is handled as in `replace` mode.

Without a `forwarded_headers` block the mode is `replace`.

---

## PROXY Protocol

An L4 load balancer that terminates the client's TCP connection in front of Keel (a cloud load balancer, another proxy, another Keel) can prepend a PROXY Protocol header naming the original client. With `proxy_protocol = true` on a listener, Keel reads that header before anything else and uses the address it carries as the client's: in forwarded headers, access logs, the consistent-hash key, and passive health.

```hcl
listener "http" "0.0.0.0:80" {
  proxy_protocol = true
}
```

On an `https` listener the header is read before the TLS handshake, so the real client address is known for the handshake's own logging as well.

Versions 1 (text) and 2 (binary) are accepted. A `LOCAL` (v2) or `UNKNOWN` (v1) header, which load balancers send for their own health checks, leaves the socket's own peer as the client. A connection that does not start with a valid header is closed without a response and counted in `keel_proxy_protocol_errors_total`; there is no fallback, since a fallback would let any client claim any address. Enable the option only on listeners that are reached exclusively through a load balancer sending the header.

The option works on every listener protocol: `http`, `https` (the header is read before the handshake), `tcp` in every `tls_mode`, and `udp`, where each datagram carries a v2 header.

---

## HTTP → HTTPS redirect

Set `redirect_http = true` on a vhost to have Keel respond to plain HTTP requests for that host with a permanent `301` redirect to the HTTPS equivalent.

```hcl
vhost "example.com" {
  pool          = "web"
  redirect_http = true # 301 redirect for HTTP requests to this host

  tls {
    cert = "/etc/keel/certs/example.crt"
    key  = "/etc/keel/certs/example.key"
  }
}
```

The redirect preserves the full request path and query string. The `Location` header is set to `https://<host><path>`.

The listener on port 80 must exist for the redirect to be served:

```hcl
listener "http" "0.0.0.0:80" {} # serves the redirect

listener "https" "0.0.0.0:443" {}
```

`redirect_http` defaults to `false`. Setting it without a TLS vhost for the same host is valid — Keel will redirect regardless of whether it terminates TLS itself or another component does.

The wildcard host (`vhost "*"`) supports `redirect_http = true` to redirect all unmatched HTTP hosts.

---

## Default action

A vhost with a `default_action` block answers requests directly — no backend pool involved. It takes a redirect or a static status, and excludes `pool` and `route` blocks on the same vhost:

```hcl
vhost "example.com" {
  pool = "web"
}

# Bare-IP access or unknown Host header: send to the real site
vhost "*" {
  default_action {
    redirect = "https://example.com" # 301; path + query appended
  }
}
```

```hcl
# Or: refuse unknown hosts with a static response
vhost "*" {
  default_action {
    status = 404
    body   = "Not found"
  }
}
```

```hcl
# Maintenance page for one host, no pool needed
vhost "app.example.com" {
  default_action {
    status = 503
    body   = "Down for maintenance, back shortly"
  }
}
```

| Attribute | Type | Default | Notes |
|---|---|---|---|
| `redirect` | string | — | Absolute `http(s)://` URL; responds `301` |
| `preserve_path` | bool | `true` | Append the request path + query to `redirect` |
| `status` | number | — | Static response status (100–599) |
| `body` | string | empty | Static response body; only valid with `status` |

The action applies on both plain HTTP and TLS listeners, and the ACME challenge path is served before it. On a wildcard vhost, the action fires only for hosts no exact vhost matches — exact vhosts route to their pools as usual.
