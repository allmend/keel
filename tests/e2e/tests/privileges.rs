//! One node, no `cluster:` section: a cluster of one. The root master binds
//! privileged TCP and UDP ports and the control socket, the control worker
//! runs as its own user and holds the state, the workers serve as the
//! configured user and cannot read it, and every child that dies is
//! replaced — the drains it knew survive.

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
    environment: [NO_COLOR=1]
    volumes: ["./node.yaml:/etc/keel/node.yaml:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.83.10 } }
"#;

const NODE: &str = r#"
keel:
  workers: 2
  user: nobody
  group: nogroup
"#;

const CONFIG: &str = r#"
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

/// uid of `keel-control`, created in the test image.
fn control_uid(stack: &Stack) -> Result<u32> {
    let out = stack.exec("keel", &["id", "-u", "keel-control"])?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().parse()?)
}

/// The keel master (parent is init), its control worker and its workers.
fn keel_procs(stack: &Stack) -> Result<(Proc, Vec<Proc>, Vec<Proc>)> {
    let control = control_uid(stack)?;
    let procs = stack.procs("keel")?;
    let keel: Vec<&Proc> = procs.iter().filter(|p| p.cmd.starts_with("/usr/local/bin/keel")).collect();
    let master = keel
        .iter()
        .find(|p| !keel.iter().any(|q| q.pid == p.ppid))
        .ok_or_else(|| anyhow::anyhow!("no keel master in {procs:?}"))?;
    let (controls, workers) = keel.iter().filter(|p| p.ppid == master.pid).map(|p| (*p).clone()).partition(|p| p.uid == control);
    Ok(((*master).clone(), controls, workers))
}

fn keel(stack: &Stack, args: &[&str]) -> Result<String> {
    let mut full = vec!["keel"];
    full.extend_from_slice(args);
    let out = stack.exec("keel", &full)?;
    anyhow::ensure!(out.status.success(), "keel {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether every worker keeps the backend out of rotation: `keel backend
/// list` reports the least drained state any worker has.
fn drained(stack: &Stack) -> Result<bool> {
    let list = keel(stack, &["backend", "list", "--pool", "web"])?;
    let line = list.lines().find(|l| l.contains("172.29.83.20:80")).unwrap_or_default();
    Ok(line.contains("draining") || line.contains("removed"))
}

#[test]
#[ignore = "needs Docker"]
fn root_master_binds_privileged_ports_and_workers_drop_privileges() -> Result<()> {
    let stack = Stack::up("privileges", COMPOSE, &[("node.yaml", NODE.to_owned()), ("config/keel.yaml", CONFIG.to_owned())])?;
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

    let (master, controls, workers) = keel_procs(&stack)?;
    assert_eq!(master.uid, 0, "master runs as root");
    assert_eq!(controls.len(), 1, "one control worker: {controls:?}");
    assert_eq!(workers.len(), 2, "two workers: {workers:?}");
    for w in &workers {
        assert_eq!(w.uid, NOBODY, "worker {} runs as nobody", w.pid);
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn killed_worker_is_replaced_under_its_index() -> Result<()> {
    let stack = Stack::up("respawn", COMPOSE, &[("node.yaml", NODE.to_owned()), ("config/keel.yaml", CONFIG.to_owned())])?;
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

    let workers = wait_until("two workers again", Duration::from_secs(20), || {
        let (_, _, workers) = keel_procs(&stack)?;
        Ok((workers.len() == 2).then_some(workers))
    })?;
    assert!(workers.iter().all(|w| w.pid.to_string() != victim), "killed pid is gone");
    assert!(workers.iter().all(|w| w.uid == NOBODY), "replacement runs as nobody");
    serving(&stack)?;
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_single_node_is_a_cluster_of_one_without_a_peer_port() -> Result<()> {
    let stack = Stack::up("single", COMPOSE, &[("node.yaml", NODE.to_owned()), ("config/keel.yaml", CONFIG.to_owned())])?;
    serving(&stack)?;
    let status = wait_until("the node leads its own cluster", Duration::from_secs(30), || {
        Ok(keel(&stack, &["cluster", "status"]).ok().filter(|s| s.contains("leader")))
    })?;
    assert_eq!(status.matches("(voter)").count(), 1, "one member:\n{status}");
    assert!(!stack.logs("keel")?.contains("peer listener ready"), "nothing listens for peers");
    // Changes are committed like on any cluster: a reload is a new version.
    keel(&stack, &["config", "reload"])?;
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn workers_cannot_read_the_state_directory() -> Result<()> {
    let stack = Stack::up("state-private", COMPOSE, &[("node.yaml", NODE.to_owned()), ("config/keel.yaml", CONFIG.to_owned())])?;
    serving(&stack)?;
    wait_until("node key written", Duration::from_secs(30), || {
        Ok(stack.exec("keel", &["test", "-f", "/var/lib/keel/node.key"])?.status.success().then_some(()))
    })?;
    let control = control_uid(&stack)?.to_string();
    for (uid, can) in [(NOBODY.to_string(), false), (control, true)] {
        for file in ["/var/lib/keel/node.key", "/var/lib/keel/raft/store.redb"] {
            let out = stack.exec("keel", &["setpriv", "--reuid", &uid, "--regid", &uid, "--clear-groups", "cat", file])?;
            assert_eq!(out.status.success(), can, "uid {uid} reading {file}");
        }
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_drain_outlives_a_control_worker_and_a_worker_restart() -> Result<()> {
    let stack = Stack::up("drain-restarts", COMPOSE, &[("node.yaml", NODE.to_owned()), ("config/keel.yaml", CONFIG.to_owned())])?;
    serving(&stack)?;
    wait_until("the node leads", Duration::from_secs(30), || {
        Ok(keel(&stack, &["cluster", "status"]).ok().filter(|s| s.contains("leader")))
    })?;
    keel(&stack, &["backend", "drain", "172.29.83.20:80"])?;
    assert!(drained(&stack)?, "drained at once");

    // The control worker dies: the master restarts it, the control socket
    // stays bound, and the drain is still in effect.
    let (_, controls, workers) = keel_procs(&stack)?;
    stack.exec("keel", &["sh", "-c", &format!("kill -9 {}", controls[0].pid)])?;
    wait_until("a new control worker answers", Duration::from_secs(30), || {
        let (_, now, _) = keel_procs(&stack)?;
        let replaced = now.iter().any(|c| c.pid != controls[0].pid);
        Ok((replaced && keel(&stack, &["status"]).is_ok()).then_some(()))
    })?;

    // A worker dies too: its replacement starts from the config, and gets
    // the drain from the control worker.
    stack.exec("keel", &["sh", "-c", &format!("kill -9 {}", workers[0].pid)])?;
    wait_until("the replacement worker has the drain", Duration::from_secs(30), || {
        let (_, _, now) = keel_procs(&stack)?;
        let replaced = now.len() == 2 && now.iter().all(|w| w.pid != workers[0].pid);
        // The merged state is the least drained one: every worker drains it.
        Ok((replaced && drained(&stack)?).then_some(()))
    })?;

    // And the node restarts: the drain is stored with the cluster state.
    stack.stop("keel")?;
    stack.start("keel")?;
    wait_until("drained after a restart", Duration::from_secs(60), || {
        Ok(drained(&stack).ok().filter(|d| *d).map(drop))
    })?;
    Ok(())
}
