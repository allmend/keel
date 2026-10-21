# Security

What each measure is and what it protects against.

---

## Encrypted cluster join

The join handshake between a new node and the member it joins through runs over plain TCP, before any mTLS identity exists. Its response carries the cluster CA certificate and the new node's freshly issued private key, which must not travel in cleartext.

Both directions of the join exchange are AEAD-encrypted with ChaCha20-Poly1305. The key is derived from the shared secret (`SHA-256("keel-cluster-join-v1\0" + secret)`), and each message uses a fresh random nonce. The secret itself is never sent on the wire; successful decryption on the receiving side proves the peer holds it. A peer without the secret cannot read a captured exchange or forge a join request or response.

A captured exchange can be brute-forced offline against a low-entropy secret; `openssl rand -hex 32` gives a strong one.

---

## Peer identity

Node certificates are issued by the cluster CA and name the node: `CN=keel-node-<id>`. A request that names a sender (a Raft vote, replicated entries, a stepdown) is refused and logged unless the sender is the node in the certificate. One member's certificate cannot act for another member.

Every member holds the cluster CA key, in memory and in its Raft store, so any member can admit joiners. A compromised member exposes the key.

---

## Peers require a shared secret

A `cluster` block without a non-empty `secret` is a load error: the join listener would otherwise give cluster membership to any peer that reaches the port. A node without the block opens no peer port.

---

## Bounded cluster RPC reads

Every length-prefixed read on the cluster port is capped before allocation; a larger frame drops the connection. A peer cannot force large allocations through the length header.

| Channel | Limit |
|---|---|
| Join exchange (plain TCP, AEAD-encrypted) | 64 KiB |
| Raft RPC (mTLS: AppendEntries, Vote, InstallSnapshot) | 64 MiB |

---

## Host header bounded before use

Logs and metrics use the configured vhost name (`"*"`, a wildcard, or `"unmatched"`), never the client's `Host` header, and the access logger also sanitizes it to filesystem-safe characters. A crafted header cannot choose log file names (`Host: ../../etc/cron.d/x`), create files without bound, or add metric labels. Backends still get the original in `X-Forwarded-Host`.

---

## Metrics endpoint

Metrics expose backend addresses, pool and vhost names, and traffic volumes.

- The default bind is `127.0.0.1:10790`.
- Only `GET /metrics` is served; anything else gets `404`.

---

## Remote control requires mTLS

The remote control listener (`control.remote`, for [keelctl](keelctl.md)) accepts only clients presenting a certificate signed by the control CA. A connection without one fails at the TLS handshake. There is no password mode and no plaintext mode.

The control CA, key included, is replicated through Raft (over mTLS), kept in each node's Raft store and written to `control/ca.key` (0600) in `keel.state_dir`. Every node accepts the same keelconfigs and can issue them.

`allow` restricts source CIDRs on top of mTLS; source addresses are unreliable behind NAT or a Kubernetes Service.

Every remote command is logged with the certificate CN and source address. `keel credentials revoke-all` replaces the control CA on every node; every issued keelconfig stops working.

---

## Control socket permissions

Whoever opens the control socket controls the node and can push config to the cluster.

The master binds it as root, gives it to `keel.control_user` and that user's primary group, mode `0660`; workers are not in the group. If that fails, the master does not start.

Its directory is root's, mode `0755`: a worker cannot unlink the socket and put its own in its place. The workers create their sockets — `worker-<index>.sock` and the Pingora fd hand-off `upgrade-<index>.sock` — in a `workers/` subdirectory owned by `keel.user`, mode `0750`. The control worker reaches them through its membership of `keel.group`.

Every control connection's request line is capped (64 MiB, enough for a config push), and the `control.remote` listener serves at most 64 connections at a time, handshakes included.

---

## Processes and what each can read

No code in the root master accepts a connection. It binds the sockets, forks, supervises, and writes two messages to the control worker over a socketpair: `SIGHUP` arrived, and worker N was started. From its children it takes no data. The control worker can send three fixed words on the socketpair (`replace`, `node`, `upgrade`); anything else is dropped unread. On `replace` the master reads the version the control worker left in the state directory and validates it like any config; on `node` it reads `node.hcl`; on `upgrade` it runs the binary on disk. A new worker is ready when a marker file named after its PID exists; the master never reads the file.

| Process | User | Parses network input | Reads |
|---|---|---|---|
| master | root | no | `node.hcl`, the config directory, a version the control worker hands over for new workers |
| control worker | `keel.control_user` (default `keel-control`) | control socket, `control.remote`, Raft peers | the state directory: node key, Raft store with the cluster and control CA keys, ACME account and certificates |
| workers | `keel.user` | proxy listeners | nothing under the state directory |

Workers get what they serve from the control worker over their sockets: the applied config version (with `node.hcl`'s `cluster` block, and its secret, removed) and the ACME certificates. A worker compromised through traffic therefore holds site TLS keys, never a CA key: it cannot mint node certificates or operator credentials. `keel.control_user` and `keel.user` must differ; the same name is a load error.

| Path | Owner | Mode |
|---|---|---|
| runtime directory (`/var/run/keel`) | root | `0755` |
| `workers/` in it | `keel.user` | `0750` |
| `challenges/` in it (HTTP-01 tokens) | `keel.control_user`, group `keel.group` | `0750` |
| `keel.state_dir` (`/var/lib/keel`), `acme.storage` | `keel.control_user` | `0700` |
| `keel.config_dir` (`/etc/keel/config`) | `keel.control_user` | `0700` |
| `access_log.dir` | `keel.user` | as found |

A directory is chowned only when its owner is not the right user (a new directory, a root-owned volume); a group set afterwards is kept. A read-only config directory (a mounted ConfigMap) is served from; writing applied versions back to it fails and is logged.

---

## Minimum TLS 1.2 on proxy listeners

Every TLS listener rejects TLS 1.0 and 1.1.

---

## Corrupt Raft snapshots surface as errors

A Raft snapshot that does not deserialize is a storage error, never an empty state: a corrupt snapshot cannot silently wipe the replicated config and drains.

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

`/etc/keel/node.hcl` can hold the cluster secret; Keel never writes it.

---

## Users

Started as root, Keel needs `keel.user`, `keel.group` and `keel.control_user` (default `keel-control`) to exist; otherwise it does not start. A number is a uid or gid even without a passwd entry; a uid used that way takes the gid of the same number.

The container image carries `keel` (uid and gid 10001) and `keel-control` (10002) in its own `/etc/passwd` and `/etc/group`. Run as root, it drops privileges like any host. Run as non-root (`--user 10001`, Kubernetes `restricted`), every process keeps that one uid: nothing is dropped, and workers can read the control worker's state, CA keys included.
