# Automatic TLS (ACME / Let's Encrypt)

Keel obtains and renews certificates automatically via the ACME v2 protocol
using the HTTP-01 challenge. Certificates come from named **issuers** — by
default Let's Encrypt, but any ACME v2 compatible CA works, and different
vhosts can use different CAs side by side.

## Quick start

The common case needs almost nothing:

```yaml
acme:
  issuers:
    default:
      email: ops@example.com      # recommended: expiry warnings from the CA

vhosts:
  - host: example.com
    pool: web
    tls:
      acme: true                  # uses the issuer named "default"
```

Even the `acme:` block is optional — `tls: { acme: true }` alone implies a
`default` issuer pointing at Let's Encrypt production (just without a contact
email).

On startup Keel registers the ACME account (once, reused forever), proves
control of `example.com` by answering the HTTP-01 challenge on port 80,
obtains the certificate, and starts serving it — no restart needed.

HTTP→HTTPS redirect is implicitly enabled for ACME vhosts (the challenge path
is exempt). Opt out with `redirect_http: false` on the vhost.

## Issuers

An **issuer** is a named CA relationship: directory URL, account contact, and
optionally a private trust root. Each issuer gets exactly one ACME account,
stored under `storage/<issuer>/account.json` and reused across all issuance —
this is what keeps you inside Let's Encrypt rate limits.

Vhosts pick their issuer by name:

```yaml
acme:
  storage: /var/lib/keel/acme
  renew_before: 30%
  issuers:
    default:                      # what `acme: true` refers to
      email: ops@example.com
    internal:                     # a private CA (step-ca, Vault, ...)
      email: infra@example.com
      directory: https://ca.corp.internal/acme/acme/directory
      root_ca: /etc/keel/corp-root.pem

vhosts:
  - host: www.example.com
    pool: web
    tls: { acme: true }           # issuer "default" → Let's Encrypt

  - host: intranet.corp.internal
    pool: intranet
    tls: { acme: internal }       # named issuer → corporate CA
```

Rules enforced at config load:

- `tls.acme` naming an issuer that isn't defined under `acme.issuers` is an
  error (`default` is the only name that may be implicit).
- A hostname belongs to exactly one issuer — assigning the same host to two
  issuers is an error.
- HTTP-01 cannot issue wildcards; `*` hosts with ACME are rejected.

### Issuer fields

| Field | Default | Notes |
|---|---|---|
| `email` | none | Account contact. Optional but recommended. |
| `directory` | Let's Encrypt production | Any ACME v2 directory URL. Staging: `https://acme-staging-v02.api.letsencrypt.org/directory` |
| `root_ca` | none | PEM trust root for the ACME API itself — internal CAs and Pebble. Never needed for Let's Encrypt. |
| `renew_before` | global value | Per-issuer renewal override. |

## Renewal

```yaml
acme:
  renew_before: 30%     # global default; issuers can override
```

`renew_before` decides when a certificate is renewed, in one of two forms:

- **Percentage** (`30%`) — renew when less than that share of the
  certificate's *total lifetime* remains. This scales across CA policies:
  a 90-day Let's Encrypt certificate renews with ~27 days left, a 6-day
  short-lived certificate with ~1.7 days left. **This is the default (30%).**
- **Absolute** (`20d`) — renew when fewer than that many days remain.
  Simpler to reason about, but wrong for short-lived certificates; prefer
  the percentage.

The renewal check runs every 60 seconds, so renewal starts within a minute of
crossing the threshold. Renewed certificates are hot-swapped into the TLS
listeners without a restart; existing connections are unaffected.

Failed issuance retries with exponential backoff (1 minute doubling to 6
hours) so a misconfigured domain cannot burn CA rate limits. Every failure is
logged with the reason.

## Requirements

- **Port 80 must reach Keel** for the hostname being issued. The CA fetches
  `http://<host>/.well-known/acme-challenge/<token>`; Keel answers on any
  plain (non-TLS) listener, before redirects and before vhost routing.
