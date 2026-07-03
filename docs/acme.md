# Automatic TLS (ACME / Let's Encrypt)

Keel obtains and renews certificates automatically via the ACME v2 protocol
using the HTTP-01 challenge. The default CA is Let's Encrypt; any ACME v2
compatible CA works.

## Quick start

```yaml
acme:
  email: ops@example.com          # recommended: expiry warnings from the CA

vhosts:
  - host: example.com
    pool: web
    tls:
      acme: true                  # that's it — no cert/key paths
```

On startup Keel registers an ACME account (once), proves control of
`example.com` by answering the HTTP-01 challenge on port 80, obtains the
certificate, and starts serving it — no restart needed. Renewal happens
automatically 30 days before expiry.

HTTP→HTTPS redirect is implicitly enabled for ACME vhosts (the challenge path
is exempt). Opt out with `redirect_http: false` on the vhost.

## Where the configuration lives

- **Global `acme:` block** — account-level settings: contact email, directory
  URL, storage location. One ACME account per Keel instance.
- **Per-vhost `tls.acme: true`** — enables automatic certificates for that
  hostname. Certificates are per-hostname, so enabling is per-vhost.

The global block is optional: if any vhost sets `tls.acme: true` without it,
defaults (Let's Encrypt production, `/var/lib/keel/acme`) apply.

## Global options

```yaml
acme:
  email: ops@example.com
  directory: https://acme-v02.api.letsencrypt.org/directory
  storage: /var/lib/keel/acme
  domains:                        # see "Certificates for TCP / passthrough backends"
    - db.example.com
  root_ca: /path/to/test-ca.pem   # only for testing against Pebble / internal CAs
```

| Field | Default | Notes |
|---|---|---|
| `email` | none | Contact for the ACME account. Optional but recommended. |
| `directory` | Let's Encrypt production | Any ACME v2 directory URL. For testing: `https://acme-staging-v02.api.letsencrypt.org/directory` |
| `storage` | `/var/lib/keel/acme` | Certificates, keys, account credentials, challenge tokens. Created `0700`. |
| `domains` | `[]` | Extra hostnames to issue certificates for that have no TLS vhost. |
| `root_ca` | none | Extra trust root for the ACME API itself (Pebble / internal CA testing). |

## Requirements

- **Port 80 must reach Keel** for the hostname being issued. The CA connects to
  `http://<host>/.well-known/acme-challenge/<token>` — Keel answers this on any
  plain (non-TLS) listener, before redirects and before vhost routing.
- **No wildcards.** HTTP-01 cannot issue wildcard certificates (that requires
  DNS-01, which is planned post-v1). Config validation rejects `*` hosts with
  `acme: true`.
- Use a real, publicly resolvable hostname. The CA resolves it with public DNS.

## Certificates for TCP / TLS-passthrough backends

Sometimes Keel is not the TLS terminator — a backend behind Keel terminates
TLS itself (database frontends, TLS passthrough, plain TCP services). Those
backends still need certificates, and Keel already owns port 80 for the
domain. `acme.domains` handles this the way Lego's standalone HTTP-01 mode
does:

```yaml
acme:
  email: ops@example.com
  domains:
    - db.example.com              # no vhost for this host — cert files only
```

Keel answers the HTTP-01 challenge for `db.example.com` and writes:

```
/var/lib/keel/acme/db.example.com.crt    (0644)
/var/lib/keel/acme/db.example.com.key    (0600)
```

Keel does **not** load these into its own TLS listeners — the operator points
the backend at the files (or copies them out). Renewals rewrite the files
atomically; have the backend watch them or reload periodically.

## Storage layout

```
/var/lib/keel/acme/
├── account.json          # ACME account credentials (0600), reused across renewals
├── challenges/           # live HTTP-01 tokens (transient, auto-cleaned)
├── example.com.crt       # certificate chain
├── example.com.key       # private key (0600)
└── db.example.com.crt/.key
```

The same account is reused for all issuance to stay inside CA rate limits.
Changing `directory` registers a fresh account automatically.

## How it works

- Each worker process runs the ACME service; an exclusive lock in the storage
  directory ensures only one talks to the CA at a time.
- Challenge tokens are files, so **any** worker can answer the CA's validation
  request regardless of which worker initiated the order.
- Issued/renewed certificates are hot-swapped into the TLS listeners within a
  minute — no restart, no dropped connections.
- Failed issuance retries with exponential backoff (1 minute doubling to
  6 hours) to respect CA rate limits. Errors are logged with the reason.
- Renewal runs when a certificate has fewer than 30 days left.

Requests for `/.well-known/acme-challenge/<token>` where the token is
**unknown** are proxied normally — a backend that manages its own ACME
certificates behind Keel keeps working.

## Testing against a local CA (Pebble)

```yaml
acme:
  directory: https://localhost:14000/dir
  storage: /tmp/keel-acme
  root_ca: /path/to/pebble.minica.pem
```

`root_ca` makes Keel trust Pebble's self-signed API certificate. Never needed
for Let's Encrypt (production or staging).
