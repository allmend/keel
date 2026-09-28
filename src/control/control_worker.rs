//! The control worker: the unprivileged child of the master that owns
//! everything instance-wide. It runs Raft (a node is a cluster of one or
//! more), serves the instance control socket and `control.remote` (both bound
//! by the master and inherited), issues ACME certificates, and drives every
//! applied config into the workers.
//!
//! It runs as `keel.control_user`: the state directory — node key, Raft store
//! with both CA keys, ACME account and certificates — is readable by it
//! alone. Workers get what they serve from it, over their control sockets.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio::sync::{mpsc, watch, Notify};
use tracing::{error, info, warn};

use crate::cluster::types::ConfigVersion;
use crate::config::Config;
use crate::control::fanout::{Applied, WorkerFanout};
use crate::control::{ControlServer, Dispatch};

/// Announced by a node without a `cluster:` section. It has no peers, so
/// nothing connects to it; adding the section later moves the node to its
/// real address.
const NO_PEERS_ADDR: &str = "127.0.0.1:7654";

/// What the master tells the control worker, one JSON line each.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum MasterMessage {
    /// SIGHUP reached the master: push the config directory.
    Reload,
    /// The master replaced worker `index`; it needs the applied config.
    WorkerRestarted { index: usize },
}

/// Longest line the master sends.
const MAX_MASTER_LINE: u64 = 4096;

/// Descriptors the master bound and passed down.
pub struct Inherited {
    pub control_socket: std::os::unix::net::UnixListener,
    pub remote: Option<std::net::TcpListener>,
    pub master: std::os::unix::net::UnixStream,
}

/// Entry point after the fork and the privilege drop. Never returns.
pub fn run(cfg: Config, inherited: Inherited, force_new_cluster: bool) -> ! {
    // A thread per CPU: every Raft RPC is a TLS handshake, and on too few
    // threads a burst of them (a cluster forming) outlasts openraft's RPC
    // deadline, so replication to a joining node never catches up.
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "control: cannot start runtime");
            std::process::exit(1);
        }
    };
    let code = match runtime.block_on(serve(cfg, inherited, force_new_cluster)) {
        Ok(()) => 0,
        Err(e) => {
            error!(error = %format!("{e:#}"), "control: fatal error");
            1
        }
    };
    std::process::exit(code)
}

async fn serve(inherited_cfg: Config, inherited: Inherited, force_new_cluster: bool) -> Result<()> {
    // Read afresh: a control worker the master restarts must start from the
    // files as they are now, not as they were when the master forked it.
    let cfg = match crate::config::load(&inherited_cfg.path) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "control: cannot re-read the config; using the master's copy");
            inherited_cfg
        }
    };

    let (shutdown_tx, shutdown) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    {
        let tx = Arc::clone(&shutdown_tx);
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            let _ = tx.send(true);
        });
    }

    let state_dir = PathBuf::from(&cfg.keel.state_dir);
    let config_dir = PathBuf::from(&cfg.keel.config_dir);
    let node_id = crate::cluster::identity::load_or_create_node_id(&state_dir)?;
    let (cluster_addr, advertise) = match &cfg.cluster {
        Some(c) => (Some(c.addr.clone()), crate::cluster::identity::announce_addr(&c.addr, c.advertise.as_deref())?),
        None => (None, NO_PEERS_ADDR.to_owned()),
    };
    info!(node_id, advertise, peers = cluster_addr.is_some(), "control: starting");

    let opts = crate::cluster::ClusterOpts {
        node_id,
        cluster_addr,
        advertise,
        state_dir: state_dir.clone(),
        force_new_cluster,
        node_yaml: cfg.node_yaml.clone(),
        config_dir: config_dir.clone(),
        startup_files: cfg.files.clone(),
        secret: cfg.cluster.as_ref().and_then(|c| c.secret.clone()),
        join: cfg.cluster.as_ref().map(|c| c.join.clone()).unwrap_or_default(),
    };
    let (cluster, svc) = crate::cluster::new_cluster(opts);
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { pingora::services::background::BackgroundService::start(&svc, shutdown).await });
    }

    let (applied_tx, applied_rx) = watch::channel(Arc::new(cfg.clone()));
    let certs_changed = Arc::new(Notify::new());
    {
        let acme = crate::acme::AcmeService::new(applied_rx, cluster.clone(), Arc::clone(&certs_changed));
        let shutdown = shutdown.clone();
        tokio::spawn(async move { pingora::services::background::BackgroundService::start(&acme, shutdown).await });
    }

    let (restarted_tx, restarted_rx) = mpsc::unbounded_channel();
    let driver = ApplyDriver {
        cluster: cluster.clone(),
        fanout: WorkerFanout::new(&cfg.keel.control_socket, cfg.keel.workers),
        node_yaml: cfg.node_yaml.clone(),
        config_dir,
        state_dir,
        applied: applied_tx,
        certs_changed,
        startup_files: Some(cfg.files.clone()),
    };
    tokio::spawn(driver.run(restarted_rx, shutdown.clone()));

    let dispatch = Dispatch::new(WorkerFanout::new(&cfg.keel.control_socket, cfg.keel.workers), cluster.clone());
    {
        let server = ControlServer { listener: inherited.control_socket, dispatch: Arc::clone(&dispatch) };
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move { server.run(&mut shutdown).await });
    }
    if let (Some(remote), Some(listener)) = (cfg.control.as_ref().and_then(|c| c.remote.clone()), inherited.remote) {
        let server = crate::control::remote::RemoteControlServer {
            cfg: remote,
            listener,
            ca_dir: cfg.keel.control_ca_dir(),
            dispatch: Arc::clone(&dispatch),
        };
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move { server.serve(&mut shutdown).await });
    }
    tokio::spawn(master_link(inherited.master, cluster, restarted_tx, Arc::clone(&shutdown_tx)));

    let mut shutdown = shutdown;
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
    info!("control: shutting down");
    // Let Raft and the listeners observe it.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    Ok(())
}

