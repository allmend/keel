# Cluster

## One node or many

A node is always a cluster. A single node is a cluster of one: it runs the same processes, commits its changes (pushes, drains, ACME certificates) to its own Raft log, and keeps them across restarts. Commands and their output are the same for one node and for many. A node opens no peer port until its `node.yaml` has a `cluster:` section.

Add nodes when you need:
- Fault tolerance — traffic continues if a node fails
- Config changes committed once and applied everywhere
- ACME certificates issued once and served by every node

---

## Node count and quorum

| Nodes | Write quorum | Failure tolerance |
|---|---|---|
| 1 | 1 | none |
| 2 | 2 of 2 | none for writes; traffic continues |
| 3 | 2 of 3 | 1 node |
| 5 | 3 of 5 | 2 nodes |
| 7 | 4 of 7 | 3 nodes |

Every write — a push, a drain, a membership change, an ACME issuance — needs a majority of the voters. With two nodes that is both: either one down stops writes, and both keep serving traffic with the config they have.

A leader is elected by majority vote only; a node never promotes itself. Split across two sites with equal node counts, neither side has a majority: both keep serving, both refuse writes, and there is no split brain. For redundancy across sites, run one node per site on three or five sites.

---

## Starting a cluster

The first node has a `cluster:` section without `join`:

```yaml
# /etc/keel/node.yaml on 10.0.0.1
cluster:
  addr: 0.0.0.0:7654
  advertise: 10.0.0.1:7654
  secret: mysecret
```

Started with no stored state, it creates the cluster: generates the cluster CA, issues its own node certificate, commits the CA to Raft so every member holds it, and commits its config directory as version 1. All inter-node communication uses mTLS with this CA.

A non-empty `secret` is required. The join listener would otherwise hand a cluster identity to any peer that can reach the port. Use a high-entropy token, e.g. `openssl rand -hex 32`: a weak secret can be brute-forced offline from a captured join exchange.

A single node that grows into a cluster gets this section added to its `node.yaml` and is restarted; it keeps its state and moves to the announced address.

---

## Join additional nodes

The other nodes list members to join through:

```yaml
# /etc/keel/node.yaml on 10.0.0.2
cluster:
  addr: 0.0.0.0:7654
  advertise: 10.0.0.2:7654
  secret: mysecret
  join: [10.0.0.1:7654]
```

A node with no stored state and a `join` list only ever joins: it contacts a listed member, authenticates with the shared secret, receives a node certificate from the cluster CA, and joins the Raft group. It never starts a cluster of its own. Its own address in the list is skipped, so one node file can serve every node except the first.

Any member admits new nodes: `join` takes the announced address of any member, including the port. The member issues the node certificate; a follower forwards the membership change to the leader. A join before the first node has committed the CA is closed (`join before the cluster CA is committed`), and the joiner retries.

A new node joins as a **learner** (it receives the log but holds no quorum weight). Once its log has caught up — typically within seconds — the leader promotes it to **voter**. `keel cluster status` shows each member's role.

If no listed member is reachable yet — the normal case when all nodes are started together — the joiner retries with exponential backoff (1s doubling up to 30s) indefinitely, logging each attempt. Errors that retrying cannot fix stop the control worker, which the master restarts with a growing delay (up to 30s), logging the reason each time: a wrong shared secret, a protocol mismatch, or an explicit rejection from the cluster. Traffic is served meanwhile.

The join exchange happens before mTLS is established, so it is encrypted with a key
derived from the shared secret (ChaCha20-Poly1305). The secret itself is never sent
on the wire — the join request and the response (which carries the new node's private
key and the CA) are both AEAD-encrypted. A passive eavesdropper on the network
segment cannot read them, and a peer without the secret cannot decrypt or forge them.

The `--cluster`, `--bootstrap`, `--join` and `--secret` flags are gone. Given one, `keel` refuses to start and names the `node.yaml` field that replaced it.

### Restarts

Each node stores its Raft state in `raft/store.redb` under `keel.state_dir` (default `/var/lib/keel`): log, vote, committed index and latest snapshot. The store holds everything the cluster replicates: pushed config, the cluster and control CAs, ACME certificates and challenges, drain entries.

A node with stored state restarts from it:

- It recovers its membership, catches up from the leader and takes part in a normal election.
- `cluster.join` is not used. One exception is refused instead: a node alone in its stored cluster whose `node.yaml` lists `join` started a cluster of its own earlier. It does not start, and names the fix: stop it, remove `raft/` from the state directory, start it again.
- After a full cluster restart the nodes elect a leader and serve the committed config.

The state directory also holds `node_id`, `node.crt`, `node.key` (`0600`), `cluster-ca.crt` and `members.yaml`, the last known members, rewritten on every membership change.

### Lost or damaged Raft state

If the store cannot be read, the node:

1. Logs `Raft store unreadable and set aside` and moves `raft/` to `raft.corrupt-<time>/`.
2. Serves traffic from its local config.
3. Rejoins through `cluster.join` and the members in `members.yaml`. It never starts a new cluster.

The leader removes the old member and adds the node back as a learner. It votes again once its log has caught up and it is promoted: a voter that lost its log could help elect a leader missing a committed entry.

If `members.yaml` lists only the node itself, it starts a new one-member cluster. Drain state and history are lost, and logged as lost.

A node whose stored address differs from `advertise` rejoins the same way, with the store moved to `raft.moved-<time>/`. A one-member cluster updates its own membership instead.

### Forced recovery

If a majority of the voters is gone for good, the rest cannot commit anything. Start one survivor with `--force-new-cluster`:

```bash
keel --force-new-cluster
```

