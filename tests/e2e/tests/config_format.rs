//! The configuration language: a node configured in HCL, with locals shared
//! across the config directory, serves traffic. `keel config validate` is
//! silent on a good config, names the file, line and column of a mistake,
//! lists files not in canonical format, prints what it loads, and compares
//! with what the running node serves. Addresses are IPs: a host name is
//! refused, even one the container could resolve.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.88.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.88.21 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes:
      - ./node.hcl:/etc/keel/node.hcl:ro
      - ./config:/etc/keel/config:ro
      - ./broken:/broken:ro
      - ./broken-node.hcl:/broken-node.hcl:ro
      - ./messy:/messy:ro
      - ./messy-node.hcl:/messy-node.hcl:ro
      - ./edited:/edited:ro
      - ./edited-node.hcl:/edited-node.hcl:ro
      - ./named:/named:ro
      - ./named-node.hcl:/named-node.hcl:ro
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.88.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}
"#;

// The local is defined in keel.hcl and used in another file.
const ROOT: &str = r#"
locals {
  backend_port = 80
  check_every  = "1s"
}

listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

vhost "*" {
  pool = "web"
}
"#;

const POOL: &str = r#"
pool "web" {
  health_check "http" {
    path     = "/health"
    interval = local.check_every
  }

  backend "172.29.88.21:80" {}
}
"#;

// Line 4, column 5: an attribute a pool does not have.
const BROKEN: &str = r#"pool "web" {
  backend "172.29.88.21:80" {}

    algorithmn = "round_robin"
}

listener "http-80" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}
"#;

const MESSY: &str = "pool \"web\" {\nalgorithm=\"round_robin\"\n  backend \"172.29.88.21:80\" { weight=2 }\n}\n";

/// A node file for `dir`, otherwise the same as NODE.
fn node_for(dir: &str) -> String {
    NODE.replace("  group   = \"nogroup\"\n", &format!("  group   = \"nogroup\"\n\n  config_dir = \"{dir}\"\n"))
}

#[test]
#[ignore = "needs Docker"]
fn a_node_configured_in_hcl_serves_traffic_and_validate_checks_files() -> Result<()> {
    let stack = Stack::up(
        "config-format",
        COMPOSE,
        &[
            ("node.hcl", NODE.to_owned()),
            ("config/keel.hcl", ROOT.to_owned()),
            ("config/pools/web.hcl", POOL.to_owned()),
            ("broken-node.hcl", node_for("/broken")),
            ("broken/keel.hcl", BROKEN.to_owned()),
            ("messy-node.hcl", node_for("/messy")),
            ("messy/keel.hcl", MESSY.to_owned()),
            ("edited-node.hcl", node_for("/edited")),
            ("edited/keel.hcl", ROOT.to_owned()),
            ("named-node.hcl", node_for("/named")),
            ("named/keel.hcl", ROOT.to_owned()),
            ("named/pools/web.hcl", POOL.replace("172.29.88.21:80", "backend:80")),
            ("edited/pools/web.hcl", POOL.replace("  backend \"172.29.88.21:80\" {}\n", "  backend \"172.29.88.21:80\" {}\n  backend \"172.29.88.22:80\" {}\n")),
        ],
    )?;
    let addr = stack.addr("keel", 80)?;
    let (_, body) = wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    assert!(body.contains("IP: 172.29.88.21"), "{body}");
    let run = |args: &[&str]| -> Result<(Option<i32>, String, String)> {
        let mut full = vec!["keel", "config", "validate"];
        full.extend_from_slice(args);
        let out = stack.exec("keel", &full)?;
        Ok((out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
    };

    // The running config: valid, and nothing to say.
    let (code, out, err) = run(&[])?;
    assert_eq!((code, out.as_str(), err.as_str()), (Some(0), "", ""));

    // A mistake: exit 1, and where it is on stderr.
    let (code, out, err) = run(&["/broken-node.hcl"])?;
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("/broken/keel.hcl:4:5: unknown attribute `algorithmn`"), "{err}");

    // Not formatted: valid, listed, and exit 3 with --check.
    let (code, out, _) = run(&["/messy-node.hcl"])?;
    assert_eq!((code, out.as_str()), (Some(0), ""));
    let (code, out, err) = run(&["/messy-node.hcl", "--list", "--check"])?;
    assert_eq!(code, Some(3), "{err}");
    assert_eq!(out, "/messy/keel.hcl\n");

    // Every file that is loaded.
    let (_, out, _) = run(&["--full"])?;
    assert!(out.contains("# file /etc/keel/node.hcl\n") && out.contains("# file /etc/keel/config/pools/web.hcl\n"), "{out}");
    assert!(out.find("config/keel.hcl").unwrap() < out.find("pools/web.hcl").unwrap(), "load order: {out}");

    // Against what the node serves: the new backend waits for a restart.
    let (code, out, err) = run(&["/edited-node.hcl", "--diff"])?;
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("+  backend \"172.29.88.22:80\" {}"), "{out}");
    assert!(out.contains("restart pool \"web\": backend \"172.29.88.22:80\" added"), "{out}");
    assert!(out.contains("restart the node file"), "{out}");
    assert!(!out.contains("\x1b["), "no colors when not on a terminal: {out}");

    // A host name, though Docker's DNS knows it: refused, and not pushed.
    let (code, _, err) = run(&["/named-node.hcl"])?;
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("pool 'web': backend: 'backend:80' is not an ip:port"), "{err}");
    let out = stack.exec("keel", &["keel", "config", "push", "/named"])?;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("'backend:80' is not an ip:port"), "{err}");
    assert_eq!(http_get(addr, "e2e.test", "/")?.0, 200);
    Ok(())
}
