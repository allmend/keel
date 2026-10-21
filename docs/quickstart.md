# Quickstart

## Build

```bash
cargo build --release
```

The binary is `target/release/keel`.

## Configure

A node file and the config directory it names. In the current directory, `node.hcl`:

```hcl
keel {
  config_dir = "config"
}
```

and `config/keel.hcl`:

```hcl
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0", "::"]
  port      = 8080
}

pool "web" {
  backend "127.0.0.1:3000" {}
}

vhost "*" {
  pool = "web"
}
```

Port 8080, every request to `127.0.0.1:3000`.

```bash
./keel --config node.hcl config validate
```

## Run

```bash
./keel --config node.hcl
curl http://localhost:8080/
```

Started as root, Keel binds the ports and the control socket, then runs the control worker as `keel-control` and the workers as `keel`; both users must exist. Started as another user, every process runs as that user. The node is a cluster of one.

## Next

- [Architecture](architecture.md)
- [Configuration](configuration.md)
- [Virtual hosts](virtual-hosts.md)
- [ACME](acme.md)
- [Load balancing](load-balancing.md)
- [Health checks](health-checks.md)
- [TCP proxying](tcp-proxying.md)
- [UDP proxying](udp-proxying.md)
- [Caching](caching.md)
- [Cluster](cluster.md)
- [CLI](cli.md)
- [keelctl](keelctl.md)
- [Access logging](access-logging.md)
- [Metrics](metrics.md)
- [Security](security.md)