It keeps its stored state and becomes the only member. Other nodes join it through `cluster.join` after moving their `raft/` aside.

Use it only for members that will not return. A returning member still holds the old membership and forms a separate cluster with the peers it reaches.

---

## Cluster configuration in node.yaml

```yaml
cluster:
  addr: 0.0.0.0:7654         # bind address for Raft peer connections
  advertise: 10.0.0.1:7654   # address the other nodes connect to
  secret: change-me-in-production
  join: [10.0.0.2:7654]      # omit on the node that starts the cluster
```

| Field | Default | Notes |
|---|---|---|
| `addr` | `0.0.0.0:7654` | Bind address for Raft peer connections; must not be a `listeners:` address |
| `advertise` | `addr` | Address this node announces to the other nodes |
| `secret` | none | Shared secret for join authentication; required |
| `join` | none | Members to join through on first start |

The other nodes connect to `advertise`, or to `addr` when `advertise` is not set, so that address must be reachable from them. An unspecified address (`0.0.0.0`, `::`) can be bound but not reached: a node refuses to start when the announced address is unspecified, naming `cluster.advertise`. Unknown keys under `cluster:` are refused.

---

## Node identity

Each node has a random 64-bit node ID, generated on first start and stored in `node_id` in `keel.state_dir`. It is not a setting and never changes. A `node_id` file that cannot be read stops the node from starting.

`keel cluster status` shows every member's ID. The node certificate carries it as `CN=keel-node-<id>`.

A join with an ID that is already a member:

| Join from | Result |
|---|---|
| The member's address | The node lost its Raft state. The leader removes the member and adds it back as a learner |
| Another address, member still answers | Refused: a copied state directory, e.g. a cloned VM. `join rejected: node ID … is a member at …, which still answers; … holds a copy of its state directory` |
| Another address, member silent | The node moved. Handled like lost state, at the new address |

To give a copied machine its own identity, delete its `node_id` before it first starts.

---

## Cluster CA

The first node generates the cluster CA once and commits certificate and key to Raft. Every member can issue node certificates to joining nodes.

A peer request that names a sender (a Raft vote, a stepdown) is refused unless the sender is the node in the certificate's `CN=keel-node-<id>`.

Bringing your own CA is not supported: `--ca-cert`, `--ca-key`, `cluster.ca_cert` and `cluster.ca_key` are refused. The cluster CA cannot be rotated in place.

---

## Config replication

The config directory, `/etc/keel/config/`, is identical on every node. Each change is a new version, committed through Raft. `/etc/keel/node.yaml` is never replicated. See [In a cluster](configuration.md#in-a-cluster).

```bash
keel config push /etc/keel/config
config version 42 committed; every node applies it
```

- Any node accepts a push. A follower forwards it to the leader.
- A push carries every file by relative path, certificates included. A single file is pushed as `keel.yaml`. Files missing from a version are deleted on every node.
- The set is validated before commit, on the receiving node and on the leader. An invalid set is refused with the reason.
- Without a reachable majority the push fails at once, naming how many members answered.
- A node writes the files only after it applied the version. A version that fails on a node leaves it on the previous one, files unchanged.

`SIGHUP` or `keel config reload` pushes the node's config directory. A node whose files changed while it was stopped pushes them at start. A new cluster takes its first version from the first node's files.

Versions apply like a local [hot reload](configuration.md#hot-reload):

- Applied: vhost rules, TLS certificates, the `acme` section, removed backends (they drain).
- Restart needed: added backends, weights, algorithms, `health_check`, `passive`, listeners, and the `cache` and `access_log` sections.

---

## Drain

`keel backend drain` is committed through Raft, so every node and all its workers stop sending the backend new connections, whichever node received the command:

```bash
keel backend drain 10.0.0.1:8080 --wait
```

The receiving node drains at once; the others when the entry is committed. `--wait` streams the receiving node's connection count. A drain needs a majority like any write, and outlives restarts of workers, the control worker and the node.

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

## Removing a member

`stepdown` runs on the node that leaves. For a member that cannot run it — a dead machine, or one you no longer trust — run on any other node:

```bash
keel cluster remove 11223344
```

```
removed 11223344 (10.0.0.3:7654); 2 voters remain, quorum 2
certificate keel-node-11223344 refused from now on
```

- The node ID is retired through Raft, then the membership change is committed. From then on every peer refuses requests signed with that node's certificate.
- A dead member otherwise stays a voter and lowers the failure tolerance: five voters with one dead tolerate one more failure, four voters after its removal tolerate one as well, with a smaller quorum.
- A removed node that comes back cannot rejoin under its ID: the join is refused, naming the fix. Delete `node_id` and `raft/` in its state directory and it joins as a new node.
- Removing the leader works; it steps down once its removal is committed. Removing the last voter is refused.
- No quorum probe: without a reachable majority the command fails like any write.

For a compromised node, removal stops it taking part in the cluster, but it still holds the cluster CA key, as every member does, and could issue certificates under other node IDs. The cluster CA cannot be rotated in place; see [Cluster CA](#cluster-ca).

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

With `control.remote` configured, every node serves [keelctl](keelctl.md) connections over mTLS. Writes sent to a follower — `config push`, `backend drain`, `cluster stepdown`, `credentials revoke-all` — are forwarded to the leader. `status` and `backend list` answer for the node that receives them. The control CA is replicated through the Raft log, so one keelconfig authenticates to every node — see [Remote control](keelctl.md#cluster-mode).

---

## Split brain behavior

During a network partition:

- Each node continues forwarding traffic with its last known config. No quorum is needed to serve requests.
- Config pushes, drains and membership changes require Raft quorum. Commands issued during a partition are rejected or blocked.
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
