# Cluster

## Standalone vs cluster

Standalone mode is a first-class deployment target. A single Keel node reads its config from a local YAML file, has no cluster overhead, and supports all features including caching, TLS, health checks, and drain.

Use cluster mode when you need:
- Fault tolerance — traffic continues if a node fails
- Coordinated config changes across nodes
- Distributed drain — drain a backend on all nodes simultaneously

---

## Node count and quorum

| Nodes | Failure tolerance | Write quorum | Notes |
|---|---|---|---|
| 1 | None | N/A | Standalone; no cluster overhead |
| 2 | 0 | Leader only | Leader-follower; see below |
| 3 | 1 | 2 of 3 | Minimum for full HA |
| 5 | 2 | 3 of 5 | Recommended for production |
| 7 | 3 | 4 of 7 | High availability with larger failure budget |

Odd node counts are recommended. Even counts work with understood trade-offs.

### 2-node behavior

With two nodes, the follower never auto-promotes on leader loss. If the leader goes down:
- The cluster becomes read-only — no new config changes can be committed.
- Existing configuration remains active on both nodes.
- Traffic continues flowing on both nodes.
- No split brain is possible — the follower simply waits for the leader to return.

A 2-node cluster provides high availability for traffic but not for configuration changes. This is a valid deployment for small teams or homelabs that want redundancy without managing a third node.

---

## Bootstrap

Bootstrap the first node:

```bash
keel --config keel.yaml --cluster --bootstrap --secret mysecret
```

The `--secret` flag sets the shared secret that joining nodes must present. You can also set it in `keel.yaml`:

```yaml
cluster:
  addr: 0.0.0.0:7654
  secret: mysecret
```

A non-empty secret is **mandatory** — Keel refuses to start cluster mode without
one, because the join listener would otherwise hand a cluster identity to any peer
that can reach the port. Use a high-entropy token, e.g. `openssl rand -hex 32`.
A weak secret is brute-forceable offline if an attacker captures a join exchange.

On bootstrap, Keel generates a cluster CA and issues a node certificate. All inter-node communication uses mTLS with this CA.

---

## Join additional nodes

On each subsequent node:

```bash
keel --config keel.yaml --cluster --join 10.0.0.1:7654 --secret mysecret
```

The joining node contacts the address given to `--join`, authenticates with the shared secret, receives a node certificate from the cluster CA, and joins the Raft group.

A new node joins as a **learner** (it receives the log but holds no quorum weight). Once its log has caught up — typically within seconds — the leader automatically promotes it to **voter**, at which point it counts toward quorum as described in the node count table above. `keel cluster status` shows each member's role.

If the join target is not reachable yet — the normal case when all nodes are started together by a service manager or orchestrator — the joiner retries with exponential backoff (1s doubling up to 30s) indefinitely, logging each attempt. Errors that retrying cannot fix are fatal and **terminate the process** so your supervisor notices: a wrong shared secret, a protocol mismatch, or an explicit rejection from the cluster.

The join exchange happens before mTLS is established, so it is encrypted with a key
derived from the shared secret (ChaCha20-Poly1305). The secret itself is never sent
on the wire — the join request and the response (which carries the new node's private
key and the CA) are both AEAD-encrypted, so a passive eavesdropper on the network
segment learns nothing and a peer without the secret cannot decrypt or forge them.

The `--join` address is only used once at startup. After a node has joined the cluster, it reconnects to peers on restart using the addresses stored in Raft state.

---

## Cluster configuration in keel.yaml

```yaml
cluster:
  addr: 0.0.0.0:7654       # RPC listen address for peer connections
  node_id: 1               # optional; derived from addr hash if omitted
  secret: change-me-in-production
```

| Field | Default | Notes |
|---|---|---|
| `addr` | `0.0.0.0:7654` | Bind address for Raft peer connections |
| `node_id` | derived | Must be unique across the cluster |
| `secret` | none | Shared secret for join authentication |
| `ca_cert` | none | BYO CA certificate path |
| `ca_key` | none | BYO CA key path |

