# Cluster

## Standalone vs cluster

Standalone mode is a first-class deployment target. A single Keel node reads its config from a local YAML file, has no cluster overhead, and supports all features including caching, TLS, health checks, and drain.

Use cluster mode when you need:
- Fault tolerance — traffic continues if a node fails
- Coordinated config changes across nodes
- ACME certificates issued once and served by every node

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
  addr: 10.0.0.1:7654
  secret: mysecret
```

A non-empty secret is required — Keel refuses to start cluster mode without
one, because the join listener would otherwise hand a cluster identity to any peer
that can reach the port. Use a high-entropy token, e.g. `openssl rand -hex 32`.
A weak secret can be brute-forced offline from a captured join exchange.

On bootstrap, Keel generates a cluster CA and issues a node certificate. All inter-node communication uses mTLS with this CA.

---

## Join additional nodes

On each subsequent node:

```bash
keel --config keel.yaml --cluster --join 10.0.0.1:7654 --secret mysecret
```

The joining node contacts the address given to `--join`, authenticates with the shared secret, receives a node certificate from the cluster CA, and joins the Raft group.

Only the bootstrap node can admit new nodes: it is the only process that holds the cluster CA, and it holds it in memory. `--join` must point at the bootstrap node's `cluster.addr`, including the port; any other member logs `plain join but no CA (not bootstrap node)` and the joiner keeps retrying.

A new node joins as a **learner** (it receives the log but holds no quorum weight). Once its log has caught up — typically within seconds — the leader automatically promotes it to **voter**, at which point it counts toward quorum as described in the node count table above. `keel cluster status` shows each member's role.

If the join target is not reachable yet — the normal case when all nodes are started together by a service manager or orchestrator — the joiner retries with exponential backoff (1s doubling up to 30s) indefinitely, logging each attempt. Errors that retrying cannot fix are fatal and terminate the process so the supervisor notices: a wrong shared secret, a protocol mismatch, or an explicit rejection from the cluster.

The join exchange happens before mTLS is established, so it is encrypted with a key
derived from the shared secret (ChaCha20-Poly1305). The secret itself is never sent
on the wire — the join request and the response (which carries the new node's private
key and the CA) are both AEAD-encrypted. A passive eavesdropper on the network
segment cannot read them, and a peer without the secret cannot decrypt or forge them.

### Restarts

Raft state — log, membership, and the cluster CA — is held in memory only; nothing is written to disk. A node does not rejoin on its own after a restart:

- A restarted member must be started with `--join` again, pointing at the bootstrap node. It rejoins under its node ID (see [Node identity](#node-identity)) and receives the current state from the leader.
- A restarted bootstrap node started with `--bootstrap` generates a new cluster CA and forms a new single-node cluster. The other nodes hold certificates from the previous CA and cannot communicate with it; restart them with `--join` pointing at the new bootstrap node.
- After a full cluster restart, each node starts from its local `keel.yaml` and every backend is active. ACME certificates are recovered from disk (see [ACME certificates in cluster mode](#acme-certificates-in-cluster-mode)).

---

## Cluster configuration in keel.yaml

```yaml
cluster:
  addr: 0.0.0.0:7654         # bind address for Raft peer connections
  advertise: 10.0.0.1:7654   # address the other nodes connect to
  secret: change-me-in-production
