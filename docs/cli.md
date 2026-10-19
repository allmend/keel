# CLI

`keel` without a subcommand runs a node; with one, it is a client of the running node, or a local tool (`config validate`, `credentials create`). Every client command also runs remotely through [keelctl](keelctl.md), with the same output.

## Server flags

| Flag | Default | Notes |
|---|---|---|
| `--config <path>` | `/etc/keel/node.hcl` | The node file. Missing at the default path: every node setting at its default |
| `--socket <path>` | `/var/run/keel/keel.sock` | Control socket, for client commands |
| `--force-new-cluster` | off | Make this node the only member, keeping its state; see [Forced recovery](cluster.md#forced-recovery) |
| `--no-color` | off | No colors in any output, logs included. `NO_COLOR` does the same |

## Control socket

Client commands talk to the node over a Unix socket (`--socket`). The master binds it and the control worker serves it, owned by `keel.control_user` and its group (0660); workers cannot open it. The control worker asks every worker over its own socket (`workers/worker-<index>.sock` beside it) and merges the answers:

- Connection counts are summed.
- Health is the worst any worker reports.
- Drain state is the least drained any worker reports.

A worker that does not answer within five seconds is left out and logged. Without a running node:

```
cannot connect to /var/run/keel/keel.sock
Is keel running?
```

## keel status

Uptime, and every backend's drain state, health and open connections.

```
keel — uptime 2h 14m 30s

  web (3 backends)
    10.0.0.1:8080              active      healthy     12 conn
    10.0.0.2:8080              active      healthy     8 conn
    10.0.0.3:8080              draining    healthy     3 conn
```

`unchecked`: the pool has no health check. A reason in parentheses is the last failed probe or the ejection cause; it shows before `unhealthy_threshold` is reached too.

## keel backend list

```bash
keel backend list --pool web
```

```
Pool: web (3 backends)
    10.0.0.1:8080              active      healthy     12 conn
```

## keel backend drain

```bash
keel backend drain <address> [--wait]
```

Stops new connections to the backend in every pool that has it. `address` is as written in the config; an address in no pool is refused. The receiving node's workers drain at once, then the drain is committed through Raft for every node. It survives restarts, and needs a majority of the voters.

Without `--wait` it returns once applied (`Drain complete.`); the backend is `draining`. With `--wait` it follows the connection count, summed over the workers, every 500 ms until 0:

```
Draining 10.0.0.1:8080 from pools: web
  connections: 0
Drain complete (22s elapsed).
```

It warns when not every worker applied the drain.

## keel config reload

Pushes this node's config directory as the next version, as `SIGHUP` does.

```
config version 43 committed; every node applies it
```

A directory that does not load is refused with the reason. See [Hot reload](configuration.md#hot-reload) for what applies without a restart.

## keel config push

```bash
keel config push <dir|file>
```

```
config version 42 committed; every node applies it
```

- A directory goes with every file by relative path; a single file as `keel.hcl`, alone.
- The set replaces the current version; files it lacks are deleted on every node.
- An invalid set is refused: `config not pushed: …`.
- Any node takes it; a follower forwards it to the leader.
- Without a reachable majority: `no quorum: 1 of 3 members reachable; config not pushed`.

## keel config validate

```bash
keel config validate [node.hcl] [--full] [--diff] [--list] [--write] [--check]
```

Loads the node file (default `--config`) and its `keel.config_dir` as a start or reload would, like `nginx -t`. Needs no running node.

Silent by default: exit 0 when the files can go live, 1 when they cannot, with the error on stderr:

```
/etc/keel/config/pools/web.hcl:4:3: unknown attribute `algorithmn`
```

| Flag | |
|---|---|
| `--full` | Print every file that is loaded, in load order, each under `# file <path>` |
| `--diff` | Diff against the version the running node serves, then each change marked `reload` (applies on reload) or `restart` (waits for a restart) |
| `--list` | List files not in canonical format |
| `--write` | Rewrite them in canonical format, with units on bare durations |
| `--check` | Exit 3 when a file is not in canonical format |

`--diff` asks the node over the local control socket (`--socket`); without one it says so and compares nothing. See [Checking a configuration](configuration.md#checking-a-configuration).

## keel cluster status

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

## keel cluster demote

On the leader: starts a new election. The node stays a voter. On a follower it fails with `this node is not the leader`.

## keel cluster stepdown

```bash
keel cluster stepdown [--force]
```

Removes this node from the cluster through Raft; a leader hands over leadership. Refused when the remaining voters would lose quorum, unless `--force`. See [Stepdown](cluster.md#stepdown).

## keel cluster remove

```bash
keel cluster remove <node_id>
```

```
removed 11223344 (10.0.0.3:7654); 2 voters remain, quorum 2
certificate keel-node-11223344 refused from now on
```

Removes another member, from any node. Its ID is refused from then on. See [Removing a member](cluster.md#removing-a-member).

## keel credentials create

```bash
keel credentials create <name> --endpoint <host:port> > keelconfig
```

Prints a keelconfig for [keelctl](keelctl.md): a client certificate from the control CA. Runs on the node, reading the CA from `control/` in `keel.state_dir`; generates the CA if there is none. Needs no running node.

| Argument | Notes |
|---|---|
| `name` | Certificate CN and audit identity; `[A-Za-z0-9.-_@]` |
| `--endpoint` | Address keelctl dials, written as given |

## keel credentials revoke-all

Replaces the control CA; every keelconfig issued so far stops working. In a cluster the new CA is committed through Raft and every node switches its listener. Needs a running node.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Error, or `config validate`: the configuration does not load |
| `3` | `config validate --check`: a file is not in canonical format |