/// Read the master's messages. The master never reads anything back: no
/// input reaches the root process. It closing the link means it is gone,
/// and so is the reason to run.
async fn master_link(
    stream: std::os::unix::net::UnixStream,
    cluster: crate::cluster::ClusterHandle,
    restarted: mpsc::UnboundedSender<usize>,
    shutdown: Arc<watch::Sender<bool>>,
) {
    let stream = match stream.set_nonblocking(true).and_then(|()| tokio::net::UnixStream::from_std(stream)) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "control: cannot read the master link");
            return;
        }
    };
    let mut reader = BufReader::new(stream);
    loop {
        let line = match crate::control::read_line_capped(&mut reader, MAX_MASTER_LINE).await {
            Ok(line) if line.is_empty() => break,
            Ok(line) => line,
            Err(e) => {
                warn!(error = %e, "control: unreadable message from the master");
                break;
            }
        };
        match serde_json::from_str::<MasterMessage>(line.trim()) {
            Ok(MasterMessage::Reload) => {
                let cluster = cluster.clone();
                tokio::spawn(async move {
                    // A SIGHUP during startup waits for the cluster instead
                    // of being lost.
                    for _ in 0..600 {
                        if cluster.raft().await.is_some() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    let pushed = match crate::config::read_config_dir(&cluster.config_dir) {
                        Ok(files) => cluster.push_config(files).await,
                        Err(e) => Err(e),
                    };
                    match pushed {
                        Ok(version) => info!(version, "reload: config directory committed as a new version"),
                        Err(e) => error!(error = %format!("{e:#}"), "reload: config directory not pushed"),
                    }
                });
            }
            Ok(MasterMessage::WorkerRestarted { index }) => {
                let _ = restarted.send(index);
            }
            Err(e) => warn!(error = %e, "control: unknown message from the master"),
        }
    }
    info!("control: the master closed its link; stopping");
    let _ = shutdown.send(true);
}

/// Applies every committed config version on this node and keeps the workers
/// serving it: the version itself, the drains and the ACME certificates.
/// A version is built against this node's `node.yaml` first; only once it
/// is applied are the config directory and the "last applied" record
/// written, so the files are always the last version that worked here.
struct ApplyDriver {
    cluster: crate::cluster::ClusterHandle,
    fanout: WorkerFanout,
    node_yaml: String,
    config_dir: PathBuf,
    state_dir: PathBuf,
    /// The applied config, for the ACME service.
    applied: watch::Sender<Arc<Config>>,
    certs_changed: Arc<Notify>,
    /// The config directory as read at start, until the first version is
    /// applied. A version with exactly these files came from this directory:
    /// it is not written back, so edits made since are not overwritten.
    startup_files: Option<crate::config::FileSet>,
}