If `node_id` is omitted, Keel derives one from a hash of the `addr` value. Explicitly set `node_id` if multiple nodes bind to the same address (e.g. behind a NAT where the bind address isn't unique).

---

## Bring your own CA

Instead of letting Keel generate a cluster CA, you can provide your own:

```bash
keel --config keel.yaml --cluster --bootstrap --ca-cert /etc/keel/ca.crt --ca-key /etc/keel/ca.key
```

Or in `keel.yaml`:

```yaml
cluster:
  addr: 0.0.0.0:7654
  ca_cert: /etc/keel/cluster-ca.crt
  ca_key: /etc/keel/cluster-ca.key
```

Joining nodes receive their certificate from this CA. The CA private key only needs to be present on the bootstrap node at startup; it is not required on follower nodes.

---

## Config replication

In cluster mode, all configuration changes flow through the Raft log. This ensures every node applies changes in the same order.

To push a new config to the entire cluster:

```bash
keel config push keel.yaml
```

This reads the local file, submits it to the leader as a Raft log entry, waits for quorum commit, and confirms once all nodes have applied it. If the cluster has no quorum the command fails.

Config push requires quorum. During a network partition or leader election, `keel config push` will block or fail. Traffic on individual nodes is unaffected.

Changes that can be pushed:
- Pool membership and weights
- Health check parameters
- Virtual host routing rules
- TLS certificate paths

Changes that require restarting each node individually:
- Listener ports
- Worker count

---

## Drain in cluster mode

Backend drain in cluster mode commits to the Raft log and is applied on all nodes:

```bash
keel backend drain 10.0.0.1:8080 --wait
```

All nodes immediately stop routing new requests to the backend. The drain completes when all nodes report zero active connections to it. Progress is streamed to the terminal.

Drain requires quorum to commit the initial drain command. If a network partition occurs mid-drain, the drain freezes — no new connections are sent to the backend, and existing connections are held until the partition heals or the cluster regains quorum.

---

## Stepping down (removing a node)

To take a node out of the cluster gracefully — for decommissioning, maintenance, or shrinking the cluster — run on that node:

```bash
keel cluster stepdown
```

What happens:

1. **Quorum check.** The node computes the post-stepdown voter set and probes each remaining voter's cluster address. If fewer than a majority of the remaining voters are reachable, the cluster would lose quorum after the stepdown, and the command refuses:

   ```
   Error: Performing this action would cause the cluster to lose quorum: after stepdown
   2 of 2 remaining voter(s) must be reachable to commit changes, but only 1 responded.
   Refusing to step down — re-run with --force to attempt anyway.
   ```

2. **Membership change via Raft.** The removal is committed to the Raft log, so every remaining node accepts the stepdown before the command returns. If the node is a follower, the request is transparently forwarded to the leader over the mTLS peer channel.

3. **Leadership handover.** If the node stepping down *is* the leader, it commits its own removal and steps down once the change is accepted; the remaining voters elect a new leader. Traffic is unaffected throughout.

On success:

```
node 3 removed from cluster membership (committed by quorum). It is safe to stop this node
```

The node keeps serving traffic with its last known config until you stop the process.

### `--force`

`keel cluster stepdown --force` skips the refusal and attempts the membership change anyway. If the remaining nodes genuinely cannot form quorum, the change cannot commit — the command fails after a 30-second timeout:

```
Error: membership change did not commit within 30s — the cluster has likely lost quorum
```

Note that the proposed change stays in the Raft log: if enough nodes come back later, the stepdown completes at that point. Use `--force` only when you understand why quorum is unavailable.

### Edge cases

- **Last voter** — stepping down the only voter would destroy the cluster; the command always refuses (the membership change could never commit). Just stop the node instead.
- **No leader** — a membership change cannot be committed without a leader; the command fails and asks you to retry after the election settles.
- **Learner** — a node that is still a learner is removed without any quorum impact.

---

## ACME certificates in cluster mode

Certificates obtained via [ACME](acme.md) are cluster state: the leader
performs issuance and renewal, commits the certificate to the Raft log, and
every node — including nodes that join later — receives it, stores it on
disk, and hot-swaps it into its TLS listeners. On restart, disk and Raft
state reconcile per hostname: the valid certificate with the most remaining
lifetime becomes the source of truth. Nothing is re-issued unless a
certificate is missing, expired, or due for renewal everywhere.

HTTP-01 challenge tokens flow through the Raft log the same way: the leader
commits each token and confirms every node holds it before asking the CA to
validate, so the CA's requests are answered by whichever node they reach.

---

## Remote control

With `control.remote` configured, every node serves [keelctl](keelctl.md) connections over mTLS; commands that change cluster state (`config push`, `stepdown`) are forwarded to the leader internally, so any node works as the endpoint across failovers. Each node's control CA is independent — share the `ca_dir` contents across nodes to use one keelconfig for the whole cluster.

---

## Split brain behavior

During a network partition:

- Each node continues forwarding traffic with its last known config. No quorum is needed to serve requests.
- Config changes (including drain) require Raft quorum. Commands issued during a partition are rejected or blocked.
- When the partition heals, nodes catch up via Raft log replay. Any committed changes during the partition (from the quorum partition) are applied to the other nodes.

This is an intentional AP/CP split: Keel is always available for traffic, and always consistent for config writes.

---

## Cluster status

```bash
keel cluster status
```

Output:

```
Cluster:
  Node ID:   12345678
  Role:      Leader
  Term:      4
  Leader:    12345678
  Committed: 42
  Members:
    [12345678] 10.0.0.1:7654  (voter)
    [98765432] 10.0.0.2:7654  (voter)
    [11223344] 10.0.0.3:7654  (learner)
```

Members are shown with their Raft role: `voter` counts toward quorum, `learner` receives the log but does not vote (nodes are learners briefly after joining, until promoted).

See [CLI reference](cli.md) for all cluster commands.
