# Automatic TLS (ACME)

Keel can obtain and renew certificates automatically using the ACME v2 protocol with the HTTP-01 challenge. Certificates come from named issuers. An issuer defaults to the public ACME directory at `https://acme-v02.api.letsencrypt.org/directory`, but any ACME v2 CA works, and different vhosts can use different CAs.

## Quick start

```yaml
acme:
  issuers:
    default:
      email: ops@example.com      # optional; the CA sends expiry warnings here

vhosts:
  - host: example.com
    pool: web
    tls:
      acme: true                  # use the issuer named "default"
```

The `acme:` block is itself optional. `tls: { acme: true }` on its own implies a `default` issuer pointing at the public production directory above, without a contact email.

On startup Keel registers an ACME account, proves control of `example.com` by answering the HTTP-01 challenge on port 80 (or, for a `dns-01` issuer, by publishing a DNS record), obtains the certificate, and begins serving it. No restart is required.

HTTP → HTTPS redirect is enabled automatically for ACME vhosts, with the challenge path exempted. Set `redirect_http: false` on the vhost to opt out.

---

## Issuers

An issuer is a named CA relationship: a directory URL, an account contact, and optionally a trust root. Each issuer has one ACME account, stored under `storage/<issuer>/account.json` and reused for every issuance. Reusing one account per issuer is what keeps Keel inside the CA's rate limits.

Vhosts select an issuer by name:

```yaml
acme:
  storage: /var/lib/keel/acme
  renew_before: 30%
  issuers:
    default:
      email: ops@example.com
    internal:
      email: infra@example.com
      directory: https://ca.corp.internal/acme/acme/directory
      root_ca: /etc/keel/corp-root.pem

vhosts:
  - host: www.example.com
    pool: web
    tls: { acme: true }           # issuer "default"

  - host: intranet.corp.internal
    pool: intranet
    tls: { acme: internal }       # named issuer
```

Config validation rejects three cases:

- `tls.acme` naming an issuer that is not defined under `acme.issuers`. Only `default` may be implicit.
- The same hostname assigned to two different issuers. A hostname belongs to exactly one issuer.
- A wildcard host (`*`) with ACME. HTTP-01 cannot issue wildcards.

### Issuer fields

| Field | Default | Notes |
|---|---|---|
| `email` | none | Account contact. Optional but recommended. |
| `directory` | `https://acme-v02.api.letsencrypt.org/directory` | Any ACME v2 directory URL. Staging: `https://acme-staging-v02.api.letsencrypt.org/directory` |
| `root_ca` | none | PEM trust root for the ACME API itself. Needed for internal or self-signed CAs; not for public ones. |
| `renew_before` | global value | Per-issuer renewal override. |

---

## Renewal

```yaml
acme:
  renew_before: 30%     # global default; issuers may override
```

`renew_before` decides when a certificate is renewed. It takes one of two forms:

- **Percentage** (`30%`) — renew when less than that share of the certificate's total lifetime remains. This scales with the CA's policy: a 90-day certificate renews with about 27 days left, a 6-day certificate with about 1.7 days left. This is the default.
- **Absolute** (`20d`) — renew when fewer than that many days remain. Simpler, but wrong for short-lived certificates. Prefer the percentage form.

The renewal check runs every 60 seconds, so a certificate is renewed within a minute of crossing its threshold. Renewed certificates are hot-swapped into the TLS listeners with no restart and no dropped connections.

Failed issuance retries with exponential backoff (1 minute, doubling to a 6-hour maximum) so a misconfigured domain cannot exhaust the CA's rate limits. Each failure is logged with its reason.

---

## Requirements

- **Port 80 must reach Keel** for the hostname being issued. The CA fetches `http://<host>/.well-known/acme-challenge/<token>`, and Keel answers on any plain (non-TLS) listener, ahead of redirects and vhost routing.
- **Resolvable DNS.** The CA resolves the hostname itself — public resolvers for a public CA, or whatever DNS an internal CA uses.
- **Wildcards need DNS-01.** An issuer with `challenge: dns-01` (below) can issue `*.example.com`; HTTP-01 cannot.

