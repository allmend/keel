//! The release image: FROM scratch, a static MUSL keel, started as root. The
//! master drops workers to keel (10001) and the control worker to
//! keel-control (10002) from the image's own passwd and group files, and the
//! node serves traffic and answers client commands without a shell.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

// The scratch image has no shell: `peek` shares its process namespace to
// list the processes.
const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.95.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.95.20 } }
  keel:
    image: {release_image}
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.95.10 } }
  peek:
    image: busybox:1.37
    command: ["sleep", "3600"]
    pid: "service:keel"
    depends_on: [keel]
    network_mode: none
"#;

const NODE: &str = r#"
keel {
  workers = 2
}
"#;

const CONFIG: &str = r#"
listener "http" "0.0.0.0:80" {}

pool "web" {
  backend "172.29.95.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

#[test]
#[ignore = "needs Docker"]
fn the_scratch_image_runs_as_root_and_drops_to_its_users() -> Result<()> {
    let stack = Stack::up("release-image", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    let (_, body) = wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    assert!(body.contains("IP: 172.29.95.20"), "{body}");

    let procs = stack.procs("peek")?;
    let keel: Vec<_> = procs.iter().filter(|p| p.cmd.starts_with("/keel")).collect();
    let master = keel.iter().find(|p| p.uid == 0).ok_or_else(|| anyhow::anyhow!("no root master in {procs:?}"))?;
    let children: Vec<u32> = keel.iter().filter(|p| p.ppid == master.pid).map(|p| p.uid).collect();
    assert_eq!(children.iter().filter(|&&uid| uid == 10002).count(), 1, "one control worker: {procs:?}");
    assert_eq!(children.iter().filter(|&&uid| uid == 10001).count(), 2, "two workers: {procs:?}");

    let out = stack.exec("keel", &["/keel", "status"])?;
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("172.29.95.20:80"), "{out:?}");
    Ok(())
}
