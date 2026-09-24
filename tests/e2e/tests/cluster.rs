//! Three-node cluster: formation and voter promotion, config push through the
//! leader, remote control over mTLS on every node, stepdown and the quorum
//! probe, and node identity — an ID generated once and kept, a node that moves
//! to a new address, and a copied state directory. node2 joins through the
//! bootstrap node and node3 through node2, so every stack admits one node
//! through a follower, which holds the replicated cluster CA.
//!
//! Each node reads its flags from `nodeN.args` (so a test can restart it with
//! others), keeps `/var/lib/keel` on a named volume (so a test can reach a
//! stopped node's Raft store), and has its config directory bind-mounted from
//! `nodeN-config/` (so a test can read what the node wrote, or edit it).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use keel_control::ControlRequest;
use keel_e2e::{http_get, wait_until, Control, Runtime, Stack};
use serde_json::Value;

const NODES: [&str; 3] = ["node1", "node2", "node3"];
const SECRET: &str = "e2e-cluster-secret";
const NODE_ID_FILE: &str = "/var/lib/keel/node_id";

fn ip(node: &str) -> String {
    format!("172.29.83.1{}", &node[4..])
}

fn join_args(target: &str) -> String {
    format!("--cluster --join {}:7654 --secret {SECRET}", ip(target))
}

/// Start flags: node1 bootstraps, node2 joins through node1, node3 through node2.
fn start_args(node: &str) -> String {
    match node {
        "node1" => format!("--cluster --bootstrap --secret {SECRET}"),
        "node2" => join_args("node1"),
        _ => join_args("node2"),
    }
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
        s.push_str(&format!(
            r#"  {node}:
    image: {{image}}
    init: true
    entrypoint: ["sh", "-c", "exec /usr/local/bin/keel $$(cat /etc/keel/args)"]
    environment: [NO_COLOR=1]
    volumes: ["./{node}.yaml:/etc/keel/node.yaml:ro", "./{node}-config:/etc/keel/config", "./{node}.args:/etc/keel/args:ro", "{node}-state:/var/lib/keel"]
    ports: ["80", "10789"]
    networks: {{ net: {{ ipv4_address: {ip} }} }}
"#,
            ip = ip(node)
        ));
    }
    // node4 waits for a node ID file before it starts keel, so a test can
    // give it a copy of another node's state first. It joins through node2.
    s.push_str(&format!(
        r#"  node4:
    image: {{image}}
    init: true
    entrypoint: ["sh", "-c", "while [ ! -s {NODE_ID_FILE} ]; do sleep 0.2; done; exec /usr/local/bin/keel {join}"]
    environment: [NO_COLOR=1]
    volumes: ["./node4.yaml:/etc/keel/node.yaml:ro", "./node4-config:/etc/keel/config"]
    networks: {{ net: {{ ipv4_address: 172.29.83.14 }} }}
volumes:
  node1-state: {{}}
  node2-state: {{}}
  node3-state: {{}}
"#,
        join = join_args("node2")
    ));
    s
}

/// The node file: binds the unspecified address and announces the node's own
/// IP, so every test exercises the difference between `addr` and `advertise`.
fn node_yaml(advertise_ip: &str) -> String {
    format!(
        r#"
keel:
  user: nobody
  group: nogroup
control:
  remote:
    address: 0.0.0.0:10789
cluster:
  addr: 0.0.0.0:7654
  advertise: {advertise_ip}:7654
  secret: {SECRET}
"#
    )
}

/// The replicated config's `keel.yaml`, with `extra_vhosts` before the catch-all.
fn lb_config(extra_vhosts: &str) -> String {
    format!(
        r#"
listeners:
  - address: 0.0.0.0:80
pools:
  web:
    backends:
      - address: 172.29.83.20:80
vhosts:
{extra_vhosts}  - host: "*"
    pool: web
"#
    )
}

const MARKER_VHOST: &str = "  - host: pushed.test\n    default_action:\n      status: 418\n      body: pushed\n";

fn file_set(entries: &[(&str, String)]) -> keel_control::FileSet {
    entries.iter().map(|(p, t)| (p.to_string(), t.clone())).collect()
}

