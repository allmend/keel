//! Root master, unprivileged workers: the master binds privileged TCP and UDP
//! ports as root, workers serve as the configured user, and a killed worker
//! is replaced under its old index.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, log_field, wait_until, Proc, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.83.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.83.20 } }
  keel:
    image: {image}
    init: true
    command: ["--config", "/etc/keel/keel.yaml"]
    environment: [NO_COLOR=1]
    volumes: ["./keel.yaml:/etc/keel/keel.yaml:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.83.10 } }
"#;

const CONFIG: &str = r#"
keel:
  workers: 2
  user: nobody
  group: nogroup
listeners:
  - address: 0.0.0.0:80
  - address: 0.0.0.0:53
    udp_pool: dns
pools:
  web:
    backends:
      - address: 172.29.83.20:80
  dns:
    backends:
      - address: 172.29.83.20:53
vhosts:
  - host: "*"
    pool: web
"#;

/// uid of `nobody` in the debian image.
const NOBODY: u32 = 65534;

fn serving(stack: &Stack) -> Result<()> {
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    Ok(())
}

/// The keel master (parent is init) and its workers.
fn keel_procs(stack: &Stack) -> Result<(Proc, Vec<Proc>)> {
    let procs = stack.procs("keel")?;
    let keel: Vec<&Proc> = procs.iter().filter(|p| p.cmd.starts_with("/usr/local/bin/keel")).collect();
    let master = keel
        .iter()
        .find(|p| !keel.iter().any(|q| q.pid == p.ppid))
        .ok_or_else(|| anyhow::anyhow!("no keel master in {procs:?}"))?;
    let workers = keel.iter().filter(|p| p.ppid == master.pid).map(|p| (*p).clone()).collect();
    Ok(((*master).clone(), workers))
}

#[test]
#[ignore = "needs Docker"]
fn root_master_binds_privileged_ports_and_workers_drop_privileges() -> Result<()> {
    let stack = Stack::up("privileges", COMPOSE, &[("keel.yaml", CONFIG.to_owned())])?;
    serving(&stack)?;

    let logs = stack.logs("keel")?;
    assert!(
        logs.lines().any(|l| l.contains("master: bound listener") && log_field(l, "address") == Some("0.0.0.0:80")),
        "master did not bind :80 itself"
    );
    assert!(
        logs.lines().any(|l| l.contains("master: bound UDP listener") && log_field(l, "address") == Some("0.0.0.0:53")),
        "master did not bind UDP :53 itself"
    );

    let (master, workers) = keel_procs(&stack)?;
    assert_eq!(master.uid, 0, "master runs as root");
    assert_eq!(workers.len(), 2, "two workers: {workers:?}");
    for w in &workers {
        assert_eq!(w.uid, NOBODY, "worker {} runs as nobody", w.pid);
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn killed_worker_is_replaced_under_its_index() -> Result<()> {
    let stack = Stack::up("respawn", COMPOSE, &[("keel.yaml", CONFIG.to_owned())])?;
    serving(&stack)?;

    let logs = stack.logs("keel")?;
    let victim = logs
        .lines()
        .filter(|l| l.contains("master: worker started"))
        .find(|l| log_field(l, "index") == Some("1"))
        .and_then(|l| log_field(l, "pid"))
        .ok_or_else(|| anyhow::anyhow!("no start line for worker 1"))?
        .to_owned();

    // The slim image has no kill(1); the shell builtin does the job.
    let out = stack.exec("keel", &["sh", "-c", &format!("kill -9 {victim}")])?;
    assert!(out.status.success(), "kill failed: {}", String::from_utf8_lossy(&out.stderr));

    wait_until("replacement of worker 1", Duration::from_secs(20), || {
        let logs = stack.logs("keel")?;
        Ok(logs
            .lines()
            .any(|l| l.contains("master: replacement worker started") && log_field(l, "index") == Some("1"))
            .then_some(()))
    })?;

    let (_, workers) = wait_until("two workers again", Duration::from_secs(20), || {
        let (master, workers) = keel_procs(&stack)?;
        Ok((workers.len() == 2).then_some((master, workers)))
    })?;
    assert!(workers.iter().all(|w| w.pid.to_string() != victim), "killed pid is gone");
    assert!(workers.iter().all(|w| w.uid == NOBODY), "replacement runs as nobody");
    serving(&stack)?;
    Ok(())
}