impl ApplyDriver {
    async fn run(mut self, mut restarted: mpsc::UnboundedReceiver<usize>, mut shutdown: watch::Receiver<bool>) {
        let mut versions = self.cluster.config_rx.clone();
        let mut drained = self.cluster.drained_rx.clone();
        let mut version = crate::cluster::applied::read(&self.state_dir).ok().flatten().map_or(0, |a| a.version);

        // Workers may be older than this control worker (it was restarted):
        // give them the current state before anything else.
        self.send_all(version).await;
        loop {
            let next = versions.borrow_and_update().clone();
            if let Some(next) = next.filter(|n| n.version != version) {
                if self.apply(&next).await {
                    version = next.version;
                }
            }
            tokio::select! {
                changed = versions.changed() => {
                    if changed.is_err() { return; }
                }
                changed = drained.changed() => {
                    if changed.is_err() { return; }
                    self.send_all(version).await;
                }
                _ = self.certs_changed.notified() => {
                    self.send_all(version).await;
                }
                // Awaited, like every hand-over here, so an older state can
                // never overtake a newer one on its way to a worker.
                Some(index) = restarted.recv() => {
                    if self.fanout.apply_to(index, &self.state(version)).await {
                        info!(index, version, "control: restarted worker has the applied config");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
            }
        }
    }

    /// Apply a committed version; false when it cannot be applied here.
    async fn apply(&mut self, next: &ConfigVersion) -> bool {
        let cfg = match crate::config::assemble(&self.node_yaml, &next.files) {
            Ok(cfg) => cfg,
            Err(e) => {
                error!(
                    version = next.version,
                    error = %format!("{e:#}"),
                    "cluster: config version cannot be applied; still serving the previous one, files unchanged"
                );
                return false;
            }
        };
        let mut cfg = cfg;
        cfg.node_yaml = self.node_yaml.clone();
        cfg.path = self.applied.borrow().path.clone();
        self.applied.send_replace(Arc::new(cfg));
        let acked = self.send_all(next.version).await;
        let from_here = self.startup_files.take().is_some_and(|files| files == next.files);
        let written = if from_here { Ok(()) } else { crate::cluster::applied::write_config_dir(&self.config_dir, &next.files) }
            .and_then(|()| crate::cluster::applied::save(&self.state_dir, next.version, &next.files));
        match written {
            Ok(()) => info!(version = next.version, workers = acked, "cluster: config version applied"),
            Err(e) => error!(
                version = next.version,
                error = %format!("{e:#}"),
                "cluster: config version applied, but its files could not be written"
            ),
        }
        true
    }

    /// What the workers serve: the applied config, its ACME certificates
    /// and the drains.
    fn state(&self, version: u64) -> Applied {
        let cfg = self.applied.borrow().clone();
        Applied {
            version,
            node_yaml: crate::config::without_cluster_section(&self.node_yaml),
            files: cfg.files.clone(),
            certs: issued_certificates(&cfg),
            drained: self.cluster.drained_rx.borrow().clone(),
        }
    }

    async fn send_all(&self, version: u64) -> usize {
        self.fanout.apply_all(&self.state(version)).await
    }
}

/// The ACME certificates on disk for the hosts `cfg` manages.
fn issued_certificates(cfg: &Config) -> crate::tls::Issued {
    let Some(acme) = cfg.acme_effective() else { return Default::default() };
    cfg.acme_assignments()
        .into_iter()
        .filter_map(|(host, _, _)| {
            let (cert, key) = crate::tls::acme_cert_paths(&acme.storage, host);
            let read = |p: &str| std::fs::read_to_string(Path::new(p)).ok();
            Some((host.to_owned(), (read(&cert)?, read(&key)?)))
        })
        .collect()
}

/// The master's end of the link: send one message, never block. A message
/// the control worker cannot take right now is dropped; it repeats the full
/// state to every worker when it starts, so nothing is lost for good.
pub fn send_to_control(link: &std::os::unix::net::UnixStream, message: &MasterMessage) {
    use std::io::Write;
    let Ok(mut line) = serde_json::to_vec(message) else { return };
    line.push(b'\n');
    if let Err(e) = (&*link).write_all(&line).context("write") {
        warn!(error = %format!("{e:#}"), ?message, "master: message to the control worker dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::MasterMessage;

    #[test]
    fn master_messages_are_one_line_of_json() {
        for m in [MasterMessage::Reload, MasterMessage::WorkerRestarted { index: 3 }] {
            let line = serde_json::to_string(&m).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<MasterMessage>(&line).unwrap(), m);
        }
    }
}