struct Cluster {
    stack: Stack,
    control: Control,
    /// Node name → node ID, as each node reports it.
    ids: BTreeMap<String, u64>,
}

impl Cluster {
    /// Three nodes up, every one a voter, and operator credentials that work
    /// against any node.
    fn up(name: &str) -> Result<Cluster> {
        let mut files: Vec<(&str, String)> =
            vec![("node4.yaml", node_yaml("172.29.83.14")), ("node4-config/keel.yaml", lb_config(""))];
        files.extend([("node1.yaml", "node1"), ("node2.yaml", "node2"), ("node3.yaml", "node3")].map(|(f, n)| (f, node_yaml(&ip(n)))));
        files.extend(["node1-config/keel.yaml", "node2-config/keel.yaml", "node3-config/keel.yaml"].map(|f| (f, lb_config(""))));
        files.extend([("node1.args", "node1"), ("node2.args", "node2"), ("node3.args", "node3")].map(|(f, n)| (f, start_args(n))));
        let stack = Stack::up(name, &compose(), &files)?;

        // The control CA reaches every node through Raft and is written to
        // ca_dir; credentials must be signed by that CA, not a local one.
        wait_until("control CA on node1", Duration::from_secs(60), || {
            let out = stack.exec("node1", &["test", "-f", "/var/lib/keel/control/ca.crt"])?;
            Ok(out.status.success().then_some(()))
        })?;
        let out = stack.exec(
            "node1",
            &["keel", "credentials", "create", "e2e", "--endpoint", "127.0.0.1:1"],
        )?;
        anyhow::ensure!(out.status.success(), "credentials create: {}", String::from_utf8_lossy(&out.stderr));
        let control = Control::from_keelconfig(&String::from_utf8_lossy(&out.stdout))?;
        let mut cluster = Cluster { stack, control, ids: BTreeMap::new() };

        wait_until("three voters", Duration::from_secs(90), || Ok((cluster.voters("node1")?.len() == 3).then_some(())))?;
        // Followers adopt the cluster's control CA once the leader has
        // committed it; until then they reject these credentials.
        for node in NODES {
            let id = wait_until(&format!("{node} accepts the credentials"), Duration::from_secs(30), || {
                Ok(cluster.node_id(node).ok())
            })?;
            cluster.ids.insert(node.to_owned(), id);
        }
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

    fn node_id(&self, node: &str) -> Result<u64> {
        self.status(node)?["node_id"].as_u64().context("node_id in cluster status")
    }

    fn id(&self, node: &str) -> u64 {
        self.ids[node]
    }

    fn name(&self, id: u64) -> Result<String> {
        self.ids.iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone()).with_context(|| format!("no node has ID {id}"))
    }

    fn leader(&self) -> Result<String> {
        let id = wait_until("a leader", Duration::from_secs(30), || {
            let status = self.status("node1").or_else(|_| self.status("node2"))?;
            Ok(status["leader_id"].as_u64())
        })?;
        self.name(id)
    }

    /// Members as `id → (role, addr)`, as `node` sees them.
    fn membership(&self, node: &str) -> Result<BTreeMap<u64, (String, String)>> {
        let status = self.status(node)?;
        let members = status["membership"].as_array().context("membership")?;
        Ok(members
            .iter()
            .filter_map(|m| {
                let id = m["id"].as_u64()?;
                Some((id, (m["role"].as_str()?.to_owned(), m["addr"].as_str()?.to_owned())))
            })
            .collect())
    }

    fn members(&self, node: &str) -> Result<Vec<u64>> {
        Ok(self.membership(node)?.into_keys().collect())
    }

    fn voters(&self, node: &str) -> Result<Vec<u64>> {
        Ok(self.membership(node)?.into_iter().filter(|(_, (role, _))| role == "voter").map(|(id, _)| id).collect())
    }

