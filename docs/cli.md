# CLI Reference

The `keel` binary serves two roles: running the proxy and acting as a CLI client to a running instance. Mode is determined by subcommand.

Every client command on this page except `keel credentials create` and `keel backend add` also works remotely via keelctl over mTLS, with identical output — see [Remote control](keelctl.md).

## Server flags

These flags are used when starting Keel as a server (no subcommand).

| Flag | Default | Notes |
|---|---|---|
| `--config <path>` | `/etc/keel/node.yaml` | The node file; it names the config directory (`keel.config_dir`). Missing at the default path = every node setting at its default. See [Files and layout](configuration.md#files-and-layout) |
| `--socket <path>` | `/var/run/keel/keel.sock` | Control socket path |
| `--force-new-cluster` | false | Forced recovery: keep this node's Raft state and make it the only member; see [Forced recovery](cluster.md#forced-recovery) |

Whether a node starts a cluster or joins one is set in `node.yaml` (`cluster.join`), not by flags; see [Cluster](cluster.md#starting-a-cluster). `--cluster`, `--bootstrap`, `--join` and `--secret` are refused, with the field that replaced them.

Examples:

```bash
# /etc/keel/node.yaml and /etc/keel/config/
keel

# Another node file
keel --config /srv/keel/node.yaml
```

---

## Control socket

CLI subcommands communicate with a running Keel instance over a Unix socket. The default path is `/var/run/keel/keel.sock`. Override with `--socket`:

```bash
keel --socket /tmp/keel.sock status
```

The master binds the socket and the control worker serves it; every command covers the whole node. Workers are separate processes with no shared memory, so each holds only its own connection counters, health results and drain progress; the control worker queries all of them and merges the results. Each worker has its own socket for this, `worker-<index>.sock` in a `workers/` subdirectory beside the instance socket. The socket belongs to `keel.control_user` and its group (`0660`); workers cannot open it.

A worker that does not answer within five seconds is excluded from the merged result and logged; the command reports what the remaining workers returned. `keel backend drain` reports how many workers applied the drain and warns when that is fewer than all of them, since a worker that missed it continues to send new connections to the backend. `keel backend drain --wait` reports the connection count as unknown and keeps waiting when no worker answers, rather than treating it as zero.

If Keel is not running or the socket path is wrong, the command fails with:

```
cannot connect to /var/run/keel/keel.sock
Is keel running?
```

---

## keel status

Show the status of the running instance: uptime, and for every backend its drain state, health and open connections. `unchecked` means the pool has no health check.

Connection counts are summed across all workers. Health is the worst state any worker reports, so a backend one worker refuses to route to is shown as degraded. Drain state is the least-drained any worker reports, so a backend that one worker still sends new connections to is shown as `active` rather than `draining`.

A reason in parentheses is the last failed probe or the ejection cause. It remains visible on a backend still shown as `healthy`: a probe failure is recorded immediately, while the state changes only after `unhealthy_threshold` consecutive failures.

```bash
keel status
```

Output:

```
keel — uptime 2h 14m 30s

  web (3 backends)
    10.0.0.1:8080              active      healthy     12 conn
    10.0.0.2:8080              active      healthy     8 conn
    10.0.0.3:8080              draining    healthy     3 conn
```

---

## keel backend list

List the backends in a specific pool with their current state and connection counts.

```bash
keel backend list --pool <name>
```

Example:

```bash
keel backend list --pool web
```

Output:

```
Pool: web (3 backends)
    10.0.0.1:8080              active      healthy     12 conn
    10.0.0.2:8080              active      healthy     8 conn
    10.0.0.3:8080              active      healthy     5 conn
```

---

## keel backend drain

Stop routing new requests to a backend and optionally wait until all active connections finish.

```bash
keel backend drain <address> [--wait]
```

The `address` must match exactly how it appears in the config (e.g. `10.0.0.1:8080`). If the address appears in multiple pools it is drained from all of them. An address in no pool is refused before anything is committed.

Without `--wait`, the command applies the drain, prints `Drain complete.` and returns. The message means the drain was applied; the backend is in the `draining` state and keeps its open connections until they end.

The drain is applied at once in every worker of the receiving node, then committed through Raft, which applies it on every other node. It is stored with the cluster state, not in the config: it outlives restarts of a worker, the control worker and the node. Committing needs a majority of the voters; without one the command fails after the receiving node has drained.

With `--wait`, the command blocks until the backend reaches zero active connections, summed across all workers. The count is polled every 500 ms and updated in place on one line:

```bash
keel backend drain 10.0.0.1:8080 --wait
```

```
Draining 10.0.0.1:8080 from pools: web
  connections: 0
Drain complete (22s elapsed).
```

A drained backend stays drained; there is no command to return it to service yet, and a config reload does not re-activate it. See [Cluster](cluster.md#drain).

---

## keel backend add

```bash
keel backend add <address> --pool <name>
```

Not supported: the command always fails and prints the instruction to add the backend to the config. A backend added to the config takes effect on restart, not on reload — see [Hot reload](configuration.md#hot-reload).

---

## keel config reload

Push this node's config directory as the next version, like `keel config push` with that directory; the answer names the committed version. `SIGHUP` to the master does the same.

```bash
keel config reload
```

On success:

```
config version 43 committed; every node applies it
```

A directory that does not load is refused with the reason, and nothing is committed.

What reloads: virtual host rules, TLS certificates, ACME hosts, backends removed from a pool (they are drained).

What does not reload without a restart: backends added to a pool, backend weights, algorithms, `health_check`, `passive`, listener ports, worker count. See [Hot reload](configuration.md#hot-reload) for the details.


---

## keel config push

Commit a config directory as the next version; every node applies it.

```bash
keel config push <dir|file>
```

Example:

```bash
keel config push /etc/keel/config
config version 42 committed; every node applies it
```

- A directory is pushed with every file by relative path, certificates included. A single file is pushed as `keel.yaml`, alone.
- The set replaces the current version. Files it no longer has are deleted on every node.
- It is validated before commit. An invalid set is refused with the reason (`config not pushed: …`).
- Any node accepts it. A follower forwards it to the leader.
- Without a reachable majority it fails at once: `no quorum: 1 of 3 members reachable; config not pushed`.

See [Config replication](cluster.md#config-replication).

---

## keel cluster status

Show Raft state for the local node, including role, term, committed log index, and cluster membership.

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

Members are shown with their Raft role — `voter` (counts toward quorum) or `learner` (still catching up after join).

---

## keel cluster demote

Trigger a new leader election from the current leader. The node stays a voting member and can win the election again. Must be run on the leader — on a follower the command fails with `this node is not the leader`.

```bash
keel cluster demote
```

---

## keel cluster stepdown

Gracefully remove the local node from the cluster. If the node is the leader, leadership is handed over to the remaining voters; the removal is committed to the Raft log so every remaining node accepts it before the command returns.

```bash
keel cluster stepdown [--force]
```

| Flag | Effect |
|---|---|
| `--force` | Attempt the stepdown even if the remaining nodes would lose quorum |

Before committing anything, the command probes the remaining voters. If the cluster would lose quorum after the stepdown, it refuses:

```
Error: Performing this action would cause the cluster to lose quorum: after stepdown
2 of 2 remaining voter(s) must be reachable to commit changes, but only 1 responded.
Refusing to step down — re-run with --force to attempt anyway.
```

On success:

```
node 3 removed from cluster membership (committed by quorum). It is safe to stop this node
```

The node keeps serving traffic until you stop the process. See [Cluster — Stepping down](cluster.md#stepping-down-removing-a-node) for details and edge cases.

---

## keel cluster remove

Remove another member — dead, or one you no longer trust — from any node. A follower forwards the command to the leader.

```bash
keel cluster remove <node_id>
```

```
removed 11223344 (10.0.0.3:7654); 2 voters remain, quorum 2
certificate keel-node-11223344 refused from now on
```

The node ID comes from `keel cluster status`. It is refused for good: every peer refuses its certificate, and a join under it fails. Removing the leader works; it steps down once its removal is committed. Without a reachable majority the command fails like any write. See [Cluster — Removing a member](cluster.md#removing-a-member).

---

## keel credentials create

Issue an operator client certificate from the control CA and print a keelconfig for [keelctl](keelctl.md) to stdout. Runs locally on the node (it reads the CA key from `control/` under `keel.state_dir`); it does not need a running keel.

```bash
keel credentials create <name> --endpoint <host:port> > keelconfig
```

| Argument | Notes |
|---|---|
| `name` | Operator name — becomes the certificate CN and the audit-log identity. `[A-Za-z0-9.-_@]` |
| `--endpoint` | Address keelctl dials; written into the keelconfig verbatim (DNS name, VIP, or IP) |

The control CA is generated on first use. The output contains a private key — store it like one.

---

## keel credentials revoke-all

Replace the control CA. Every keelconfig issued so far stops working; issue new ones with `keel credentials create`.

```bash
keel credentials revoke-all
```

In a cluster the new CA is committed through Raft (a follower forwards the request to the leader) and every node re-keys its remote control listener. On a single node the running listener switches at once. Needs a running keel.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| non-zero | Error (message printed to stderr) |
