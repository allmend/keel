//! UDP flows across a worker replacement: the new worker takes each flow
//! with its upstream socket, so the flow keeps its backend and the backend
//! sees the same source address.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{wait_until, Runtime, Stack};

// Each backend answers with its name and the address the datagram came from.
const ECHO: &str = "import socket,sys\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\ns.bind(('0.0.0.0',9000))\nwhile True:\n d,a=s.recvfrom(2048)\n s.sendto(f'{sys.argv[1]} {a[0]}:{a[1]}'.encode(),a)\n";

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.102.0/24
services:
  a:
    image: python:3.12-alpine
    command: ["python", "-c", "{echo}", "a"]
    networks: { net: { ipv4_address: 172.29.102.20 } }
  b:
    image: python:3.12-alpine
    command: ["python", "-c", "{echo}", "b"]
    networks: { net: { ipv4_address: 172.29.102.21 } }
  client:
    image: python:3.12-alpine
    command: ["sleep", "3600"]
    networks: { net: { ipv4_address: 172.29.102.30 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro", "./next:/next:ro"]
    networks: { net: { ipv4_address: 172.29.102.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}
"#;

const CONFIG: &str = r#"
listener "echo" {
  protocol  = "udp"
  addresses = ["0.0.0.0"]
  port      = 5300
  pool      = "echo"
}

pool "echo" {
  algorithm = "round_robin"

  backend "172.29.102.20:9000" {}
  backend "172.29.102.21:9000" {}
}
"#;

// From the same client address every time: one flow.
const SEND: &str = "import socket\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\ns.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\ns.bind(('0.0.0.0',40000))\ns.settimeout(3)\ns.sendto(b'ping',('172.29.102.10',5300))\nprint(s.recvfrom(2048)[0].decode())\n";

/// The answering backend and the source address it saw.
fn ask(stack: &Stack) -> Result<String> {
    let out = stack.exec("client", &["python", "-c", SEND])?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
#[ignore = "needs Docker"]
fn a_flow_keeps_its_backend_and_socket_across_new_workers() -> Result<()> {
    let compose = COMPOSE.replace("{echo}", &ECHO.replace('\n', "\\n").replace('"', "\\\""));
    let next = format!("{CONFIG}\nkeel {{\n  udp_flow_timeout = \"5m\"\n}}\n");
    let stack = Stack::up(
        "udp-handover",
        &compose,
        &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned()), ("next/keel.hcl", next)],
    )?;
    let before = wait_until("an answer through keel", Duration::from_secs(60), || Ok(ask(&stack).ok()))?;
    assert_eq!(ask(&stack)?, before, "one flow: same backend, same upstream socket");

    // The keel block changes: new workers.
    let out = stack.exec("keel", &["keel", "config", "push", "/next"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_until("new workers", Duration::from_secs(30), || {
        Ok(stack.logs("keel")?.contains("master: new workers ready").then_some(()))
    })?;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(ask(&stack)?, before, "after the switch: the same backend, from the same socket");
    // The old worker logs once the new one has taken the flows.
    wait_until("the hand-over logged", Duration::from_secs(10), || {
        Ok(stack.logs("keel")?.contains("udp: flows handed to the new worker").then_some(()))
    })?;
    Ok(())
}