    fn all_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.ids.values().copied().collect();
        ids.sort();
        ids
    }

    /// Restart `node` with other start flags.
    fn restart_with(&self, node: &str, args: &str) -> Result<()> {
        self.stack.stop(node)?;
        self.stack.write_file(&format!("{node}.args"), args)?;
        self.stack.start(node)
    }

    fn logged(&self, node: &str, text: &str) -> Result<bool> {
        Ok(self.stack.logs(node)?.contains(text))
    }

    /// Push a config with a `pushed.test` vhost answering 418, through the leader.
    fn push_marker(&self) -> Result<()> {
        let leader = self.leader()?;
        self.push(&leader, file_set(&[("keel.yaml", lb_config(MARKER_VHOST))]))?;
        Ok(())
    }

    fn push(&self, node: &str, files: keel_control::FileSet) -> Result<Value> {
        self.request(node, &ControlRequest::ConfigPush { files })
    }

    fn not_serving_marker(&self, node: &str) -> Result<()> {
        let addr = self.stack.addr(node, 80)?;
        wait_until(&format!("no pushed vhost on {node}"), Duration::from_secs(60), || {
            Ok(http_get(addr, "pushed.test", "/").ok().filter(|(s, _)| *s == 200))
        })?;
        Ok(())
    }

    fn serves_marker(&self, node: &str) -> Result<()> {
        let addr = self.stack.addr(node, 80)?;
        wait_until(&format!("pushed vhost on {node}"), Duration::from_secs(60), || {
            Ok(http_get(addr, "pushed.test", "/").ok().filter(|(s, _)| *s == 418))
        })?;
        Ok(())
    }

    fn wait_for_voters(&self, from: &str, ids: &[u64]) -> Result<()> {
        let mut want = ids.to_vec();
        want.sort();
        wait_until(&format!("voters {want:?}"), Duration::from_secs(90), || {
            Ok((self.voters(from).ok() == Some(want.clone())).then_some(()))
        })
    }

    fn read_node_id_file(&self, node: &str) -> Result<String> {
        let out = self.stack.exec(node, &["cat", NODE_ID_FILE])?;
        anyhow::ensure!(out.status.success(), "cat {NODE_ID_FILE} on {node} failed");
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

#[test]
#[ignore = "needs Docker"]
fn three_nodes_form_a_cluster_of_voters() -> Result<()> {
    let cluster = Cluster::up("cluster-form")?;
    let mut distinct = cluster.all_ids();
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "three different node IDs: {:?}", cluster.ids);
    for node in NODES {
        assert_eq!(cluster.members(node)?, cluster.all_ids(), "{node} sees all three members");
        let (status, _) = http_get(cluster.stack.addr(node, 80)?, "e2e.test", "/")?;
        assert_eq!(status, 200, "{node} serves traffic");
        // Bound on 0.0.0.0, announced on its own address.
        let (_, addr) = &cluster.membership("node1")?[&cluster.id(node)];
        assert_eq!(addr, &format!("{}:7654", ip(node)), "{node} is registered at its advertised address");
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn config_pushed_to_the_leader_reaches_every_node() -> Result<()> {
    let cluster = Cluster::up("cluster-push")?;
    let leader = cluster.leader()?;

    cluster.push(&leader, file_set(&[("keel.yaml", lb_config(MARKER_VHOST))]))?;

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

    // Sent to the follower over remote control; the removal is committed by the leader.
    cluster.request(&follower, &ControlRequest::ClusterStepdown { force: false })?;
    wait_until("follower removed", Duration::from_secs(30), || {
        Ok((!cluster.members(&leader)?.contains(&cluster.id(&follower))).then_some(()))
    })?;

    let remaining = NODES.iter().find(|n| **n != leader && **n != follower).expect("third node").to_string();
    cluster.request(&leader, &ControlRequest::ClusterStepdown { force: false })?;
    wait_until("leader removed, remaining node leads", Duration::from_secs(30), || {
        let status = cluster.status(&remaining)?;
        let members = cluster.members(&remaining)?;
        Ok((!members.contains(&cluster.id(&leader)) && status["leader_id"].as_u64() == Some(cluster.id(&remaining)))
            .then_some(()))
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
    assert!(cluster.members(&leader)?.contains(&cluster.id(leaving)), "{leaving} is still a member");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn node_id_survives_a_restart() -> Result<()> {
    let cluster = Cluster::up("cluster-restart")?;
    let before = cluster.read_node_id_file("node3")?;
    assert_eq!(before, cluster.id("node3").to_string(), "the file holds the ID the node reports");

    cluster.stack.stop("node3")?;
    cluster.stack.start("node3")?;
    wait_until("node3 back as a voter under its ID", Duration::from_secs(90), || {
        let back = cluster.node_id("node3").ok() == Some(cluster.id("node3"));
        Ok((back && cluster.voters("node1")?.len() == 3).then_some(()))
    })?;
    assert_eq!(cluster.read_node_id_file("node3")?, before, "node ID file unchanged");
    assert_eq!(cluster.members("node1")?, cluster.all_ids(), "no member added or lost");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_node_that_moves_to_a_new_address_keeps_its_id() -> Result<()> {
    let cluster = Cluster::up("cluster-readdress")?;
    let id = cluster.id("node3");
    let new_ip = "172.29.83.23";

    cluster.stack.stop("node3")?;
    cluster.stack.write_file("node3.yaml", &node_yaml(new_ip))?;
    cluster.stack.set_ip("node3", new_ip)?;
    cluster.stack.start("node3")?;

    let want = format!("{new_ip}:7654");
    wait_until("node3 a voter at its new address", Duration::from_secs(90), || {
        let members = cluster.membership("node1")?;
        let moved = members.get(&id).is_some_and(|(role, addr)| role == "voter" && *addr == want);
        Ok((moved && members.len() == 3).then_some(()))
    })?;
    assert_eq!(cluster.node_id("node3")?, id, "node3 kept its ID");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_copied_state_directory_is_refused() -> Result<()> {
    let cluster = Cluster::up("cluster-clone")?;
    let copied = cluster.read_node_id_file("node2")?;

    // node4 starts keel as soon as it holds node2's ID, while node2 is alive.
    let write = format!("mkdir -p /var/lib/keel && printf '%s\\n' {copied} > {NODE_ID_FILE}");
    let out = cluster.stack.exec("node4", &["sh", "-c", &write])?;
    anyhow::ensure!(out.status.success(), "write node ID on node4: {}", String::from_utf8_lossy(&out.stderr));

    let line = wait_until("node4's join refused", Duration::from_secs(60), || {
        let logs = cluster.stack.logs("node4")?;
        Ok(logs.lines().find(|l| l.contains("join rejected")).map(str::to_owned))
    })?;
    assert!(line.contains(&format!("{}:7654", ip("node2"))), "refusal names the member's address: {line}");
    assert!(line.contains("172.29.83.14:7654"), "refusal names the joiner's address: {line}");
    assert_eq!(cluster.members("node1")?, cluster.all_ids(), "membership unchanged");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_follower_admits_a_joiner() -> Result<()> {
    let cluster = Cluster::up("cluster-follower-join")?;
    assert_ne!(cluster.leader()?, "node2", "node2 is a follower");

    // A fresh identity for node4, which joins through node2.
    let id: u64 = 4_242_424_242;
    let out = cluster.stack.exec("node4", &["sh", "-c", &format!("mkdir -p /var/lib/keel && echo {id} > {NODE_ID_FILE}")])?;
    anyhow::ensure!(out.status.success(), "write node ID on node4: {}", String::from_utf8_lossy(&out.stderr));

    wait_until("node4 a voter", Duration::from_secs(90), || {
        let members = cluster.membership("node1")?;
        let voter = members.get(&id).is_some_and(|(role, addr)| role == "voter" && addr == "172.29.83.14:7654");
        Ok((voter && members.len() == 4).then_some(()))
    })?;
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn members_file_lists_the_cluster() -> Result<()> {
    let cluster = Cluster::up("cluster-members")?;
    let out = cluster.stack.exec("node2", &["cat", "/var/lib/keel/members.yaml"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    for node in NODES {
        assert!(text.contains(&cluster.id(node).to_string()), "{node} listed:\n{text}");
        assert!(text.contains(&format!("{}:7654", ip(node))), "{node}'s address listed:\n{text}");
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_node_restarts_from_its_stored_state_without_join() -> Result<()> {
    let cluster = Cluster::up("cluster-restore")?;
    cluster.restart_with("node3", &format!("--cluster --secret {SECRET}"))?;
    wait_until("node3 restored", Duration::from_secs(60), || Ok(cluster.logged("node3", "restarting from stored state")?.then_some(())))?;
    cluster.wait_for_voters("node1", &cluster.all_ids())?;
    assert_eq!(cluster.node_id("node3")?, cluster.id("node3"));
    assert!(!cluster.logged("node1", "lost its state")?, "node3 did not have to rejoin");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_full_cluster_restart_recovers_membership_and_config() -> Result<()> {
    let cluster = Cluster::up("cluster-full-restart")?;
    cluster.push_marker()?;
    cluster.serves_marker("node3")?;

    for node in NODES {
        cluster.stack.stop(node)?;
    }
    for node in NODES {
        cluster.stack.start(node)?;
    }
    cluster.wait_for_voters("node2", &cluster.all_ids())?;
    for node in NODES {
        cluster.serves_marker(node)?;
        assert_eq!(cluster.node_id(node)?, cluster.id(node), "{node} kept its ID");
    }
    // node1 still carries --bootstrap: stored state wins, no second cluster.
    assert!(cluster.logged("node1", "--bootstrap and --join are ignored")?);
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_damaged_store_is_set_aside_and_the_node_rejoins_without_losing_committed_entries() -> Result<()> {
    let cluster = Cluster::up("cluster-damaged")?;
    cluster.push_marker()?;
    cluster.serves_marker("node3")?;

    cluster.stack.stop("node3")?;
    cluster.stack.on_volume("node3-state", "printf 'not a store' > /state/raft/store.redb")?;
    cluster.stack.start("node3")?;

    // Traffic first: node3 serves from its local config while it rejoins.
    let addr = cluster.stack.addr("node3", 80)?;
    wait_until("node3 serving", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(s, _)| *s == 200))
    })?;
    assert!(cluster.logged("node3", "Raft store unreadable")?);
    let listing = cluster.stack.on_volume("node3-state", "ls /state")?;
    assert!(listing.contains("raft.corrupt-"), "store kept for inspection: {listing}");

    cluster.wait_for_voters("node1", &cluster.all_ids())?;
    assert_eq!(cluster.node_id("node3")?, cluster.id("node3"), "node3 kept its ID");
    assert!(cluster.logged("node1", "lost its state or moved")?, "rejoined through remove and learner");
    cluster.serves_marker("node3")?;
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn forced_recovery_makes_the_survivor_the_only_member() -> Result<()> {
    let cluster = Cluster::up("cluster-force")?;
    cluster.push_marker()?;
    cluster.serves_marker("node1")?;

    cluster.stack.stop("node2")?;
    cluster.stack.stop("node3")?;
    cluster.restart_with("node1", &format!("--cluster --force-new-cluster --secret {SECRET}"))?;

    let id = cluster.id("node1");
    cluster.wait_for_voters("node1", &[id])?;
    wait_until("node1 leads", Duration::from_secs(30), || {
        Ok((cluster.status("node1")?["leader_id"].as_u64() == Some(id)).then_some(()))
    })?;
    assert_eq!(cluster.members("node1")?, vec![id], "the others are gone from the membership");
    cluster.serves_marker("node1")?;
    // Writes commit again.
    cluster.push("node1", file_set(&[("keel.yaml", lb_config(""))]))?;
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_pushed_file_set_is_written_to_every_config_directory_and_replaces_it() -> Result<()> {
    let cluster = Cluster::up("config-files")?;
    let leader = cluster.leader()?;
    let follower = NODES.iter().find(|n| **n != leader).expect("a follower").to_string();

    // Pushed through a follower: forwarded to the leader.
    let marker = format!("vhosts:\n{MARKER_VHOST}");
    let reply = cluster.push(&follower, file_set(&[("keel.yaml", lb_config("")), ("vhosts/pushed.yaml", marker.clone())]))?;
    assert!(reply["version"].as_u64().is_some(), "a version comes back: {reply}");
    for node in NODES {
        cluster.serves_marker(node)?;
        wait_until(&format!("{node} wrote the pushed file"), Duration::from_secs(30), || {
            Ok((cluster.stack.read_file(&format!("{node}-config/vhosts/pushed.yaml"))?.as_deref() == Some(marker.as_str())).then_some(()))
        })?;
    }

    // A set without the file removes it everywhere.
    cluster.push(&leader, file_set(&[("keel.yaml", lb_config(""))]))?;
    for node in NODES {
        cluster.not_serving_marker(node)?;
        wait_until(&format!("{node} deleted the file"), Duration::from_secs(30), || {
            Ok(cluster.stack.read_file(&format!("{node}-config/vhosts/pushed.yaml"))?.is_none().then_some(()))
        })?;
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn an_invalid_push_is_refused_and_changes_nothing() -> Result<()> {
    let cluster = Cluster::up("config-invalid")?;
    let leader = cluster.leader()?;
    let broken = lb_config("  - host: broken.test\n    pool: nowhere\n");
    let err = cluster.push(&leader, file_set(&[("keel.yaml", broken)])).expect_err("must be refused").to_string();
    assert!(err.contains("config not pushed") && err.contains("nowhere"), "{err}");
    for node in NODES {
        assert_eq!(cluster.stack.read_file(&format!("{node}-config/keel.yaml"))?, Some(lb_config("")), "{node} files unchanged");
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn a_push_without_quorum_fails_at_once() -> Result<()> {
    let cluster = Cluster::up("config-no-quorum")?;
    let leader = cluster.leader()?;
    for node in NODES.iter().filter(|n| **n != leader) {
        cluster.stack.stop(node)?;
    }
    let started = std::time::Instant::now();
    let err = cluster.push(&leader, file_set(&[("keel.yaml", lb_config(MARKER_VHOST))])).expect_err("no quorum").to_string();
    assert!(err.contains("no quorum: 1 of 3 members reachable"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10), "failed at once, not after a commit timeout");
    let (status, _) = http_get(cluster.stack.addr(&leader, 80)?, "e2e.test", "/")?;
    assert_eq!(status, 200, "traffic unaffected");
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn sighup_pushes_the_node_config_directory() -> Result<()> {
    let cluster = Cluster::up("config-sighup")?;
    cluster.stack.write_file("node2-config/keel.yaml", &lb_config(MARKER_VHOST))?;
    cluster.stack.signal("node2", "HUP")?;
    for node in NODES {
        cluster.serves_marker(node)?;
    }
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn files_edited_while_a_node_was_stopped_become_the_next_version() -> Result<()> {
    let cluster = Cluster::up("config-edited")?;
    // Let node3 apply the seeded version first, so it has a version to compare against.
    wait_until("node3 applied a version", Duration::from_secs(30), || {
        Ok(cluster.stack.on_volume("node3-state", "cat /state/applied_config.json").ok())
    })?;
    cluster.stack.stop("node3")?;
    cluster.stack.write_file("node3-config/keel.yaml", &lb_config(MARKER_VHOST))?;
    cluster.stack.start("node3")?;
    for node in NODES {
        cluster.serves_marker(node)?;
    }
    assert!(cluster.logged("node3", "config files changed while stopped")?);
    Ok(())
}

#[test]
#[ignore = "needs Docker"]
fn revoke_all_locks_out_every_earlier_keelconfig() -> Result<()> {
    let cluster = Cluster::up("revoke-all")?;
    let leader = cluster.leader()?;
    let follower = NODES.iter().find(|n| **n != leader).expect("a follower").to_string();
    cluster.request(&follower, &ControlRequest::CredentialsRevokeAll)?;

    for node in NODES {
        wait_until(&format!("{node} refuses the old keelconfig"), Duration::from_secs(30), || {
            Ok(cluster.request(node, &ControlRequest::Status).is_err().then_some(()))
        })?;
    }
    // Credentials from the new CA work everywhere.
    let out = cluster.stack.exec("node1", &["keel", "credentials", "create", "e2e2", "--endpoint", "127.0.0.1:1"])?;
    let fresh = Control::from_keelconfig(&String::from_utf8_lossy(&out.stdout))?;
    for node in NODES {
        let addr = cluster.stack.addr(node, 10789)?;
        wait_until(&format!("{node} accepts the new keelconfig"), Duration::from_secs(30), || {
            Ok(fresh.request(addr, &ControlRequest::Status).ok())
        })?;
    }
    Ok(())
}
