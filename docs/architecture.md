# Architecture

How a Keel node is put together: its processes, what each one owns, and how
traffic, configuration, health, metrics and logs move between them.

---

## One node

Every node runs the same tree of processes, standalone or in a cluster. A
node without a `cluster` block is a cluster of one.

```
                         keel  (master)
                  +-----------------------------+
                  | binds every socket          |
                  | forks and restarts children |
                  | maps the metrics region     |
                  | reads no input from anyone  |
                  +--------------+--------------+
                                 | fork
          +----------------------+----------------------+
          |                      |                      |
          v                      v                      v
+-------------------+  +-------------------+  +-------------------+
|  control worker   |  |     worker 0      |  |     worker N      |
|-------------------|  |-------------------|  |-------------------|
| Raft (config,     |  | Pingora data      |  | same as worker 0  |
|  drains, certs)   |  |  plane: HTTP,     |  |                   |
| control socket,   |  |  TCP, UDP         |  |                   |
|  control.remote   |  | health checks,    |  |                   |
| /metrics          |  |  passive ejection |  |                   |
| ACME              |  | access logs       |  |                   |
| applies config    |  |                   |  |                   |
|  to the workers   |  |                   |  |                   |
+-------------------+  +-------------------+  +-------------------+
```

| Process | Runs as | Accepts connections on | Can read |
|---|---|---|---|
| master | root (or the invoking user) | nothing | `node.hcl`, the config directory |
| control worker | `keel.control_user` | control socket, `control.remote`, Raft peers, metrics | the state directory: node key, Raft store, CA keys, ACME |
| worker | `keel.user` | proxy listeners | what the control worker hands it |

