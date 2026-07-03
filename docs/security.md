# Security Hardening

This page documents the security hardening applied to Keel's alpha codebase: what
was exposed, what changed, and what operators need to do about it. Two of the
changes are breaking — see [Operator impact](#operator-impact) at the end.

---

## Cluster join exchange is encrypted

**Risk:** the join handshake between a new node and the bootstrap node runs over
plain TCP, *before* any mTLS identity exists. The response carries the cluster CA
certificate and the new node's freshly issued **private key**. Previously this
exchange was cleartext JSON, and the shared secret itself was transmitted inside
the request — anyone sniffing the network segment could capture a node private key,
the CA, and the cluster secret.

**Now:** both directions of the join exchange are AEAD-encrypted with
ChaCha20-Poly1305. The key is derived from the shared secret
(`SHA-256("keel-cluster-join-v1\0" + secret)`), and each message uses a fresh
random nonce. The secret is never sent on the wire — not even encrypted.
Successful decryption on the receiving side is itself proof that the peer holds
the secret, so a peer without it can neither read a captured exchange nor forge a
join request or response.

**Residual risk:** a captured exchange can be brute-forced offline against a
low-entropy secret. Use a high-entropy token:

```bash
keel --config keel.yaml --cluster --bootstrap --secret "$(openssl rand -hex 32)"
```

See [Cluster](cluster.md) for the full join flow.

---

## Cluster mode requires a shared secret

**Risk:** cluster mode would previously start with no secret at all. The join
listener then handed a CA-signed mTLS identity to any peer that could reach the
cluster port — a full cluster takeover with one TCP connection.

**Now:** Keel refuses to start `--cluster` mode (bootstrap or join) without a
non-empty secret from `--secret` or `cluster.secret` in `keel.yaml`.

---

## Cluster RPC reads are bounded

**Risk:** the length-prefixed wire protocol on the cluster port allocated a buffer
of whatever size the peer's 4-byte length header claimed — a remote peer could
drive multi-gigabyte allocations and take the node down (memory-exhaustion DoS).

**Now:** every length-prefixed read is capped before allocation:

| Channel | Limit |
|---|---|
| Join exchange (plain TCP, AEAD-encrypted) | 64 KiB |
| Raft RPC (mTLS: AppendEntries, Vote, InstallSnapshot) | 64 MiB |

Frames above the limit are rejected and the connection is dropped.

---

## Host header is no longer a filesystem or metrics key

**Risk:** the raw client-supplied `Host` header was used directly as the access
log filename (`access_<host>.log`) and as a Prometheus label. A crafted header
could:

- traverse outside the log directory (`Host: ../../etc/cron.d/x`),
- create unbounded log files, exhausting file descriptors and inodes,
- explode metric cardinality until Prometheus scrapes fall over.

**Now:** requests are mapped to a *bounded, operator-configured* vhost label
before logging or metric recording: the exact configured host, `"*"` if only a
wildcard vhost matches, or `"unmatched"`. The raw header never reaches the
filesystem or the metrics registry. As defense in depth, the access logger also
sanitizes the label to filesystem-safe characters before building a filename.

The original `Host` value is still forwarded upstream in `X-Forwarded-Host` —
backends see what the client sent; only Keel's internal keys are bounded.

---

## Metrics endpoint locked down

**Risk:** metrics expose backend addresses, pool and vhost names, and traffic
volumes — a network map of your infrastructure. The endpoint bound to
`0.0.0.0:9090` by default and answered every path and method.

**Now:**

- Default bind is `127.0.0.1:9090`. Remote scraping requires explicitly setting
  `metrics.address: 0.0.0.0:9090` (and firewalling the port), or running a local
  scrape agent against loopback.
- Only `GET /metrics` is served; any other method or path returns `404`.

---

## Control socket permissions restricted

**Risk:** anyone who can open the control socket owns the proxy — it can drain
backends, reload config, and push config to the entire cluster. The socket was
created with default (world-writable, umask-dependent) permissions.

**Now:** the socket directory is created `0750` *before* the socket is bound, and
the socket itself is set to `0660` (owner + group only). If the permissions cannot
be applied, Keel refuses to serve the control socket rather than run it open.

---

## Worker privilege drop is fail-closed

**Risk:** the privilege drop from root to `keel.user` / `keel.group` logged a
warning and *continued as root* if `setuid`/`setgid` failed or the configured
user didn't exist. Supplementary groups were never dropped, so workers kept
root's group memberships even after a successful `setuid`.

**Now:**

- The master resolves `keel.user` and `keel.group` *before forking any worker*,
  so a misconfigured name fails startup fast instead of fork/exit looping.
- Workers drop supplementary groups (`setgroups([])`), then gid, then uid — in
  that order — and **exit** if any step fails while running as root.
- After the drop, the worker verifies it is actually no longer root and exits if
  it somehow still is.
- Running unprivileged (typical in dev) skips the drop as before.

---

## Minimum TLS 1.2 on proxy listeners

**Risk:** TLS listeners negotiated whatever the OpenSSL default allowed,
including the obsolete TLS 1.0 and 1.1.

**Now:** every TLS listener sets a TLS 1.2 floor. TLS 1.0/1.1 handshakes are
rejected. This applies to all proxy listeners in both standalone and cluster
mode; cluster-internal mTLS already used rustls with modern defaults.

---

## Corrupt Raft snapshots surface as errors

**Risk:** a Raft snapshot that failed to deserialize was silently replaced with
`ClusterState::default()` — wiping the replicated config and drain map without
any signal to the operator, and letting a corrupted (or tampered) snapshot pass
as valid.

**Now:** snapshot deserialization failure is returned as a storage error, which
surfaces through openraft instead of silently resetting cluster state.

---

## Operator impact

Two changes are **breaking**:

| Change | Action required |
|---|---|
| Cluster mode refuses to start without a secret | Set `cluster.secret` in `keel.yaml` or pass `--secret`. Use a high-entropy token, e.g. `openssl rand -hex 32`. |
| Metrics default moved from `0.0.0.0:9090` to `127.0.0.1:9090` | To scrape from another host, set `metrics.address: 0.0.0.0:9090` explicitly and firewall the port. |

Also note:

- All nodes of a cluster must run the same build across a join: the join
  exchange is now encrypted, so a new node cannot join an old (cleartext)
  bootstrap node or vice versa.
- If workers previously "worked" as root because the `keel` user was missing,
  startup now fails with a clear error — create the user/group or set
  `keel.user` / `keel.group`.