- **Public DNS.** The CA resolves the hostname with public resolvers (for
  internal CAs, whatever DNS your CA uses).
- **No wildcards** — that needs DNS-01, which is planned post-v1.

## Certificates for TCP / TLS-passthrough backends

Sometimes Keel is not the TLS terminator — a backend behind Keel terminates
TLS itself (databases, TLS passthrough, plain TCP services). Those backends
still need certificates, and Keel already owns port 80 for the domain. The
top-level `certificates:` section handles this the way Lego's standalone
HTTP-01 mode does:

```yaml
certificates:
  - host: db.example.com          # no vhost — Keel only does the challenge
    issuer: default               # optional; "default" when omitted
```

Because `certificates:` is a top-level list (like `vhosts:`), conf.d files
can declare their own entries next to their pools and vhosts:

```yaml
# /etc/keel/conf.d/db-team.yaml
pools:
  db-frontend: { ... }

certificates:
  - host: db.example.com
```

Keel answers the HTTP-01 challenge for `db.example.com` and writes:

```
/var/lib/keel/acme/db.example.com.crt    (0644)
/var/lib/keel/acme/db.example.com.key    (0600)
```

Keel does **not** load these into its own TLS listeners — point the backend
at the files (or copy them out). Renewals rewrite the files atomically; have
the backend watch them or reload periodically.

## Storage layout

```
/var/lib/keel/acme/               # acme.storage, created 0700
├── default/
│   └── account.json              # one ACME account per issuer (0600)
├── internal/
│   └── account.json
├── challenges/                   # live HTTP-01 tokens (transient, auto-cleaned)
├── www.example.com.crt           # certificate chain
├── www.example.com.key           # private key (0600)
└── db.example.com.crt/.key
```

Certificate files are flat (`{host}.crt` / `{host}.key`) regardless of issuer
— a hostname has exactly one certificate. Changing an issuer's `directory`
registers a fresh account automatically; existing certificates keep serving
until their normal renewal.

## Restarts and persistence

Certificates persist on disk and are **not** re-issued on restart, reload, or
reboot. On startup Keel loads whatever is in `storage`; the CA is only
contacted when a certificate is missing, expired, unparsable, or inside the
renewal window. A Keel that restarts while the CA is unreachable keeps
serving its existing certificates without logging a single ACME error.

## Cluster mode

In cluster mode certificates are replicated through the Raft log:

- **Only the leader talks to the CAs.** One account, one issuance, no
  duplicate certificates across nodes.
- Issued and renewed certificates are committed as Raft entries; every node
  (and any node that joins later, via snapshot) writes them to its own
  `storage` and hot-swaps them into its TLS listeners.
- On startup and continuously, disk and Raft state are reconciled per host:
  the **valid certificate with the most remaining lifetime wins** and
  overwrites the other side, so both converge on one source of truth. A
  full-cluster restart recovers certificates from disk; the leader pushes
  them back into Raft.

Current limitation: during issuance the HTTP-01 token is served by the
leader, so port-80 traffic for the domain must be able to reach the leader
(challenge-token replication to all nodes is a planned follow-up).

## How it works

- Each worker process runs the ACME service; an exclusive lock in the storage
  directory ensures only one talks to the CAs at a time.
- Challenge tokens are files, so **any** worker can answer the CA's
  validation request regardless of which worker initiated the order.
- Issued/renewed certificates are hot-swapped into the TLS listeners within a
  minute — no restart, no dropped connections.

Requests for `/.well-known/acme-challenge/<token>` where the token is
**unknown** are proxied normally — a backend that manages its own ACME
certificates behind Keel keeps working.

## Testing against a local CA (Pebble)

```yaml
acme:
  storage: /tmp/keel-acme
  issuers:
    pebble:
      directory: https://localhost:14000/dir
      root_ca: /path/to/pebble.minica.pem

vhosts:
  - host: test.example
    pool: web
    tls: { acme: pebble }
```

`root_ca` makes Keel trust Pebble's self-signed API certificate. Never needed
for Let's Encrypt (production or staging).
