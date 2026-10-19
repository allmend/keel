# ACME

Keel obtains and renews certificates over ACME v2 with the HTTP-01 challenge, from named issuers: any ACME v2 CA, one or several.

```hcl
acme {
  issuer "default" {
    email = "ops@example.com"
  }
}

vhost "example.com" {
  pool = "web"

  tls {
    acme = true
  }
}
```

`acme = true` uses the issuer `default`. Without an `acme` block, `default` is the public directory `https://acme-v02.api.letsencrypt.org/directory` without contact email.

Keel registers an account, answers the challenge on port 80, obtains the certificate and serves it. ACME vhosts redirect plain HTTP to HTTPS, except the challenge path; `redirect_http = false` turns that off.

## Issuers

Each issuer has one ACME account, `storage/<issuer>/account.json`, used for every issuance; in a cluster, by every node.

```hcl
acme {
  storage      = "/var/lib/keel/acme"
  renew_before = "30%"

  issuer "default" {
    email = "ops@example.com"
  }

  issuer "internal" {
    directory = "https://ca.corp.internal/acme/acme/directory"
    root_ca   = "certs/corp-root.pem"
  }
}

vhost "intranet.corp.internal" {
  pool = "intranet"

  tls {
    acme = "internal"
  }
}
```

| Attribute | Default | Notes |
|---|---|---|
| `email` | none | Account contact |
| `directory` | `https://acme-v02.api.letsencrypt.org/directory` | ACME v2 directory URL |
| `root_ca` | none | PEM trust root for the ACME API. A relative path is a file of the config directory |
| `renew_before` | the `acme` block's | Per-issuer override |

Load errors:

- `acme = "<name>"` for an issuer that is not defined; only `default` may be implicit.
- One host with two issuers.
- ACME on a wildcard host: HTTP-01 cannot issue wildcards. A wildcard vhost uses `tls.cert` and `tls.key`.

## Renewal

`renew_before` (default `30%`) is a share of the certificate's lifetime (`30%`: a 90-day certificate renews with 27 days left) or a duration (`20d`).

Renewal is checked every 60 seconds. A renewed certificate is served from the next handshake. A failed issuance is logged and retried after 1 minute, doubling up to 6 hours.

## Requirements

- Port 80 for the host reaches Keel. The CA fetches `http://<host>/.well-known/acme-challenge/<token>`; Keel answers on every `http` listener, before redirects and routing. A token Keel does not know is proxied like any request.
- The CA can resolve the host.

## Certificates without a vhost

A `certificate` block gets a certificate for a host Keel does not terminate HTTP for: a TCP listener, or a backend that terminates TLS itself.

```hcl
certificate "db.example.com" {
  issuer = "default"
}
```

Keel writes `db.example.com.crt` (0644) and `db.example.com.key` (0600) to `acme.storage` and rewrites them atomically on renewal. A `tcp` listener with `tls_mode = "terminate"` or `"reencrypt"` serves it when its `tls_host` names the block.

A block with `cert` and `key` loads those files instead of issuing; `issuer` is ignored, and the files are re-read with every applied version:

```hcl
certificate "ldap.example.com" {
  cert = "/etc/keel/certs/ldap.example.com.crt"
  key  = "/etc/keel/certs/ldap.example.com.key"
}
```

`certificate` blocks may be in any file of the config directory.

## Storage

```
/var/lib/keel/acme/               acme.storage, 0700
├── default/account.json          one account per issuer, 0600
├── internal/account.json
├── www.example.com.crt           certificate chain
├── www.example.com.key           0600
└── db.example.com.crt, .key
```

One certificate per host, whatever its issuer. `acme.storage` is absolute and outside the config directory. Challenge tokens are in `challenges/` in the runtime directory (`/var/run/keel/challenges`). A changed `directory` registers a new account.

Certificates on disk are reused across restarts; the CA is contacted only for a certificate that is missing, expired, unreadable or due. A reload or push applies ACME changes at once: a new host is issued, a vhost switched from its own certificate to ACME serves the old one until the new one arrives, and a host switched away keeps its files in `storage`.

## Cluster

- Only the leader talks to the CA.
- An issuer's account is committed to Raft when first registered; every node writes it, and a new leader reuses it.
- Issued certificates are committed to Raft; every node writes and serves them, nodes that join later included.
- Disk and Raft are reconciled per host: the valid certificate with the longest remaining lifetime wins.
- Challenge tokens are committed to Raft. The leader asks the CA to validate only once every node holds the token, so any node can answer the validation request.

The leader serves a new certificate at once; the other nodes within 60 seconds. For a first issuance, other nodes cannot serve TLS for the host until then.

## Processes

The control worker runs ACME and is the only process that reads `acme.storage`. It writes challenge tokens to the runtime directory, which every worker answers from, and sends certificates to the workers.

## Testing against a local CA

```hcl
acme {
  storage = "/tmp/keel-acme"

  issuer "test" {
    directory = "https://localhost:14000/dir"
    root_ca   = "/path/to/test-ca-root.pem"
  }
}
```

`root_ca` is the test CA's API certificate (Pebble's, for example).
