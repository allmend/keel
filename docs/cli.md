# CLI Reference

The `keel` binary serves two roles: running the proxy and acting as a CLI client to a running instance. Mode is determined by subcommand.

## Server flags

These flags are used when starting Keel as a server (no subcommand).

| Flag | Default | Notes |
|---|---|---|
| `--config <path>` | `keel.yaml` | Path to the YAML config file |
| `--conf-dir <dir>` | none | Load additional `*.yaml` files from this directory (conf.d style) |
| `--socket <path>` | `/var/run/keel/keel.sock` | Control socket path |
| `--cluster` | false | Enable cluster mode |
| `--bootstrap` | false | Bootstrap a new cluster (requires `--cluster`) |
| `--join <addr>` | none | Join an existing cluster at this address (requires `--cluster`) |
| `--secret <secret>` | none | Shared secret for cluster join/bootstrap |
| `--ca-cert <path>` | none | BYO cluster CA certificate |
| `--ca-key <path>` | none | BYO cluster CA key |
| `--upgrade` | false | Trigger a zero-downtime binary upgrade |

Examples:

```bash
# Standalone
keel --config /etc/keel/keel.yaml

# With conf.d
keel --config /etc/keel/keel.yaml --conf-dir /etc/keel/conf.d

# Cluster — bootstrap first node
keel --config /etc/keel/keel.yaml --cluster --bootstrap --secret mytoken

# Cluster — join existing cluster
keel --config /etc/keel/keel.yaml --cluster --join 10.0.0.1:7654 --secret mytoken
```

---

## Control socket

CLI subcommands communicate with a running Keel instance over a Unix socket. The default path is `/var/run/keel/keel.sock`. Override with `--socket`:

```bash
keel --socket /tmp/keel.sock status
```

If Keel is not running or the socket path is wrong, the command fails with:

```
cannot connect to /var/run/keel/keel.sock
Is keel running?
```

---

## keel status

Show the status of the running instance, including uptime and backend state for all pools.

```bash
keel status
```

Output:

```
keel — uptime 2h 14m 30s

  web (3 backends)
    10.0.0.1:8080             active      12 conn
    10.0.0.2:8080             active       8 conn
    10.0.0.3:8080             draining     3 conn
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
    10.0.0.1:8080             active      12 conn
    10.0.0.2:8080             active       8 conn
    10.0.0.3:8080             active       5 conn
```

---

## keel backend drain

Stop routing new requests to a backend and optionally wait until all active connections finish.

```bash
keel backend drain <address> [--wait]
```

The `address` must match exactly how it appears in `keel.yaml` (e.g. `10.0.0.1:8080`). If the address appears in multiple pools it is drained from all of them.

Without `--wait`, the command initiates the drain and returns immediately. The backend transitions to the `Draining` state and continues to drain in the background.

With `--wait`, the command blocks and streams live connection counts until the backend reaches zero active connections:

```bash
keel backend drain 10.0.0.1:8080 --wait
```

```
Draining 10.0.0.1:8080 from pools: web
  connections: 14
  connections: 9
  connections: 3
  connections: 0
Drain complete (22s elapsed).
```

In cluster mode, drain is committed via Raft and applied on all nodes. See [Cluster](cluster.md).

---

## keel config reload

Reload configuration from disk. Equivalent to sending `SIGHUP` to the master process.

```bash
keel config reload
```

On success:

```
config reloaded
```

What reloads: pool membership, health check settings, virtual host rules, TLS certificates.

What does not reload without a restart: listener ports, worker count.

In cluster mode, use `keel config push` instead to distribute the new config to all nodes.

---

## keel config push

Push a config file to all cluster nodes via Raft. Only available in cluster mode.

```bash
keel config push <file>
```

Example:

```bash
keel config push /etc/keel/keel.yaml
```

The file is read locally, submitted to the cluster leader as a Raft log entry, committed to quorum, and applied on all nodes. The command waits for commit before returning.

Requires quorum. Fails if the cluster has no leader or cannot reach quorum.

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
  Role:      Leader
  Term:      4
  Leader:    12345678
  Committed: 42
  Members:
    [12345678] 10.0.0.1:7654  (voter)
    [98765432] 10.0.0.2:7654  (voter)
    [11223344] 10.0.0.3:7654  (learner)
```

Members are shown with their Raft role — `voter` (counts toward quorum) or `learner` (still catching up after join).

Only available in cluster mode.

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

Only available in cluster mode.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| non-zero | Error (message printed to stderr) |
