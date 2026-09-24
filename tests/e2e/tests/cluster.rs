//! Three-node cluster: formation and voter promotion, config push through the
//! leader, remote control over mTLS on every node, stepdown, and the quorum
//! probe that refuses a stepdown the remaining nodes could not survive.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use keel_control::ControlRequest;
use keel_e2e::{http_get, wait_until, Control, Runtime, Stack};
use serde_json::Value;

const NODES: [&str; 3] = ["node1", "node2", "node3"];
const SECRET: &str = "e2e-cluster-secret";

fn ip(node: &str) -> String {
    format!("172.29.83.1{}", &node[4..])
}

fn compose() -> String {
    let mut s = String::from(
        r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.83.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.83.20 } }
"#,
    );
    for node in NODES {
        let role = if node == "node1" {
            "\"--bootstrap\"".to_owned()
        } else {
            format!("\"--join\", \"{}:7654\"", ip("node1"))
        };
        s.push_str(&format!(
            r#"  {node}:
    image: {{image}}
    init: true
    command: ["--config", "/etc/keel/keel.yaml", "--cluster", {role}, "--secret", "{SECRET}"]
    environment: [NO_COLOR=1]
    volumes: ["./{node}.yaml:/etc/keel/keel.yaml:ro"]
    ports: ["80", "10789"]
    networks: {{ net: {{ ipv4_address: {ip} }} }}
"#,
            ip = ip(node)
        ));
    }
    s
}

fn config(node: &str, extra_vhosts: &str) -> String {
    format!(
        r#"
keel:
  user: nobody
  group: nogroup
listeners:
  - address: 0.0.0.0:80
control:
  remote:
    address: 0.0.0.0:10789
cluster:
  addr: {ip}:7654
  node_id: {id}
  secret: {SECRET}
pools:
  web:
    backends:
      - address: 172.29.83.20:80
vhosts:
{extra_vhosts}  - host: "*"
    pool: web
"#,
        ip = ip(node),
        id = &node[4..],
    )
}

struct Cluster {
    stack: Stack,
    control: Control,
}

impl Cluster {
    /// Three nodes up, every one a voter, and operator credentials that work
    /// against any node.
    fn up(name: &str) -> Result<Cluster> {
        let files: Vec<(&str, String)> =
            NODES.iter().map(|n| (Box::leak(format!("{n}.yaml").into_boxed_str()) as &str, config(n, ""))).collect();
        let stack = Stack::up(name, &compose(), &files)?;

        // The control CA reaches every node through Raft and is written to
        // ca_dir; credentials must be signed by that CA, not a local one.
        wait_until("control CA on node1", Duration::from_secs(60), || {
            let out = stack.exec("node1", &["test", "-f", "/var/lib/keel/control/ca.crt"])?;
            Ok(out.status.success().then_some(()))
        })?;
        let out = stack.exec(
            "node1",
            &["keel", "--config", "/etc/keel/keel.yaml", "credentials", "create", "e2e", "--endpoint", "127.0.0.1:1"],
        )?;
        anyhow::ensure!(out.status.success(), "credentials create: {}", String::from_utf8_lossy(&out.stderr));
        let control = Control::from_keelconfig(&String::from_utf8_lossy(&out.stdout))?;
        let cluster = Cluster { stack, control };

        wait_until("three voters", Duration::from_secs(90), || {
            let status = cluster.status("node1")?;
            let voters = status["membership"]
                .as_array()
                .map(|m| m.iter().filter(|n| n["role"] == "voter").count())
                .unwrap_or(0);
            Ok((voters == 3).then_some(()))
        })?;
        Ok(cluster)
    }

    fn control_addr(&self, node: &str) -> Result<SocketAddr> {
        self.stack.addr(node, 10789)
    }

    fn request(&self, node: &str, request: &ControlRequest) -> Result<Value> {
        self.control.request(self.control_addr(node)?, request)
    }

    fn status(&self, node: &str) -> Result<Value> {
        self.request(node, &ControlRequest::ClusterStatus)
    }

    fn leader(&self) -> Result<String> {
        wait_until("a leader", Duration::from_secs(30), || {
            let status = self.status("node1").or_else(|_| self.status("node2"))?;
            Ok(status["leader_id"].as_u64().map(|id| format!("node{id}")))
        })
    }

    fn members(&self, node: &str) -> Result<Vec<u64>> {
        let status = self.status(node)?;
        let mut ids: Vec<u64> = status["membership"]
            .as_array()
            .context("membership")?
            .iter()
            .filter_map(|m| m["id"].as_u64())
            .collect();
        ids.sort();
        Ok(ids)
    }
}

