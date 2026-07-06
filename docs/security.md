# Security

This page describes the security properties of Keel's proxy and cluster code: what each measure is and what it protects against.

---

## Encrypted cluster join

The join handshake between a new node and the bootstrap node runs over plain TCP, before any mTLS identity exists. Its response carries the cluster CA certificate and the new node's freshly issued private key — material that must never travel in cleartext.

Both directions of the join exchange are AEAD-encrypted with ChaCha20-Poly1305. The key is derived from the shared secret (`SHA-256("keel-cluster-join-v1\0" + secret)`), and each message uses a fresh random nonce. The secret is never sent on the wire, not even encrypted — successful decryption on the receiving side is itself proof that the peer holds it. A peer without the secret can neither read a captured exchange nor forge a join request or response.

A captured exchange can still be brute-forced offline against a low-entropy secret, so use a high-entropy token:

```bash
keel --config keel.yaml --cluster --bootstrap --secret "$(openssl rand -hex 32)"
```

See [Cluster](cluster.md) for the full join flow.

---

## Cluster mode requires a shared secret

Keel refuses to start `--cluster` mode, bootstrap or join, without a non-empty secret from `--secret` or `cluster.secret` in `keel.yaml`.

Without this requirement, the join listener would hand a CA-signed mTLS identity to any peer that can reach the cluster port — a full cluster takeover from a single TCP connection.

---

## Bounded cluster RPC reads

The cluster port uses a length-prefixed wire protocol. Every length-prefixed read is capped before allocation; frames above the limit are rejected and the connection is dropped. A remote peer cannot drive large allocations by claiming an arbitrary size in the 4-byte length header.

| Channel | Limit |
|---|---|
| Join exchange (plain TCP, AEAD-encrypted) | 64 KiB |
| Raft RPC (mTLS: AppendEntries, Vote, InstallSnapshot) | 64 MiB |

---

## Host header is not a filesystem or metrics key

Requests are mapped to a bounded, operator-configured vhost label before anything is logged or recorded: the exact configured host, `"*"` if only a wildcard vhost matches, or `"unmatched"`. The raw client-supplied `Host` header never reaches the filesystem or the metrics registry, and the access logger sanitizes the label to filesystem-safe characters as a second line of defense.

This closes three attacks from a single crafted header: path traversal in access-log filenames (`Host: ../../etc/cron.d/x`), file-descriptor and inode exhaustion through unbounded log file creation, and metric cardinality explosion.

The original `Host` value is still forwarded upstream in `X-Forwarded-Host`, so backends see what the client sent. Only Keel's internal keys are bounded.

---

## Metrics endpoint locked down

Metrics reveal backend addresses, pool and vhost names, and traffic volumes — effectively a network map of the infrastructure.

- The default bind is `127.0.0.1:9090`. Remote scraping requires setting `metrics.address: 0.0.0.0:9090` explicitly and firewalling the port, or running a local scrape agent against loopback.
- Only `GET /metrics` is served. Any other method or path returns `404`.

---

## Control socket permissions

Anyone who can open the control socket controls the proxy — draining backends, reloading config, pushing config to the whole cluster.

The socket directory is created `0750` before the socket is bound, and the socket itself is set to `0660` (owner and group only). If those permissions cannot be applied, Keel refuses to serve the control socket rather than run it open.

---

## Fail-closed privilege drop

Workers drop from root to `keel.user` / `keel.group` and exit rather than continue as root if any step fails:

- The master resolves `keel.user` and `keel.group` before forking any worker, so a misconfigured name fails startup immediately instead of fork/exit looping.
- Each worker drops supplementary groups (`setgroups([])`), then gid, then uid, in that order, and exits if any step fails while running as root.
- After the drop, the worker confirms it is no longer root and exits if it somehow still is.
- A process already running unprivileged (typical in dev) skips the drop.

---

## Minimum TLS 1.2 on proxy listeners

Every TLS listener sets a TLS 1.2 floor and rejects TLS 1.0/1.1 handshakes. This applies to all proxy listeners in standalone and cluster mode. Cluster-internal mTLS uses modern defaults as well.

---

## Corrupt Raft snapshots surface as errors

A Raft snapshot that fails to deserialize is returned as a storage error and surfaces to the operator. It is never silently replaced with an empty state — a corrupt or tampered snapshot cannot wipe the replicated config and drain map unnoticed.

---

## Operator checklist

| Requirement | Action |
|---|---|
| Cluster mode needs a secret | Set `cluster.secret` in `keel.yaml` or pass `--secret`. Use a high-entropy token, e.g. `openssl rand -hex 32`. |
| Metrics bind to loopback by default | To scrape from another host, set `metrics.address: 0.0.0.0:9090` explicitly and firewall the port. |
| Workers need a user to drop to | Startup fails if `keel.user` / `keel.group` cannot be resolved. Create the user and group, or point the fields at an existing account. |
| All cluster nodes must speak the same join protocol | Run the same Keel build across the cluster when joining nodes. |
