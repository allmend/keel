# Security Hardening

This page describes the security measures applied to Keel's cluster and proxy code: what each one protects against and what it means for operators. Two of them are breaking changes — see [Operator impact](#operator-impact) at the end.

---

## Encrypted cluster join

The join handshake between a new node and the bootstrap node runs over plain TCP, before any mTLS identity exists. Its response carries the cluster CA certificate and the new node's freshly issued private key. If that exchange were cleartext, anyone able to sniff the network segment could capture a node private key, the CA, and the shared secret.

Both directions of the join exchange are AEAD-encrypted with ChaCha20-Poly1305. The key is derived from the shared secret (`SHA-256("keel-cluster-join-v1\0" + secret)`), and each message uses a fresh random nonce. The secret is never sent on the wire, not even encrypted — successful decryption on the receiving side is itself proof that the peer holds it. A peer without the secret can neither read a captured exchange nor forge a join request or response.

A captured exchange can still be brute-forced offline against a low-entropy secret, so use a high-entropy token:

```bash
keel --config keel.yaml --cluster --bootstrap --secret "$(openssl rand -hex 32)"
```

See [Cluster](cluster.md) for the full join flow.

---

## Cluster mode requires a shared secret

Cluster mode used to start with no secret at all. The join listener would then hand a CA-signed mTLS identity to any peer that could reach the cluster port — a full takeover from a single TCP connection.

Keel now refuses to start `--cluster` mode, bootstrap or join, without a non-empty secret from `--secret` or `cluster.secret` in `keel.yaml`.

---

## Bounded cluster RPC reads

The length-prefixed wire protocol on the cluster port allocated a buffer of whatever size the peer's 4-byte length header claimed, so a remote peer could drive multi-gigabyte allocations and take the node down.

Every length-prefixed read is now capped before allocation. Frames above the limit are rejected and the connection is dropped.

| Channel | Limit |
|---|---|
| Join exchange (plain TCP, AEAD-encrypted) | 64 KiB |
| Raft RPC (mTLS: AppendEntries, Vote, InstallSnapshot) | 64 MiB |

---

## Host header is not a filesystem or metrics key

The raw client-supplied `Host` header was used directly as the access log filename (`access_<host>.log`) and as a Prometheus label. A crafted header could traverse outside the log directory (`Host: ../../etc/cron.d/x`), create unbounded log files until file descriptors or inodes ran out, or explode metric cardinality until Prometheus scrapes failed.

Requests are now mapped to a bounded, operator-configured vhost label before anything is logged or recorded: the exact configured host, `"*"` if only a wildcard vhost matches, or `"unmatched"`. The raw header never reaches the filesystem or the metrics registry, and the access logger sanitizes the label to filesystem-safe characters as a second line of defense.

The original `Host` value is still forwarded upstream in `X-Forwarded-Host`, so backends see what the client sent. Only Keel's internal keys are bounded.

---

## Metrics endpoint locked down

Metrics reveal backend addresses, pool and vhost names, and traffic volumes — effectively a network map of the infrastructure. The endpoint used to bind to `0.0.0.0:9090` and answer every path and method.

- The default bind is now `127.0.0.1:9090`. Remote scraping requires setting `metrics.address: 0.0.0.0:9090` explicitly and firewalling the port, or running a local scrape agent against loopback.
- Only `GET /metrics` is served. Any other method or path returns `404`.

---

## Control socket permissions

Anyone who can open the control socket controls the proxy — draining backends, reloading config, pushing config to the whole cluster. The socket was created with default, umask-dependent permissions.

The socket directory is now created `0750` before the socket is bound, and the socket itself is set to `0660` (owner and group only). If those permissions cannot be applied, Keel refuses to serve the control socket rather than run it open.

---

## Fail-closed privilege drop

The drop from root to `keel.user` / `keel.group` used to log a warning and continue as root if `setuid` or `setgid` failed, or if the configured user did not exist. Supplementary groups were never dropped, so a worker kept root's group memberships even after a successful `setuid`.

- The master resolves `keel.user` and `keel.group` before forking any worker, so a misconfigured name fails startup immediately instead of fork/exit looping.
- Each worker drops supplementary groups (`setgroups([])`), then gid, then uid, in that order, and exits if any step fails while running as root.
- After the drop, the worker confirms it is no longer root and exits if it somehow still is.
- A process already running unprivileged (typical in dev) skips the drop, as before.

---

## Minimum TLS 1.2 on proxy listeners

TLS listeners used to negotiate whatever the OpenSSL default allowed, including the obsolete TLS 1.0 and 1.1.

Every TLS listener now sets a TLS 1.2 floor and rejects 1.0/1.1 handshakes. This applies to all proxy listeners in standalone and cluster mode; cluster-internal mTLS already used rustls with modern defaults.

---

## Corrupt Raft snapshots surface as errors

A Raft snapshot that failed to deserialize was silently replaced with `ClusterState::default()`, wiping the replicated config and drain map with no signal to the operator and letting a corrupt or tampered snapshot pass as valid.

Snapshot deserialization failure is now returned as a storage error, which surfaces through openraft instead of resetting cluster state.

---

## Operator impact

Two changes are breaking:

| Change | Action required |
|---|---|
| Cluster mode refuses to start without a secret | Set `cluster.secret` in `keel.yaml` or pass `--secret`. Use a high-entropy token, e.g. `openssl rand -hex 32`. |
| Metrics default moved from `0.0.0.0:9090` to `127.0.0.1:9090` | To scrape from another host, set `metrics.address: 0.0.0.0:9090` explicitly and firewall the port. |

Two more things to be aware of:

- Every node in a cluster must run the same build across a join. The join exchange is encrypted now, so a new node cannot join an old cleartext bootstrap node, or the reverse.
- If workers previously ran as root because the `keel` user was missing, startup now fails with a clear error. Create the user and group, or set `keel.user` / `keel.group`.