#[test]
#[ignore = "needs Docker"]
fn three_nodes_form_a_cluster_of_voters() -> Result<()> {
    let cluster = Cluster::up("cluster-form")?;
    for node in NODES {
        assert_eq!(cluster.members(node)?, vec![1, 2, 3], "{node} sees all three members");
        let (status, _) = http_get(cluster.stack.addr(node, 80)?, "e2e.test", "/")?;
        assert_eq!(status, 200, "{node} serves traffic");
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn config_pushed_to_the_leader_reaches_every_node() -> Result<()> {
    let cluster = Cluster::up("cluster-push")?;
    let leader = cluster.leader()?;

    let vhost = "  - host: pushed.test\n    default_action:\n      status: 418\n      body: pushed\n";
    let yaml = config(&leader, vhost);
    cluster.request(&leader, &ControlRequest::ConfigPush { yaml })?;

    for node in NODES {
        let addr = cluster.stack.addr(node, 80)?;
        wait_until(&format!("pushed vhost on {node}"), Duration::from_secs(30), || {
            Ok(http_get(addr, "pushed.test", "/").ok().filter(|(s, _)| *s == 418))
        })?;
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn remote_control_answers_on_every_node_and_requires_a_client_certificate() -> Result<()> {
    let cluster = Cluster::up("cluster-control")?;
    for node in NODES {
        let status = cluster.request(node, &ControlRequest::Status)?;
        assert!(status["pools"].is_array(), "{node} answers status: {status}");

        let refused = cluster.control.request_without_client_cert(cluster.control_addr(node)?, &ControlRequest::Status);
        assert!(refused.is_err(), "{node} accepted a client without a certificate");
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn cluster_nodes_run_unprivileged() -> Result<()> {
    let cluster = Cluster::up("cluster-privileges")?;
    for node in NODES {
        let procs = cluster.stack.procs(node)?;
        let keel: Vec<_> = procs.iter().filter(|p| p.cmd.starts_with("/usr/local/bin/keel")).collect();
        assert!(!keel.is_empty(), "{node} runs keel: {procs:?}");
        for p in keel {
            assert_ne!(p.uid, 0, "{node}: keel pid {} runs as root", p.pid);
        }
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn stepdown_of_a_follower_is_forwarded_then_the_leader_steps_down() -> Result<()> {
    let cluster = Cluster::up("cluster-stepdown")?;
    let leader = cluster.leader()?;
    let follower = NODES.iter().find(|n| **n != leader).expect("a follower").to_string();
    let follower_id: u64 = follower[4..].parse()?;

    // Sent to the follower over remote control; the removal is committed by the leader.
    cluster.request(&follower, &ControlRequest::ClusterStepdown { force: false })?;
    wait_until("follower removed", Duration::from_secs(30), || {
        Ok((!cluster.members(&leader)?.contains(&follower_id)).then_some(()))
    })?;

    let remaining = NODES.iter().find(|n| **n != leader && **n != follower).expect("third node").to_string();
    let leader_id: u64 = leader[4..].parse()?;
    cluster.request(&leader, &ControlRequest::ClusterStepdown { force: false })?;
    wait_until("leader removed, remaining node leads", Duration::from_secs(30), || {
        let status = cluster.status(&remaining)?;
        let members = cluster.members(&remaining)?;
        let own_id = status["node_id"].as_u64();
        Ok((!members.contains(&leader_id) && status["leader_id"].as_u64() == own_id).then_some(()))
    })?;

    let (status, _) = http_get(cluster.stack.addr(&remaining, 80)?, "e2e.test", "/")?;
    assert_eq!(status, 200, "remaining node still serves");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn stepdown_is_refused_when_the_rest_would_lose_quorum() -> Result<()> {
    let cluster = Cluster::up("cluster-quorum")?;
    let leader = cluster.leader()?;
    let others: Vec<&str> = NODES.iter().copied().filter(|n| *n != leader).collect();
    let (leaving, stopped) = (others[0], others[1]);

    // With `stopped` down, removing `leaving` leaves {leader, stopped}: one of two reachable.
    cluster.stack.stop(stopped)?;
    let refused = cluster.request(leaving, &ControlRequest::ClusterStepdown { force: false });
    let err = refused.expect_err("stepdown without quorum must be refused").to_string();
    assert!(err.to_lowercase().contains("quorum"), "refusal names quorum: {err}");
    assert!(cluster.members(&leader)?.contains(&leaving[4..].parse()?), "{leaving} is still a member");
    Ok(())
}