---

## DNS-01 challenge

An issuer with `challenge: dns-01` proves control of a name by publishing a
TXT record at `_acme-challenge.<host>` instead of answering on port 80. It
is the only challenge that can issue wildcard certificates, and it works
for hosts whose port 80 does not reach Keel.

Keel publishes the record with an RFC 2136 dynamic update signed with TSIG,
sent to the zone's primary server. BIND, Knot, PowerDNS, Windows DNS, and
most enterprise DNS accept these; no provider API is involved.

```yaml
acme:
  issuers:
    wild:
      email: ops@example.com
      challenge: dns-01
      dns:
        type: rfc2136
        server: ns1.example.com:53      # the primary that accepts updates
        zone: example.com
        tsig_name: keel-acme
        tsig_key_file: /etc/keel/keel-acme.key   # base64 secret; or tsig_key inline
        tsig_algorithm: hmac-sha256     # default; also hmac-sha384, hmac-sha512
        propagation_wait: 10s           # default
        ttl: 60                         # default

vhosts:
  - host: "*.example.com"
    pool: web
    tls: { acme: wild }
```

| Field | Default | Notes |
|---|---|---|
| `challenge` | `http-01` | `dns-01` requires `dns` |
| `dns.type` | required | `rfc2136` |
| `dns.server` | required | `host:port` of the primary. Updates and the self-check query go there over TCP |
| `dns.zone` | required | Zone named in the update. The challenge name must fall inside it |
| `dns.tsig_name` | required | Key name from the server's key statement |
| `dns.tsig_key` / `dns.tsig_key_file` | one required | Base64 secret, inline or in a file |
| `dns.tsig_algorithm` | `hmac-sha256` | `hmac-sha256`, `hmac-sha384`, or `hmac-sha512` |
| `dns.propagation_wait` | `10s` | Wait after the record is visible on the primary, for secondaries the CA may query |
| `dns.ttl` | `60` | TTL of the TXT record |

For BIND, the matching server side is a key statement and an update policy
that allows that key to change TXT records in the zone:

```
key "keel-acme" {
  algorithm hmac-sha256;
  secret "<base64>";
};
zone "example.com" {
  type primary;
  file "example.com.zone";
  update-policy { grant keel-acme zonesub TXT; };
};
```

`tsig-keygen keel-acme` prints a key statement with a fresh secret.

Issuance with DNS-01 proceeds as: the TXT record is added (any existing
TXT at the name is replaced); Keel queries the same server until the record
is visible, for up to 30 seconds; it waits `propagation_wait`; it tells the
CA to validate; after the order completes, pass or fail, the record is
deleted. A refused update is reported with the TSIG error when the server
sends one: `BADKEY` (unknown key name), `BADSIG` (secret mismatch),
`BADTIME` (clock skew beyond five minutes).

The update response itself is not authenticated; the self-check query is
what confirms the record exists on the server that was asked. Keel talks
only to the configured primary. If the CA's resolvers reach a secondary,
`propagation_wait` must cover zone transfer.

In cluster mode the leader publishes the record, as it runs the order.
Unlike HTTP-01 there is nothing to replicate: the record lives in DNS, not
on the nodes.

## Certificates for TCP / TLS-passthrough backends

Sometimes Keel is not the TLS terminator — a backend behind Keel terminates TLS itself (databases, TLS passthrough, plain TCP services). Those backends still need certificates, and Keel already owns port 80 for the domain. The top-level `certificates:` section covers this: Keel performs the HTTP-01 challenge and writes the certificate to disk without loading it into its own listeners.

```yaml
certificates:
  - host: db.example.com
    issuer: default        # optional; "default" when omitted
```

Because `certificates:` is a top-level list, like `vhosts:`, conf.d files can declare their own entries alongside their pools:

```yaml
# /etc/keel/conf.d/db-team.yaml
pools:
  db-frontend: { ... }

certificates:
  - host: db.example.com
```

Keel answers the challenge for `db.example.com` and writes:

```
/var/lib/keel/acme/db.example.com.crt    (0644)
/var/lib/keel/acme/db.example.com.key    (0600)
```