Started without root, there is nothing to drop: every process runs as the
invoking user. See [Security](security.md#processes-and-what-each-can-read).

---

## Who binds, who serves

The master binds every listening socket before it forks, so ports below 1024
need root in one process only, and a child that dies never leaves a port
unbound. The children inherit the sockets they serve.

```
 master binds                        served by
 ------------------------------      ----------------------------------
 proxy listeners (TCP)  ---------->  every worker, one shared socket;
                                     the kernel spreads accept()
 proxy listeners (UDP)  ---------->  one SO_REUSEPORT socket per worker;
                                     a client's flow stays on one worker
 control socket         ---------->  control worker
 control.remote         ---------->  control worker
 metrics.address        ---------->  control worker
 ICMP sockets           ---------->  one per worker, for icmp health checks
```

On Linux this holds whether or not Keel started as root. On other platforms
each worker binds its own proxy listeners with `SO_REUSEPORT`.

The processes talk over Unix sockets:

```
 master ---- socketpair ----> control worker      "SIGHUP arrived",
                              (one way)           "worker N was restarted"

 control worker ---- workers/worker-<N>.sock ---> worker N
                     applied config, ACME certificates, drains,
                     status questions, liveness pings
```

The master only ever writes to the control worker; nothing it reads comes
from a network peer or another process.

---

## A request

```
 client
   |
   v
 listener (bound by the master, accepted by one worker)
   |
   |  [PROXY protocol header read first, if the listener expects one]
   |
   +-- HTTP/HTTPS: TLS (SNI picks the certificate), vhost by Host,
   |               route by path prefix, cache lookup, pool
   +-- tcp_pool:   passthrough, terminate or reencrypt; no vhosts
   +-- udp_pool:   one flow per client ip:port until it is idle
   |
   v
 pool: pick a backend that is active, healthy and not ejected
   |                       (no backend left: HTTP 503, TCP closed,
   |                        UDP dropped)
   v
 backend
   |
   v
 response to the client; access log line; counters in shared memory
```

Everything in this path runs inside one worker. Workers share no request
state: each keeps its own upstream connection pool and passive ejection
view.

---

## Configuration

`node.hcl` describes the node and is read at start only. The config
directory (`/etc/keel/config/`) describes the traffic and is replicated: it
becomes a numbered version in the Raft log, and every node applies the same
version.

```
 edit files in /etc/keel/config/
            |
            |  SIGHUP, keel config reload, or keel config push <dir>
            v
 control worker: load and validate ---- invalid: refused, nothing changes
            |
            v
 Raft: commit the file set as version V (needs a majority of the cluster)
            |
            v
 every node's control worker:
   build version V against this node's node.hcl
            |
            v
   hand V to every worker over its socket ---> workers swap routing,
            |                                  certificates and drains
            |                                  without dropping connections
            v
   write the files back to /etc/keel/config/ and record V as applied
```

A worker started later, after a crash say, gets the applied version from
the control worker. Some settings take a restart; see
[Configuration](configuration.md#hot-reload).

---

## Several nodes

A cluster is several of the nodes above, joined by Raft over mutual TLS on
the peer port (`cluster.addr`, for example `:7654`). Only the control
workers take part; workers never talk to other nodes.

```
                 clients (DNS, an L4 load balancer, anycast ...)
                 |                   |                   |
                 v                   v                   v
        +----------------+  +----------------+  +----------------+
        | node A         |  | node B         |  | node C         |
        |  master        |  |  master        |  |  master        |
        |  control  <----+--+->  control <---+--+-> control      |
        |  workers       |  |  workers       |  |  workers       |
        +----------------+  +----------------+  +----------------+
                     \             Raft, mTLS            /
                      `---------- peer port -------------'
```

- Traffic never needs the cluster: each node serves from the last version it
  applied, even cut off from the others.
- Writes do: a config push, a drain or a membership change is committed by a
  majority, then applied on every node.
- Any node takes commands; one that is not the leader forwards writes to it.
- Every node runs its own health checks and acts on its own view of the
  backends.

See [Clustering](cluster.md).

---

## Health, drains and ejection

| State | Decided by | Seen by |
|---|---|---|
| Drain | an operator command, committed through Raft | every worker of every node |
| Active health | health checks, run by every worker for itself | that worker |
| Passive ejection | real traffic failing to connect, per worker | that worker |

Every worker probes every backend, so a backend receives one probe per
worker per interval.

---

## Metrics

```
 worker 0 --+
 worker 1 --+-- count into their own stripe --> shared memory region
 worker N --+                                    (mapped by the master
                                                  before forking)
 master ------- restarts, workers running ----->        |
                                                        v
 Prometheus --- GET /metrics ---> control worker: sums the stripes
```

One endpoint per node, node totals only. A replacement worker takes over its
predecessor's stripe, so counters only reset when the master restarts. See
[Metrics](metrics.md#one-endpoint-per-node).

---

## Logs

```
 workers         access log: one JSON line per request, connection or flow
                 -> files under access_log.dir, or stdout with dir: "-"

 every process   Keel's own log: startup, health changes, reloads, errors
                 -> stderr, as text
```

In a container, `dir: "-"` puts the access log on stdout and leaves stderr
to Keel's own log. See [Access logging](access-logging.md).

---

## When a process dies

| Process | Traffic | What happens |
|---|---|---|
| a worker | the other workers keep serving; its listeners stay bound | the master forks a replacement under the same index: same UDP socket, same metrics stripe; it gets the applied config from the control worker. A worker that ran 10s or more is replaced at once; one that died sooner waits 1s, doubling up to 30s per index |
| the control worker | unaffected: workers keep the last applied version | restarted like a worker: at once after 10s or more, else after 1s doubling up to 30s; control socket and metrics answer again once it is back |
| the master | stopped with SIGTERM, SIGINT or SIGQUIT, it stops its children first; killed outright, the control worker stops (its link to the master closes) and the workers keep serving on their own until they are stopped | restart Keel (systemd, the container runtime) |
| a whole node | other nodes keep serving; the cluster keeps a majority or goes read-only | the node rejoins from its stored state when it starts |

---

## Paths and ports

| What | Default | Set by |
|---|---|---|
| Node file | `/etc/keel/node.hcl` | `--config` |
| Config directory | `/etc/keel/config/` | `keel.config_dir` |
| State directory (node key, Raft store, ACME) | `/var/lib/keel` | `keel.state_dir` |
| Control socket | `/var/run/keel/keel.sock` | `keel.control_socket` |
| Worker sockets | `/var/run/keel/workers/` | next to the control socket |
| Metrics | `127.0.0.1:10790` | `metrics.address` |
| Remote control | off | `control.remote.address` |
| Raft peers | off | `cluster.addr` |
| Access logs | `/var/log/keel` | `access_log.dir` |
