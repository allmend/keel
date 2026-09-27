# Security

This page describes the security properties of Keel's proxy and cluster code: what each measure is and what it protects against.

---

## Encrypted cluster join

The join handshake between a new node and the member it joins through runs over plain TCP, before any mTLS identity exists. Its response carries the cluster CA certificate and the new node's freshly issued private key, which must not travel in cleartext.

Both directions of the join exchange are AEAD-encrypted with ChaCha20-Poly1305. The key is derived from the shared secret (`SHA-256("keel-cluster-join-v1\0" + secret)`), and each message uses a fresh random nonce. The secret itself is never sent on the wire; successful decryption on the receiving side proves the peer holds it. A peer without the secret cannot read a captured exchange or forge a join request or response.

A captured exchange can still be brute-forced offline against a low-entropy secret, so use a high-entropy token:

```bash
openssl rand -hex 32    # for cluster.secret in node.yaml
```

See [Cluster](cluster.md) for the full join flow.

---

## Peer identity

Node certificates are issued by the cluster CA and name the node: `CN=keel-node-<id>`. A request that names a sender (a Raft vote, replicated entries, a stepdown) is refused and logged unless the sender is the node in the certificate. One member's certificate cannot act for another member.

Every member holds the cluster CA key, in memory and in its Raft store, so any member can admit joiners. A compromised member exposes the key.

---

## Peers require a shared secret

A `node.yaml` with a `cluster:` section and no non-empty `cluster.secret` is a load error. A node without the section opens no peer port.

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

- The default bind is `127.0.0.1:10790`. To scrape from another host, set `metrics.address: 0.0.0.0:10790` and firewall the port, or run a local scrape agent against loopback.
- Only `GET /metrics` is served. Any other method or path returns `404`.

---

## Remote control requires mTLS

The remote control listener (`control.remote`, for [keelctl](keelctl.md)) accepts only clients presenting a certificate signed by the control CA. A connection without one fails at the TLS handshake. There is no password mode and no plaintext mode.

The control CA, private key included, is replicated through the Raft log so every node authenticates the same keelconfigs. The log travels only over the cluster's mTLS mesh and is stored in each node's Raft store under `keel.state_dir`; on each node the key is also written to `control/ca.key` under `keel.state_dir` with mode 0600, the same as a locally generated one. Any node can therefore issue operator credentials, which is the intended property: a node with control-plane access is already trusted with the whole cluster's configuration.

The optional `allow:` list additionally restricts accepted source CIDRs. Source addresses are not reliable behind NAT or a Kubernetes Service, so the restriction narrows exposure but does not replace mTLS.

Every remote command is audit-logged with the client certificate's CN and source address. To invalidate all issued credentials, run `keel credentials revoke-all`: the control CA is replaced — on every node of a cluster — and every existing keelconfig stops working.

---

## Control socket permissions

Anyone who can open the control socket controls the proxy: draining backends, pushing config to the whole cluster.

The master binds the socket as root, gives it to `keel.control_user` and that user's primary group, and sets `0660`. Operators reach it through that group; workers are not in it. If the ownership or mode cannot be applied, the master does not start.

The directory holding it is owned by root, mode `0755`. Write permission on a directory allows unlinking its entries, so a worker-writable directory would let a compromised worker remove the socket, bind its own in that path, and answer operator commands. The workers create their sockets — `worker-<index>.sock` and the Pingora fd hand-off `upgrade-<index>.sock` — in a `workers/` subdirectory owned by `keel.user`, mode `0750`. The control worker reaches them through its membership of `keel.group`.

Every control connection's request line is capped (64 MiB, enough for a config push), and the `control.remote` listener serves at most 64 connections at a time, handshakes included.

---

## Processes and what each can read

No code in the root master accepts a connection or reads from one. It binds the sockets, forks, supervises, and writes two messages to the control worker over a socketpair: `SIGHUP` arrived, and worker N was restarted.

| Process | User | Parses network input | Reads |
|---|---|---|---|
| master | root | no | `node.yaml`, the config directory |
| control worker | `keel.control_user` (default `keel-control`) | control socket, `control.remote`, Raft peers | the state directory: node key, Raft store with the cluster and control CA keys, ACME account and certificates |
| workers | `keel.user` | proxy listeners | nothing under the state directory |

Workers get what they serve from the control worker over their sockets: the applied config version (with `node.yaml`'s `cluster:` section, and its secret, removed) and the ACME certificates. A worker compromised through traffic therefore holds site TLS keys, never a CA key: it cannot mint node certificates or operator credentials. `keel.control_user` and `keel.user` must differ; the same name is a load error.

| Path | Owner | Mode |
|---|---|---|
| runtime directory (`/var/run/keel`) | root | `0755` |
| `workers/` in it | `keel.user` | `0750` |
| `challenges/` in it (HTTP-01 tokens) | `keel.control_user`, group `keel.group` | `0750` |
| `keel.state_dir` (`/var/lib/keel`), `acme.storage` | `keel.control_user` | `0700` |
| `keel.config_dir` (`/etc/keel/config`) | `keel.control_user` | `0700` |
| `access_log.dir` | `keel.user` | as found |

A directory is chowned only when its owner is not already the right user, e.g. a new directory or a root-owned volume; a group set afterwards is kept. A state or config directory an earlier version gave to `keel.user` moves to `keel.control_user` at the next start. A read-only config directory (a mounted ConfigMap) is served from; writing applied versions back to it fails and is logged.

---

## Minimum TLS 1.2 on proxy listeners

Every TLS listener sets a TLS 1.2 floor and rejects TLS 1.0/1.1 handshakes. This applies to all proxy listeners. Cluster-internal mTLS uses modern defaults as well.

---

## Corrupt Raft snapshots surface as errors

A Raft snapshot that fails to deserialize is returned as a storage error and surfaces to the operator. It is not silently replaced with an empty state, so a corrupt or tampered snapshot cannot wipe the replicated config and drain map unnoticed.

---

## Where secrets are stored

Files that can hold a private key are `0600` in a `0700` directory:

| Path | Holds |
|---|---|
| `keel.state_dir` (`/var/lib/keel`) | `node.key`, the node's mTLS key |
| `raft/store.redb` in the state directory | cluster and control CA keys, ACME account and certificate keys, pushed config including its keys |
| `control/` in the state directory | the control CA key, which signs operator credentials |
| `acme.storage` (`/var/lib/keel/acme`) | issued certificate keys, ACME account keys |
| the config directory | keys the config names by relative path |

Exclude these from backups, or back them up as secrets. `/etc/keel/node.yaml` can hold the cluster secret; Keel never writes it.

Ownership of these directories: [Processes and what each can read](#processes-and-what-each-can-read).

---

## Operator checklist

| Requirement | Action |
|---|---|
| Peers need a secret | Set `cluster.secret` in `node.yaml`. Use a high-entropy token, e.g. `openssl rand -hex 32`. |
| Metrics bind to loopback by default | To scrape from another host, set `metrics.address: 0.0.0.0:10790` explicitly and firewall the port. |
| Two users | Startup fails if `keel.user`, `keel.group` or `keel.control_user` (default `keel-control`) cannot be resolved. Create them, e.g. `useradd --system keel-control`. |
| All cluster nodes must speak the same join protocol | Run the same Keel build across the cluster when joining nodes. |
