# Cluster

A node is always a cluster, of one or more. A single node runs the same processes, commits its changes (pushes, drains, ACME certificates) to its own Raft log and keeps them across restarts. Commands and output are the same for one node and many. A node opens no peer port without a `cluster` block in `node.hcl`.

## Quorum

| Nodes | Write quorum | Tolerates |
|---|---|---|
| 1 | 1 | none |
| 2 | 2 of 2 | none for writes |
| 3 | 2 of 3 | 1 node |
| 5 | 3 of 5 | 2 nodes |
| 7 | 4 of 7 | 3 nodes |

Every write — a push, a drain, a membership change, an ACME issuance — needs a majority of the voters. Traffic needs none: every node serves the last version it applied.

A leader is elected by majority vote; a node never promotes itself. Without the leader's heartbeat for 1–2 seconds the others elect a new one; writes wait, traffic does not. A partition with no majority side keeps serving everywhere and refuses writes everywhere. When it heals, nodes catch up from the Raft log.

## Configuration

```hcl
cluster {
  addr      = "0.0.0.0:7654"
  advertise = "10.0.0.2:7654"
  secret    = "change-me"
  join      = ["10.0.0.1:7654"]
}
```

| Attribute | Default | Notes |
|---|---|---|
| `addr` | `0.0.0.0:7654` | Peer listen address; not a listener's address |
| `advertise` | `addr` | Address the other nodes connect to. Required when `addr` is unspecified (`0.0.0.0`, `::`) |
| `secret` | none | Shared join secret; required. A weak secret can be brute-forced offline from a captured join exchange: `openssl rand -hex 32` |
| `join` | none | Members to join through on first start |

## Starting and joining

The first node has no `join`. Started without stored state, it generates the cluster CA, issues its own node certificate, commits the CA to Raft and commits its config directory as version 1.

A node with `join` and no stored state only joins: it contacts a listed member, authenticates with the secret, gets a node certificate and joins the Raft group. Its own address in the list is skipped, so every node but the first can share one `cluster` block.

- Any member admits nodes; a follower forwards the membership change to the leader. A join before the CA is committed is closed, and the joiner retries.
- A new node joins as a learner (gets the log, does not vote) and is promoted to voter once its log has caught up.
- An unreachable member is retried with backoff (1s doubling up to 30s), forever. A wrong secret, a protocol mismatch or a refusal stops the control worker; the master restarts it with a growing delay (up to 30s). Traffic is served meanwhile.
- The join exchange is encrypted with a key derived from the secret (ChaCha20-Poly1305); the secret is never sent. After joining, peers talk mTLS.

A single node with a `cluster` block added and a restart keeps its state and moves to the announced address.

## Restarts

Raft state is in `raft/store.redb` under `keel.state_dir` (default `/var/lib/keel`): log, vote, committed index, snapshot. A node with stored state restarts from it, catches up from the leader and ignores `cluster.join` — except a node that is alone in its stored cluster and lists `join`: it started a cluster of its own earlier, and refuses to start until `raft/` is removed.

The state directory also holds `node_id`, `node.crt`, `node.key` (0600), `cluster-ca.crt` and `members.hcl`, the last known members as `member "<node_id>"` blocks.

### Lost or damaged Raft state

An unreadable store is moved to `raft.corrupt-<time>/` and logged (`Raft store unreadable and set aside`). The node serves traffic from its local config and rejoins through `cluster.join` and `members.hcl`, never starting a new cluster. The leader replaces the old member with the node as a learner. If `members.hcl` lists only the node itself, it starts a new one-member cluster; drain state is lost and logged as lost.

A node whose stored address differs from `advertise` rejoins the same way, its store moved to `raft.moved-<time>/`. A one-member cluster updates its own membership instead.

### Forced recovery

With a majority of the voters gone for good, start one survivor with:

```bash
keel --force-new-cluster
```

