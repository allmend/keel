# Virtual Hosts

HTTP requests are routed by host to a pool. Each `vhost` block is one host; its label is the host name.

## Host matching

```hcl
vhost "api.example.com" {
  pool = "api"
}

vhost "*.example.com" {
  pool = "sites"
}

vhost "*" {
  pool = "default"
}
```

Order: exact host, then a one-label wildcard, then `*`. `*.example.com` matches `www.example.com`, not `a.b.example.com` or `example.com`. The position of a block does not matter.

The host comes from the `Host` header, or `:authority` on HTTP/2, without its port. Logs and metrics use the configured name (`*.example.com`), never the raw header.

Blocks with the same host are merged into one route list; for a path prefix in two of them, the first loaded wins.

## Path routing

One `route` block per path prefix; the longest matching prefix wins.

```hcl
vhost "example.com" {
  route "/api/v2/" { pool = "api-v2" }
  route "/api/" { pool = "api-v1" }
  route "/" { pool = "web" }
}
```

`/api/v2/users` goes to `api-v2`, `/static/logo.png` to `web`. A request no route matches gets 502.

A vhost has `pool` or `route` blocks, not both. The same path twice in one vhost is a load error.

## TLS

Served on `https` listeners, with a certificate per vhost.

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

The certificate is picked by SNI in the same order as hosts: exact, one-label wildcard, then the `*` vhost's. A client asking for a host with none gets the `*` certificate, or a failed handshake if there is none. `cert` is PEM with the full chain. ACME instead of files: see [ACME](acme.md).

Certificates are re-read with every applied version (`SIGHUP`, `keel config reload`, a push) without dropping connections.

HTTPS offers HTTP/2 and HTTP/1.1 by ALPN.

## Forwarded headers

Keel sets on every upstream request:

| Header | Value |
|---|---|
| `X-Forwarded-For` | Client IP |
| `X-Real-IP` | Client IP |
| `X-Forwarded-Proto` | `http` or `https` |
| `X-Forwarded-Host` | The request's host |
| `Forwarded` | RFC 7239: `for`, `proto`, `host` |

```hcl
vhost "internal.example.com" {
  pool = "internal"

  forwarded_headers {
    mode            = "append"
    trusted_proxies = ["10.0.0.0/8", "172.16.0.0/12"]
  }
}
```

| `mode` | |
|---|---|
| `replace` (default) | Incoming forwarded headers are replaced with the direct client's IP |
| `append` | A direct client inside `trusted_proxies` keeps its `X-Forwarded-For` chain, Keel appends its IP, and `X-Real-IP` is the first address of the chain. Any other client is handled as `replace` |
| `off` | Forwarded headers are removed |

## WebSocket

The HTTP/1.1 upgrade passes through on `http` and `https` listeners. An open socket:

- is a connection to its backend: it counts in `keel_active_connections` and keeps a drain at `draining` until it closes;
- is never cached;
- gets one access entry when it closes: status `101`, `duration_ms` the socket's lifetime, `bytes_in`/`bytes_out` the frames each way;
- is closed `grace_period` after a shutdown signal.

WebSocket over HTTP/2 (RFC 8441) is not supported. Clients offered `h2` open WebSockets on a separate HTTP/1.1 connection, as browsers do when the server does not offer extended CONNECT.

## PROXY protocol

With `proxy_protocol = true` a listener reads a PROXY protocol header (v1 or v2) before anything else and uses its address as the client's: in forwarded headers, logs, the consistent-hash key and passive health.

```hcl
listener "http" "0.0.0.0:80" {
  proxy_protocol = true
}
```

- Every protocol: `http`, `https` (read before the handshake), `tcp` in every `tls_mode`, and `udp` (a v2 header per datagram).
- A `LOCAL` (v2) or `UNKNOWN` (v1) header keeps the socket's peer as the client.
- A connection without a valid header is closed without a response and counted in `keel_proxy_protocol_errors_total`.

## Access lists

`allow` and `deny` take networks or single addresses, IPv4 and IPv6, on a vhost, a route, or a `tcp` or `udp` listener.

```hcl
vhost "admin.example.com" {
  allow = ["10.0.0.0/8", "2001:db8::/32"]
  deny  = ["10.0.66.0/24"]

  route "/" {
    pool = "admin"
  }

  route "/health" {
    pool  = "admin"
    allow = ["0.0.0.0/0", "::/0"]
  }
}

listener "tcp" "0.0.0.0:5432" {
  pool  = "postgres"
  allow = ["10.1.0.0/16"]
}
```

- `deny` first. Then, if `allow` lists anything, only those clients.
- A route with a list of its own uses it instead of the vhost's.
- HTTP answers `403`, TCP closes the connection, UDP drops the datagram. Each is logged and counted with error `denied`.
- The client is the PROXY protocol address when the listener reads one. When that client is in the vhost's `trusted_proxies`, it is the last `X-Forwarded-For` address that is not.
- A push applies them live. Open TCP connections and UDP flows are not cut; HTTP checks every request.
- ACME challenges are answered before the check.

## HTTP to HTTPS redirect

`redirect_http = true` answers plain HTTP for the host with a 301 to `https://<host><path>?<query>`. Default `false`, `true` for ACME vhosts. On `vhost "*"` it applies to every host no other vhost matches.

```hcl
vhost "example.com" {
  pool          = "web"
  redirect_http = true

  tls {
    cert = "/etc/keel/certs/example.crt"
    key  = "/etc/keel/certs/example.key"
  }
}
```

## Default action

A vhost with a `default_action` block answers itself, without a pool. It excludes `pool` and `route`.

```hcl
vhost "*" {
  default_action {
    redirect = "https://example.com"
  }
}

vhost "app.example.com" {
  default_action {
    status = 503
    body   = "Down for maintenance"
  }
}
```

| Attribute | Default | Notes |
|---|---|---|
| `redirect` | — | Absolute `http(s)://` URL; answers 301 |
| `preserve_path` | `true` | Append the request's path and query to `redirect` |
| `status` | — | 100–599; excludes `redirect` |
| `body` | empty | With `status` only |

It applies on HTTP and HTTPS. ACME challenges are answered before it. On `vhost "*"` it applies only to hosts no other vhost matches.