```

| Field | Default | Notes |
|---|---|---|
| `addr` | `0.0.0.0:7654` | Bind address for Raft peer connections |
| `advertise` | `addr` | Address this node announces to the other nodes |
| `secret` | none | Shared secret for join authentication |
| `ca_cert` | none | Accepted but not used; see [Cluster CA](#cluster-ca) |
| `ca_key` | none | Accepted but not used |

The other nodes connect to `advertise`, or to `addr` when `advertise` is not set, so that address must be reachable from them. An unspecified address (`0.0.0.0`, `::`) can be bound but not reached: cluster mode refuses to start when the announced address is unspecified, naming `cluster.advertise`. Unknown keys under `cluster:` are refused.

---

## Node identity

Each node has a random 64-bit node ID. It is generated on first start and stored in `node_id` in `keel.state_dir` (default `/var/lib/keel/node_id`); every later start, reboot, or upgrade reads it back. The ID is not a setting. A `node_id` file that cannot be read or parsed stops the node from starting — Keel never picks a new ID in its place. Started as root, Keel creates the state directory and assigns it to `keel.user` before dropping privileges.

`keel cluster status` shows each member's ID and address, and the node's own ID. The node certificate carries it as `CN=keel-node-<id>`.

A join with an ID that is already a member is handled by where it comes from:

| Join comes from | Result |
|---|---|
| The member's registered address | The node rejoins (a restart) |
| Another address, and the registered address still answers | Refused: the joiner holds a copy of the member's state directory — a cloned VM or a copied disk. It exits with `join rejected: node ID … is a member at …, which still answers; … holds a copy of its state directory` |
| Another address, and the registered address does not answer | The node has moved. The leader removes the old member; the node joins again as a learner at its new address and is promoted to voter once its log has caught up |

To give a copied machine an identity of its own, delete `node_id` from its state directory before it first starts.

---

## Cluster CA

The bootstrap node generates the cluster CA at startup and keeps it in memory. Joining nodes receive their node certificate from it.

Bringing your own CA is not implemented. The `--ca-cert` / `--ca-key` flags and the `cluster.ca_cert` / `cluster.ca_key` fields are accepted and ignored; the bootstrap node always generates its own CA.

---

## Config replication

In cluster mode, all configuration changes flow through the Raft log. This ensures every node applies changes in the same order.

To push a new config to the entire cluster:

```bash
keel config push keel.yaml
```

This reads the local file and commits its text as a Raft log entry. The command returns once a quorum has committed the entry; each node applies it when the entry reaches its state machine.

- **Send it to the leader.** Raft accepts writes only on the leader, and `config push` is not forwarded: sent to a follower, the command fails. `keel cluster status` shows the leader.
- **One file.** The file's text is pushed as is. `include:` globs and `--conf-dir` fragments are not expanded, so a split config is pushed without its fragments.
- **No validation before commit.** The text is not validated before it is committed. Each node parses it when applying; a node that cannot parse it logs `cluster: invalid config YAML` and keeps its current config.
- **Quorum.** Without quorum the commit cannot happen: during a network partition or leader election the command blocks or fails. Traffic on individual nodes is unaffected.

A pushed config is applied the same way as a local [hot reload](configuration.md#hot-reload):

- Applied: virtual host routing rules, TLS certificates, backends removed from a pool (they drain).
- Not applied until restart: backends added to a pool, weights, algorithms, `health_check`, `passive`, listeners, and the `keel`, `cache`, `access_log`, `metrics`, `acme` and `control` sections.

---

## Drain in cluster mode

`keel backend drain` applies to the node that receives the command only. It is not committed to the Raft log and needs no quorum:

```bash
keel backend drain 10.0.0.1:8080 --wait
```

That node stops routing new connections to the backend; `--wait` streams that node's connection count. The other nodes keep sending traffic to the backend. To drain a backend cluster-wide, run the command on every node (locally or with `keelctl` against each node's endpoint).

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

3. **Leadership handover.** If the node stepping down is the leader, it commits its own removal and steps down once the change is accepted; the remaining voters elect a new leader. Traffic is unaffected throughout.

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

With `control.remote` configured, every node serves [keelctl](keelctl.md) connections over mTLS. `cluster stepdown` sent to a follower is forwarded to the leader; `config push` is not and must reach the leader. Other commands answer for the node that receives them. The control CA is replicated through the Raft log, so one keelconfig authenticates to every node — see [Remote control](keelctl.md#cluster-mode).

---

## Split brain behavior

During a network partition:

- Each node continues forwarding traffic with its last known config. No quorum is needed to serve requests.
- Config pushes and membership changes require Raft quorum. Commands issued during a partition are rejected or blocked. Drain is local to each node and unaffected.
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
  Role:      leader
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
