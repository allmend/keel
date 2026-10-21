# CLI

`keel` without a subcommand runs a node; with one, it is a client of the running node, or a local tool (`config validate`, `credentials create`). Every client command also runs remotely through [keelctl](keelctl.md), with the same output.

## Server flags

| Flag | Default | Notes |
|---|---|---|
| `--config <path>` | `/etc/keel/node.hcl` | The node file. Missing at the default path: every node setting at its default |
| `--socket <path>` | `/var/run/keel/keel.sock` | Control socket, for client commands |
| `--force-new-cluster` | off | Make this node the only member, keeping its state; see [Forced recovery](cluster.md#forced-recovery) |
| `--no-color` | off | No colors in any output, logs included. `NO_COLOR` does the same |
| `-o`, `--output <text\|json>` | `text` | Client commands and local tools; see [JSON output](#json-output) |

## JSON output

`-o json` prints one JSON object per line on stdout, for every client command and local tool, on `keel` and `keelctl` alike. Errors go to stderr with a non-zero exit, as in text mode. Fields:

| Command | Object |
|---|---|
| `status` | `uptime_secs`, `listeners`: `[{name, protocol, bound, skipped}]`, `pools`: `[{name, backends}]` |
| `backend list` | `pool`, `backends` |
| `backend drain` | `pools`, `workers`, `workers_acknowledged`, `done`; with `--wait` then one `{connections, done}` per update until `done` |
| `backend activate` | `pools` |
| `cluster status` | `node_id`, `role`, `term`, `leader_id`, `last_committed`, `membership`: `[{id, addr, role}]` |
| `config push` | `message`, `version` |
| `config reload`, `cluster demote`, `cluster stepdown`, `cluster remove`, `credentials revoke-all` | `message` |
| `config validate` | `valid`, `error` when not, `unformatted` (files), `files` with `--full` (`[{path, text}]`), `diff` and `changes` with `--diff` (`[{new_workers, change}]`) |
| `credentials create` | `endpoint`, `ca_cert`, `client_cert`, `client_key` |

A backend is `{address, state, health, health_reason, connections}`. `state` is `active`, `draining` or `removed`; `health` is `healthy`, `unhealthy`, `initializing`, `ejected` or `unchecked`; `health_reason` is `null` or why.

## Control socket

Client commands talk to the node over a Unix socket (`--socket`). The master binds it and the control worker serves it, owned by `keel.control_user` and its group (0660); workers cannot open it. The control worker asks every worker over its own socket (`workers/worker-<index>.sock` beside it) and merges the answers:

- Connection counts are summed.
- Health is the control worker's verdict, with its reason; `ejected` when any worker ejected the backend.
- Drain state is the least drained any worker reports.

A worker that does not answer within five seconds is left out and logged. Without a running node:

```
cannot connect to /var/run/keel/keel.sock
Is keel running?
```

## keel status

Uptime, what each listener bound on this node, and every backend's drain state, health and open connections.

```
keel — uptime 2h 14m 30s
  listener web (https): 194.56.78.90:443; not on this node: 194.56.78.91:443, 194.56.78.92:443
  listener dns (udp): 0.0.0.0:53, [::]:53

  web (3 backends)
    10.0.0.1:8080              active      healthy     12 conn
    10.0.0.2:8080              active      healthy     8 conn
    10.0.0.3:8080              draining    healthy     3 conn
```

`unchecked`: the pool has no health check. `initializing`: no probe has answered yet. A reason in parentheses is the last failed probe or the ejection cause; it shows before `unhealthy_threshold` is reached too.

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

Stops new connections to the backend in every pool that has it. `address` is `ip:port` as written in the config, or an IP for every backend on it; an address in no pool is refused. The receiving node's workers drain at once, then the drain is committed through Raft for every node. It survives restarts, and needs a majority of the voters.

Without `--wait` it returns once applied (`Drain complete.`); the backend is `draining`. With `--wait` it follows the connection count, summed over the workers, every 500 ms until 0:

```
Draining 10.0.0.1:8080 from pools: web
  connections: 0
Drain complete (22s elapsed).
```

It warns when not every worker applied the drain.

## keel backend activate

```bash
keel backend activate <address>
```

Returns a drained backend to service in every pool that has it, on every node. `address` as for `drain`. Committed through Raft like a drain. Health checks and passive ejection still apply.

## keel upgrade

Runs the binary on disk in place of the running one, without dropping a connection. `SIGUSR2` to the master does the same.

```
upgrade started: the master tries the binary on disk, then runs it in place; see the node's log
```

1. The master runs the new binary's `--version` and `config validate`. If either fails, nothing changes: `master: upgrade refused` in the log.
2. It stops the control worker (a leader's peers elect another within seconds) and executes the new binary in its own process: same PID, so systemd and container init see no change. Listening sockets, the metrics region and the workers carry over; counters continue.
3. The new binary starts a control worker and new workers. Once they have the node's state, the old workers drain as on a reload: open requests, WebSockets, TCP connections and UDP flows finish or move over, at most `keel.drain_timeout`.

`keel_build_info{version}` shows the running version. Install the new binary the way packages do, by writing a new file and renaming it over the old one; a running binary cannot be written in place. A node file that changes `keel.user`, `keel.group`, `keel.control_user` or a path applies with the upgrade.

## keel config reload

Pushes this node's config directory as the next version, as `SIGHUP` does.

```
config version 43 committed; every node applies it
```

A directory that does not load is refused with the reason. See [Hot reload](configuration.md#hot-reload) for how each change applies.

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
| `--diff` | Diff against the version the running node serves, then each change marked `reload` (applies in place) or `workers` (applies by new workers); both on reload, without dropping connections |
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
