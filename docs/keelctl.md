# Remote Control (keelctl)

`keelctl` controls a running Keel node or cluster from anywhere: an operator
workstation, a CI job, a bastion. It speaks the same control protocol as the
local `keel` subcommands, over TCP with mandatory mTLS. Binaries ship for
Linux, macOS, and FreeBSD.

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

Each node's control CA lives in its own `ca_dir`, so a keelconfig
authenticates to the nodes that share that CA. To use one keelconfig for the
whole cluster, place the same `ca.crt`/`ca.key` in every node's `ca_dir`
(config management, mounted secret). Automatic control-CA replication via
Raft is planned.

## Revocation

There is no per-certificate revocation. To invalidate issued credentials,
delete `ca_dir` and restart Keel — a new CA is generated, all previously
issued keelconfigs stop working, and each operator needs a new one.

## Audit log

Every remote command appears in Keel's app log (stderr):

```
INFO keel::control: control: remote command client="john@203.0.113.7:52144" command="backend_drain"
```

Local Unix-socket commands are not attributed — the socket is already
restricted to owner+group on the node.
