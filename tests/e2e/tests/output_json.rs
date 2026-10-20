//! `--output json`: every keel and keelctl command prints its result as JSON,
//! one object per line, and fails with a non-zero exit as in text mode.

use std::time::Duration;

use anyhow::{Context, Result};
use keel_e2e::{http_get, wait_until, Runtime, Stack};
use serde_json::Value;

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.93.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.93.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes:
      - ./node.hcl:/etc/keel/node.hcl:ro
      - ./config:/etc/keel/config:ro
      - ./broken-node.hcl:/broken-node.hcl:ro
      - ./broken:/broken:ro
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.93.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}

control {
  remote {
    address = "127.0.0.1:10789"
  }
}
"#;

const CONFIG: &str = r#"
listener "http" "0.0.0.0:80" {}

pool "web" {
  backend "172.29.93.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

const BACKEND: &str = "172.29.93.20:80";

/// Run a command in the keel container: exit status and stdout lines as JSON.
fn run(stack: &Stack, args: &[&str]) -> Result<(Option<i32>, Vec<Value>)> {
    let out = stack.exec("keel", args)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = stdout
        .lines()
        .map(|l| serde_json::from_str(l).with_context(|| format!("{args:?}: not JSON: {l:?}\nstderr: {}", String::from_utf8_lossy(&out.stderr))))
        .collect::<Result<_>>()?;
    Ok((out.status.code(), lines))
}

/// A command that succeeds with one JSON object.
fn one(stack: &Stack, args: &[&str]) -> Result<Value> {
    let (code, mut lines) = run(stack, args)?;
    anyhow::ensure!(code == Some(0) && lines.len() == 1, "{args:?}: exit {code:?}, {lines:?}");
    Ok(lines.remove(0))
}

#[test]
#[ignore = "needs Docker"]
fn every_command_prints_json() -> Result<()> {
    let node_for_broken = NODE.replace("  group   = \"nogroup\"\n", "  group   = \"nogroup\"\n\n  config_dir = \"/broken\"\n");
    let stack = Stack::up(
        "output-json",
        COMPOSE,
        &[
            ("node.hcl", NODE.to_owned()),
            ("config/keel.hcl", CONFIG.to_owned()),
            ("broken-node.hcl", node_for_broken),
            ("broken/keel.hcl", "pool \"web\" {\n  algorithmn = \"x\"\n}\n".to_owned()),
        ],
    )?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    let status = one(&stack, &["keel", "-o", "json", "status"])?;
    assert_eq!(status["pools"][0]["name"], "web", "{status}");
    let list = one(&stack, &["keel", "backend", "list", "--pool", "web", "--output", "json"])?;
    assert_eq!(list["backends"][0]["address"], BACKEND, "{list}");
    let cluster = wait_until("the node leads", Duration::from_secs(30), || {
        Ok(one(&stack, &["keel", "-o", "json", "cluster", "status"]).ok().filter(|c| c["role"] == "leader"))
    })?;
    assert_eq!(cluster["membership"].as_array().map(Vec::len), Some(1), "{cluster}");

    // A waited drain: one line per response, the last one done.
    let (code, lines) = run(&stack, &["keel", "-o", "json", "backend", "drain", BACKEND, "--wait"])?;
    assert_eq!(code, Some(0));
    assert_eq!(lines[0]["pools"][0], "web", "{lines:?}");
    assert_eq!(lines.last().map(|l| l["done"].clone()), Some(Value::Bool(true)), "{lines:?}");
    let activated = one(&stack, &["keel", "-o", "json", "backend", "activate", BACKEND])?;
    assert_eq!(activated["pools"][0], "web", "{activated}");
    one(&stack, &["keel", "-o", "json", "config", "reload"])?;

    // A failure: non-zero, nothing on stdout.
    let (code, lines) = run(&stack, &["keel", "-o", "json", "backend", "activate", "192.0.2.1:80"])?;
    assert_eq!((code, lines.len()), (Some(1), 0));

    // validate: valid or not, one object.
    let valid = one(&stack, &["keel", "-o", "json", "config", "validate"])?;
    assert_eq!(valid, serde_json::json!({ "valid": true, "unformatted": [] }));
    let (code, lines) = run(&stack, &["keel", "-o", "json", "config", "validate", "/broken-node.hcl"])?;
    assert_eq!(code, Some(1));
    assert_eq!(lines[0]["valid"], false, "{lines:?}");
    assert!(lines[0]["error"].as_str().is_some_and(|e| e.contains("/broken/keel.hcl:2:3: unknown attribute `algorithmn`")), "{lines:?}");

    // Credentials as JSON, and keelctl with a keelconfig.
    let creds = one(&stack, &["keel", "-o", "json", "credentials", "create", "ops", "--endpoint", "127.0.0.1:10789"])?;
    assert_eq!(creds["endpoint"], "127.0.0.1:10789", "{creds}");
    let out = stack.exec("keel", &["bash", "-c", "keel credentials create ops --endpoint 127.0.0.1:10789 > /tmp/keelconfig 2>/dev/null"])?;
    assert!(out.status.success());
    let remote = one(&stack, &["keelctl", "-c", "/tmp/keelconfig", "-o", "json", "status"])?;
    assert_eq!(remote["pools"], status["pools"], "keelctl prints what keel prints");
    let remote = one(&stack, &["keelctl", "-c", "/tmp/keelconfig", "-o", "json", "backend", "list", "--pool", "web"])?;
    assert_eq!(remote["backends"][0]["address"], BACKEND, "{remote}");
    Ok(())
}