It keeps its state and becomes the only member. Other nodes move `raft/` aside and join it. A member that later returns still holds the old membership and forms a separate cluster with the peers it reaches.

## Node identity

A random 64-bit node ID, generated on first start, in `node_id` in `keel.state_dir`. It never changes; an unreadable `node_id` stops the node. The node certificate carries it as `CN=keel-node-<id>`, and a peer request naming a sender is refused unless the certificate names that sender.

A join with an ID that is already a member:

| From | Result |
|---|---|
| The member's address | Lost Raft state: the member is replaced, as a learner |
| Another address, member answers | Refused, as a copied state directory: `join rejected: node ID … is a member at …, which still answers; … holds a copy of its state directory` |
| Another address, member silent | A move: handled like lost state, at the new address |

A copied machine gets its own identity when its `node_id` is deleted before its first start.

## Cluster CA

Generated once by the first node; certificate and key are committed to Raft, so every member can issue node certificates. It cannot be rotated in place.

## Config replication

The config directory is the same on every node; each push is a version committed through Raft. `node.hcl` is not replicated. See [In a cluster](configuration.md#in-a-cluster).

```bash
keel config push /etc/keel/config
config version 42 committed; every node applies it
```

- Any node takes a push; a follower forwards it to the leader.
- Every file goes by relative path, certificates included; a single file is pushed as `keel.hcl`. Files missing from a version are deleted.
- The set is validated on the receiving node and on the leader before commit.
- Without a reachable majority the push fails at once.
- A node writes the files after applying the version. A version that fails on a node leaves it on the previous one.

`SIGHUP` or `keel config reload` pushes the node's config directory. A node whose files changed while it was stopped pushes them at start.

## Drain

`keel backend drain` is committed through Raft: every worker of every node stops sending the backend new connections. The receiving node drains at once, the others on commit. `--wait` follows the receiving node's connection count. A drain outlives restarts of workers, the control worker and the node, until `keel backend activate` is committed the same way.

## Stepdown

`keel cluster stepdown` on a node removes it from the cluster.

1. The node probes the voters that would remain. With fewer than a majority of them reachable it refuses:

   ```
   Error: Performing this action would cause the cluster to lose quorum: after stepdown
   2 of 2 remaining voter(s) must be reachable to commit changes, but only 1 responded.
   Refusing to step down — re-run with --force to attempt anyway.
   ```

2. The removal is committed through Raft; a follower forwards it to the leader.
3. A leader commits its own removal, then steps down; the rest elect a new leader.

```
node 3 removed from cluster membership (committed by quorum). It is safe to stop this node
```

The node serves traffic until it is stopped.

`--force` skips the probe. A change that cannot commit fails after 30 seconds (`membership change did not commit within 30s — the cluster has likely lost quorum`) and stays in the log: it completes if enough nodes return.

The last voter cannot step down. Without a leader the command fails. A learner leaves without affecting quorum.

## Removing a member

For a member that cannot step down itself, on any other node:

```bash
keel cluster remove 11223344
```

```
removed 11223344 (10.0.0.3:7654); 2 voters remain, quorum 2
certificate keel-node-11223344 refused from now on
```

- The node ID is retired through Raft, then the membership change is committed. Every peer refuses that node's certificate from then on.
- A returning removed node cannot join under its ID; it joins as a new node after `node_id` and `raft/` are deleted.
- Removing the leader works: it steps down once its removal is committed. The last voter cannot be removed.
- Without a reachable majority it fails like any write.

A removed node still holds the cluster CA key, like every member.

## Remote control

With `control.remote` set, every node serves [keelctl](keelctl.md). Writes sent to a follower (`config push`, `backend drain`, `cluster stepdown`, `cluster remove`, `credentials revoke-all`) go to the leader; `status` and `backend list` answer for the receiving node. The control CA is replicated, so one keelconfig works on every node.

## Status

```bash
keel cluster status
```

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

ACME in a cluster: see [ACME](acme.md#cluster).
