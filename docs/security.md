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

The remote control listener (`control.remote`, for [keelctl](keelctl.md)) accepts only clients presenting a certificate signed by the control CA. A connection without one fails at the TLS handshake. There is no password mode and no plaintext mode.

In cluster mode the control CA, private key included, is replicated through the Raft log so every node authenticates the same keelconfigs. The log travels only over the cluster's mTLS mesh and is held in memory; on each node the key is written to `ca_dir/ca.key` with mode 0600, the same as a locally generated one. Any node can therefore issue operator credentials, which is the intended property: a node with control-plane access is already trusted with the whole cluster's configuration.

The optional `allow:` list additionally restricts accepted source CIDRs. Source addresses are not reliable behind NAT or a Kubernetes Service, so the restriction narrows exposure but does not replace mTLS.

Every remote command is audit-logged with the client certificate's CN and source address. To invalidate all issued credentials, delete `control.remote.ca_dir` and restart — a new CA is generated and every existing keelconfig stops working.

---

## Control socket permissions

Anyone who can open the control socket controls the proxy: draining backends, reloading config, pushing config to the whole cluster.

The socket directory is created `0750` before the socket is bound, and the socket itself is set to `0660` (owner and group only). If those permissions cannot be applied, Keel refuses to serve the control socket rather than run it open.

The master binds the socket while still root and then hands it to `keel.user` and `keel.group`, so reaching it requires that group rather than root. The directory holding it stays root-owned (`0750`, group `keel.group`): a process that can write a directory can unlink what is in it, so a worker-writable directory would let a compromised worker replace the master's socket and answer operator commands in its place. The workers create their own sockets — `worker-<index>.sock` and the Pingora fd hand-off `upgrade-<index>.sock` — in a `workers/` subdirectory that does belong to `keel.user`.

---

## The remote control listener runs as root

This one is a known deviation from the process model, recorded here rather than glossed over.

Keel's design says the root master binds ports, spawns workers, and never touches request data. The `control.remote` mTLS listener breaks that: the master owns it, so TLS handshakes, client-certificate verification and JSON command parsing for a network-facing port all happen in the privileged process — the same one that holds the listening sockets and runs the fork loop. The control CA in `ca_dir` is read by root as well.

It moved there for correctness. When each worker bound the port, every worker but one failed with `control: remote listener failed`, and the one that won could only answer for itself — it had no view of the other workers' connection counts, health or drain state. Answering for the instance requires the process that knows about all the workers, and that is the master.

The exposure is bounded by mTLS: a client certificate signed by the control CA is required before any command is parsed, and `control.remote.allow` can restrict source ranges on top. But certificate verification itself is attacker-reachable code running as root, which the worker model was built to avoid.

If that trade does not suit your deployment, leave `control.remote` unset. The local Unix socket is unaffected and runs the same protocol; `keelctl` then needs a shell on the node, as it did before remote control existed.

A better shape — a dedicated unprivileged control process that fans out to the workers, closer to how nginx keeps its master free of request handling — is open for discussion rather than settled.

---

## Fail-closed privilege drop

Workers drop from root to `keel.user` / `keel.group` and exit rather than continue as root if any step fails:

- The master resolves `keel.user` and `keel.group` before forking any worker, so a misconfigured name fails startup immediately instead of fork/exit looping.
- On Linux, the master binds every listener (TCP and UDP) while still root and the workers inherit the sockets across `fork`. No worker ever holds `CAP_NET_BIND_SERVICE` or binds a port below 1024 itself.
- The same applies to the ICMP datagram sockets used by `icmp` health checks: opened by the master (which has `CAP_NET_RAW`), inherited by the workers. Keel never opens raw sockets.
- Each worker drops supplementary groups (`setgroups([])`), then gid, then uid, in that order, and exits if any step fails while running as root.
- After the drop, the worker confirms it is no longer root and exits if it somehow still is.
- A process already running unprivileged (typical in dev) skips the drop.
- Before dropping, the root process creates the directories Keel writes to afterwards — the control-socket directory, the control CA directory, and the ACME storage — and assigns them to `keel.user`, so a root-owned default such as `/var/lib/keel` in the container image does not break them.
- Cluster mode is a single process without a master. Started as root on Linux, it binds its listeners (and ICMP sockets) itself, drops to `keel.user`, and then starts the data plane, the Raft peer listener, and the control plane unprivileged.

---

## Minimum TLS 1.2 on proxy listeners

Every TLS listener sets a TLS 1.2 floor and rejects TLS 1.0/1.1 handshakes. This applies to all proxy listeners in standalone and cluster mode. Cluster-internal mTLS uses modern defaults as well.

---

## Gateway rules fail closed

- A `rate_limit` or `auth` rule answers before any backend is selected, so a refused request costs no upstream connection.
- JWT: the accepted algorithms follow from the configured key — a shared secret accepts only `HS*`, a public key only `RS*`/`ES*` — so a token cannot choose an algorithm the operator did not intend, and `alg: none` is never accepted. `exp` is required. A key file that parses incorrectly makes the rule refuse every request rather than pass them.
- `claim_headers` are removed from every incoming request before verified values are inserted, so a backend can trust them regardless of what the client sent.
- Rate-limit buckets are per worker process; see [API gateway](gateway.md#rate-limiting) for what that means for the effective limit.

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