Point the backend at these files, or copy them out. Renewals rewrite the files atomically, so have the backend watch them or reload periodically.

The same entries serve TCP listeners with `tls_mode: terminate` or `reencrypt` ([TCP proxying](tcp-proxying.md)): the listener's `tls_host` names the entry, and Keel serves the issued certificate itself, renewing it in place. An entry can also point at files Keel does not issue:

```yaml
certificates:
  - host: ldap.example.com
    cert: /etc/keel/certs/ldap.example.com.crt
    key: /etc/keel/certs/ldap.example.com.key
```

Such an entry is not ACME-managed; `issuer` is ignored on it, and the files are re-read on SIGHUP.

---

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
└── db.example.com.crt / .key
```

Certificate files are flat (`{host}.crt` / `{host}.key`) regardless of issuer, because a hostname has exactly one certificate. Changing an issuer's `directory` registers a fresh account; existing certificates keep serving until their normal renewal.

---

## Restarts and persistence

Certificates persist on disk and are not re-issued on restart, reload, or reboot. On startup Keel loads whatever is in `storage`; the CA is contacted only when a certificate is missing, expired, unparsable, or inside its renewal window. A node that restarts while the CA is unreachable keeps serving its existing certificates.

---

## Cluster mode

In cluster mode certificates are replicated through the Raft log:

- Only the leader contacts the CAs. One account, one issuance, no duplicate certificates across nodes.
- Issued and renewed certificates are committed as Raft entries. Every node — including any node that joins later, via snapshot — writes them to its own `storage` and hot-swaps them into its listeners.
- On startup, and continuously afterwards, disk and Raft state are reconciled per hostname: the valid certificate with the most remaining lifetime wins and overwrites the other side. A full-cluster restart recovers certificates from disk, and the leader pushes them back into Raft.
- HTTP-01 challenge tokens are committed to the Raft log during issuance. The leader confirms every node holds the token before telling the CA to validate, so validation requests — which may come from multiple vantage points and land on any node — are answered wherever they arrive. Port 80 for the domain may reach any cluster node. Tokens are retracted from all nodes when the order completes.

### Issuance flow

The CA never pushes anything to Keel. Its only inbound request is the HTTP-01 validation `GET`; everything else happens over the leader's outbound connection to the CA's API.

1. The leader starts the order and receives the challenge token from the CA.
2. The token is committed to the Raft log; every node writes it to its challenge directory.
3. The leader confirms every node holds the token, then tells the CA to validate. The validation requests may land on any node — each serves the same token.
4. The leader polls the order until validation completes, submits the CSR, and downloads the certificate — all over its own connection to the CA. Which node answered the validation request plays no part here.
5. The challenge tokens are retracted from all nodes; the certificate is committed to the Raft log.
6. Every other node adopts the certificate and hot-swaps it into its TLS listeners on its next check cycle, within 60 seconds.

During step 6 the leader serves the new certificate while the other nodes still serve the previous one. For a renewal both are valid, so this is invisible. For a first issuance, nodes other than the leader cannot terminate TLS for the hostname until they adopt the certificate — up to a minute after the leader obtains it.

---

## How it works

Every worker process runs the ACME service, and an exclusive lock in the storage directory ensures only one worker talks to the CAs at a time. Challenge tokens are written as files, so any worker can answer the CA's validation request regardless of which worker started the order. Issued and renewed certificates are hot-swapped into the listeners within a minute — no restart, no dropped connections.

A request for `/.well-known/acme-challenge/<token>` where the token is unknown is proxied normally. A backend that manages its own ACME certificates behind Keel keeps working.

---

## Testing against a local CA

For local testing, point an issuer at a self-hosted ACME test server such as Pebble and supply its trust root:

```yaml
acme:
  storage: /tmp/keel-acme
  issuers:
    test:
      directory: https://localhost:14000/dir
      root_ca: /path/to/test-ca-root.pem

vhosts:
  - host: test.example
    pool: web
    tls: { acme: test }
```

`root_ca` makes Keel trust the test server's self-signed API certificate. It is never needed for a public CA.
