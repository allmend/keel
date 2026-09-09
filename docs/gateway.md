# API Gateway

Rules that shape traffic on its way through a vhost: a per-client rate
limit, JWT authentication, header changes in both directions, and path
rewriting toward the backend. Each rule is a block on a vhost or on a
route; a route's block overrides the vhost's for that field, the same way
`cache` does.

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

## Authentication: JWT

`auth.jwt` requires a valid JSON Web Token on every request to the vhost or
route. Validation is self-contained: the key is configured, Keel fetches
nothing. A request without a valid token is answered `401` with a
`WWW-Authenticate: Bearer` challenge naming the reason, and counted in
`keel_auth_failures_total{vhost,reason}`. It never reaches a backend.

```yaml
vhosts:
  - host: api.example.com
    pool: api
    auth:
      jwt:
        public_key: /etc/keel/issuer.pem   # RS256/384/512 or ES256/384
        # secret: <base64>                 # or HS256/384/512 with a shared secret
        # secret_file: /etc/keel/jwt.key   # base64 secret in a file
        issuer: https://issuer.example.com # optional: iss must equal this
        audience: api                      # optional: aud must be or contain this
        header: Authorization              # default; expects "Bearer <token>"
        leeway: 30s                        # default; clock skew on exp and nbf
        claim_headers:                     # claims copied to request headers
          sub: X-Auth-Subject
          org: X-Auth-Org
```

| Field | Default | Notes |
|---|---|---|
| `secret`, `secret_file`, `public_key` | exactly one | A secret accepts only `HS*` tokens; a public key only `RS*` and `ES*`. `alg: none` is never accepted |
| `issuer` | none | Required `iss` value when set |
| `audience` | none | Required `aud` value when set; `aud` may be a string or an array |
| `header` | `Authorization` | Header carrying `Bearer <token>` |
| `leeway` | `30s` | Tolerance on `exp` and `nbf` |
| `claim_headers` | none | claim → header. String, number, and boolean claims are copied; others skipped |

`exp` is required in every token; a token without one is malformed. `nbf`
is honoured when present. The token header itself is passed through to the
backend unchanged.

Claim headers are trustworthy at the backend: whatever the client sent
under those names is removed before the verified values are inserted, on
every request, whether or not the claim was present in the token.

Refusal reasons, as they appear in the challenge and the metric: `missing`,
`malformed`, `algorithm`, `signature`, `expired`, `not_yet_valid`,
`issuer`, `audience`.

A key file that cannot be read at startup is a validation error. One that
can be read but not parsed makes the rule refuse every request and logs
the error, rather than letting requests through.

## Order of operations

For one request: vhost and route match → rate limit → JWT → HTTP to HTTPS
redirect → backend selection → forwarded headers → path rewrite → request
header rules → claim headers → backend → `X-Cache` → response header rules
→ client. The rate limit runs before authentication so unauthenticated
floods are cut at the cheaper step.

## Not included

Body rewriting is out of scope by decision. Rate limits keyed by header
value or API key, shared buckets across workers or nodes, API keys as an
auth method, forward-auth to an external service, and JWKS fetching are
candidates once these rules show their gaps in use.
