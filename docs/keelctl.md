# Remote Control (keelctl)

`keelctl` controls a running Keel node or cluster from an operator
workstation, a CI job, or a bastion host. It speaks the same control protocol
as the local `keel` subcommands, over TCP with mandatory mTLS. Binaries ship
for Linux, macOS, and FreeBSD.

## Security

keelctl communicates over mTLS. The remote listener only accepts connections
presenting a client certificate signed by the node's control CA; the
certificate CN is the operator name, and every command is audit-logged as
`name@address command`. Connections without a valid client certificate fail
at the TLS handshake. There is no password mode and no plaintext mode.

The optional `allow` list additionally restricts which source CIDRs the
listener accepts. Source addresses are not reliable behind NAT or a
Kubernetes Service, so this narrows exposure but never replaces mTLS.

## Enabling the remote listener

```yaml
# keel.yaml
control:
  remote:
    address: 0.0.0.0:10789
    allow:                       # optional; empty = any source
      - 10.1.2.0/24
    # ca_dir: /var/lib/keel/control   # default
```

Remote control is off unless `control.remote` is configured. The local Unix
socket (`keel.control_socket`) is always on and unchanged — `keel status` on
the node works exactly as before.

`address` must be a literal `ip:port`; a hostname is rejected at startup (see
[Configuration](configuration.md#control)). The listener is owned by the root
master process, which is a deliberate trade documented in
[Security](security.md#the-remote-control-listener-runs-as-root).

On first start with `control.remote` set (or on the first
`keel credentials create`), Keel generates the control CA in `ca_dir`:
`ca.crt` and `ca.key` (0600, directory 0700). The listener's server
certificate is issued from it in memory at each start.

## Creating credentials

Run once on the node — via SSH, `docker exec`, or `kubectl exec`:

```bash
keel credentials create john --endpoint lb1.example.com:10789 > keelconfig
```

The output is a **keelconfig**: one YAML file with the endpoint, the control
CA certificate, and a client certificate + private key for operator `john`.
Treat it like a private key. Copy it to the workstation or CI secret store —
after this, no shell access to the node is needed.

`--endpoint` is written into the keelconfig verbatim: it is the address the
operator dials, which is usually not the bind address (DNS name, VIP,
port-forward). The TLS identity keelctl verifies is the fixed name
`keel-control` against the CA, so any reachable route to the node works.

## Using keelctl

```bash
keelctl status
keelctl backend list --pool web
keelctl backend drain 10.0.0.1:8080 --wait
keelctl config reload
keelctl config push keel.yaml
keelctl cluster status
keelctl cluster stepdown
```

Same commands, same output as the on-node `keel` CLI.

The keelconfig is resolved in order:

1. `--config <path>`
2. `KEEL_CONFIG` environment variable
3. `./keelconfig` in the working directory
4. `~/.keel/config`

## Cluster mode

Every node with `control.remote` configured listens; commands that change
cluster state (`config push`, `stepdown`) are forwarded to the leader
internally, so the endpoint does not need to be the leader and keeps working
across failovers.

The control CA is cluster-wide. When a cluster forms, the leader commits
its control CA (certificate and key) to the Raft log; every node, including
one that joins later, writes it into its own `ca_dir` and re-keys its remote
listener. One keelconfig therefore authenticates to every node, and
`keel credentials create` produces a valid keelconfig on any node.

A node that starts with a different local CA (for example a node previously
run standalone) adopts the cluster's and logs that it did; credentials
issued by its old CA stop working. Create credentials once the cluster is
up rather than on a node before it joins, for the same reason.

## Revocation

There is no per-certificate revocation. To invalidate issued credentials,
delete `ca_dir` and restart Keel — a new CA is generated, all previously
issued keelconfigs stop working, and each operator needs a new one.

In cluster mode the CA in the Raft log would repopulate a deleted `ca_dir`
on the next start, and the log is in memory. Rotation therefore means:
stop every node, delete `ca_dir` on every node, start the cluster again.
The new leader generates and publishes a fresh CA.

## Audit log

Every remote command appears in Keel's app log (stderr):

```
INFO keel::control: control: remote command client="john@203.0.113.7:52144" command="backend_drain"
```

Local Unix-socket commands are not attributed — the socket is already
restricted to owner+group on the node.
