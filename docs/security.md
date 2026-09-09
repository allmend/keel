# Security

This page describes the security properties of Keel's proxy and cluster code: what each measure is and what it protects against.

---

## Encrypted cluster join

The join handshake between a new node and the bootstrap node runs over plain TCP, before any mTLS identity exists. Its response carries the cluster CA certificate and the new node's freshly issued private key, which must not travel in cleartext.

Both directions of the join exchange are AEAD-encrypted with ChaCha20-Poly1305. The key is derived from the shared secret (`SHA-256("keel-cluster-join-v1\0" + secret)`), and each message uses a fresh random nonce. The secret itself is never sent on the wire; successful decryption on the receiving side proves the peer holds it. A peer without the secret cannot read a captured exchange or forge a join request or response.

A captured exchange can still be brute-forced offline against a low-entropy secret, so use a high-entropy token:

```bash
keel --config keel.yaml --cluster --bootstrap --secret "$(openssl rand -hex 32)"
```

See [Cluster](cluster.md) for the full join flow.

---

## Cluster mode requires a shared secret

Keel refuses to start `--cluster` mode, bootstrap or join, without a non-empty secret from `--secret` or `cluster.secret` in `keel.yaml`.

Without it, the join listener would issue a CA-signed mTLS identity to any peer that can reach the cluster port, granting that peer full cluster membership.

---

## Bounded cluster RPC reads

The cluster port uses a length-prefixed wire protocol. Every length-prefixed read is capped before allocation; frames above the limit are rejected and the connection is dropped. A remote peer cannot drive large allocations by claiming an arbitrary size in the 4-byte length header.

| Channel | Limit |
|---|---|
| Join exchange (plain TCP, AEAD-encrypted) | 64 KiB |
| Raft RPC (mTLS: AppendEntries, Vote, InstallSnapshot) | 64 MiB |

---

## Host header bounded before use

Requests are mapped to a bounded, operator-configured vhost label before anything is logged or recorded: the exact configured host, `"*"` if only a wildcard vhost matches, or `"unmatched"`. The raw client-supplied `Host` header never reaches the filesystem or the metrics registry, and the access logger sanitizes the label to filesystem-safe characters as a second line of defense.

This bounds three things that would otherwise be driven by a crafted header: access-log filenames (no path traversal, e.g. `Host: ../../etc/cron.d/x`), the number of log files created (no file-descriptor or inode exhaustion), and metric label cardinality.

The original `Host` value is still forwarded upstream in `X-Forwarded-Host`, so backends see what the client sent. Only Keel's internal keys are bounded.

---

## Metrics endpoint

Metrics expose backend addresses, pool and vhost names, and traffic volumes. Access is restricted by default to limit what this reveals about the infrastructure.

- The default bind is `127.0.0.1:9090`. To scrape from another host, set `metrics.address: 0.0.0.0:9090` and firewall the port, or run a local scrape agent against loopback.
- Only `GET /metrics` is served. Any other method or path returns `404`.

---

## Remote control requires mTLS

The remote control listener (`control.remote`, for [keelctl](keelctl.md)) accepts only clients presenting a certificate signed by the node's control CA. A connection without one fails at the TLS handshake. There is no password mode and no plaintext mode.

The optional `allow:` list additionally restricts accepted source CIDRs. Source addresses are not reliable behind NAT or a Kubernetes Service, so the restriction narrows exposure but does not replace mTLS.

Every remote command is audit-logged with the client certificate's CN and source address. To invalidate all issued credentials, delete `control.remote.ca_dir` and restart — a new CA is generated and every existing keelconfig stops working.

---

## Control socket permissions

Anyone who can open the control socket controls the proxy: draining backends, reloading config, pushing config to the whole cluster.

The socket directory is created `0750` before the socket is bound, and the socket itself is set to `0660` (owner and group only). If those permissions cannot be applied, Keel refuses to serve the control socket rather than run it open.

---

## Fail-closed privilege drop

Workers drop from root to `keel.user` / `keel.group` and exit rather than continue as root if any step fails:

- The master resolves `keel.user` and `keel.group` before forking any worker, so a misconfigured name fails startup immediately instead of fork/exit looping.
- On Linux, the master binds every listener (TCP and UDP) while still root and the workers inherit the sockets across `fork`. No worker ever holds `CAP_NET_BIND_SERVICE` or binds a port below 1024 itself.
- Each worker drops supplementary groups (`setgroups([])`), then gid, then uid, in that order, and exits if any step fails while running as root.
- After the drop, the worker confirms it is no longer root and exits if it somehow still is.
- A process already running unprivileged (typical in dev) skips the drop.

---

## Minimum TLS 1.2 on proxy listeners

Every TLS listener sets a TLS 1.2 floor and rejects TLS 1.0/1.1 handshakes. This applies to all proxy listeners in standalone and cluster mode. Cluster-internal mTLS uses modern defaults as well.

---

## Corrupt Raft snapshots surface as errors

A Raft snapshot that fails to deserialize is returned as a storage error and surfaces to the operator. It is not silently replaced with an empty state, so a corrupt or tampered snapshot cannot wipe the replicated config and drain map unnoticed.

---

## Operator checklist

| Requirement | Action |
|---|---|
| Cluster mode needs a secret | Set `cluster.secret` in `keel.yaml` or pass `--secret`. Use a high-entropy token, e.g. `openssl rand -hex 32`. |
| Metrics bind to loopback by default | To scrape from another host, set `metrics.address: 0.0.0.0:9090` explicitly and firewall the port. |
| Workers need a user to drop to | Startup fails if `keel.user` / `keel.group` cannot be resolved. Create the user and group, or point the fields at an existing account. |
| All cluster nodes must speak the same join protocol | Run the same Keel build across the cluster when joining nodes. |
