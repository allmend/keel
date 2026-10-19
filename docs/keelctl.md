# keelctl

`keelctl` runs the `keel` client commands against a node or cluster over TCP with mTLS. Binaries for Linux, macOS and FreeBSD.

## Security

The listener accepts only client certificates signed by the node's control CA; the CN is the operator name, and every command is logged with it. Nothing else is accepted.

`allow` restricts source CIDRs on top of that.

## Listener

```hcl
# /etc/keel/node.hcl
control {
  remote {
    address = "0.0.0.0:10789"
    allow   = ["10.1.2.0/24"]
  }
}
```

Off without `control.remote`; the local control socket is always on. `address` is a literal `ip:port`. The master binds it, the control worker serves it.

The control CA is generated on the first start with `control.remote` (or the first `keel credentials create`) in `control/` in `keel.state_dir`: `ca.crt`, `ca.key` (0600). The listener's certificate is issued from it at each start.

## Credentials

On the node:

```bash
keel credentials create john --endpoint lb1.example.com:10789 > keelconfig
```

A keelconfig is one YAML file: the endpoint, the control CA certificate, and a client certificate and private key for `john`.

`--endpoint` is written as given. keelctl verifies the server as `keel-control` against the CA, so any route to the node works (DNS name, VIP, port-forward).

## Use

```bash
keelctl status
keelctl backend list --pool web
keelctl backend drain 10.0.0.1:8080 --wait
keelctl config reload
keelctl config push ./config
keelctl cluster status
keelctl cluster stepdown
```

The keelconfig is found in this order:

1. `--config <path>`
2. `KEEL_CONFIG` environment variable
3. `./keelconfig` in the working directory
4. `~/.keel/config`

## Cluster

Every node with `control.remote` listens. Writes sent to a follower (`cluster stepdown`, `cluster remove`, `config push`, `config reload`, `backend drain`, `credentials revoke-all`) go to the leader; `status` and `backend list` answer for the receiving node.

The leader commits its control CA to Raft; every node, later joiners included, takes it and re-keys its listener. One keelconfig works on every node, and `keel credentials create` works on any node. A node joining with another CA adopts the cluster's and logs it; keelconfigs from its old CA stop working.

## Revocation

`keel credentials revoke-all` replaces the control CA through Raft; every keelconfig issued so far stops working. There is no per-certificate revocation.

## Audit log

Every remote command is logged:

```
INFO keel::control: control: remote command client="john@203.0.113.7:52144" command="backend_drain"
```

Commands on the local socket are not attributed.
