//! The root master. It binds every listening socket — data plane, instance
//! control socket, `control.remote` — prepares the directories, forks the
//! control worker and the workers, and supervises them. It has no async
//! runtime and reads no input from anyone: it only ever writes to the control
//! worker, over a socketpair. The nginx model, plus a control process.
//!
//! ```text
//! keel (master, root)
//! ├ control worker (keel.control_user): Raft, control listeners, ACME
//! ├ worker 0 (keel.user): Pingora data plane
//! └ worker N
//! ```

use crate::config::Config;
use crate::control::control_worker::{self, MasterMessage};
use crate::proxy;
use anyhow::{Context, Result};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Gid, Pid, Uid};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Same backlog Pingora uses for the listeners it binds itself.
const LISTENER_BACKLOG: i32 = 65535;

/// How often the master looks at signals and dead children.
const TICK: Duration = Duration::from_millis(100);

/// A control worker that dies sooner than this after starting is restarted
/// with a growing delay, so a start that keeps failing (a refused join, a
/// broken store) does not spin.
const CONTROL_STABLE: Duration = Duration::from_secs(10);
const CONTROL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Listening sockets bound by the root master before any worker is forked.
/// Workers inherit them across `fork` and never bind privileged ports
/// themselves.
pub struct BoundListeners {
    /// One listening socket per TCP listener, shared by every worker; the
    /// kernel spreads `accept` across the processes.
    pub tcp: Vec<(String, std::net::TcpListener)>,
    /// One socket per worker per UDP listener, all in one SO_REUSEPORT group,
    /// so the kernel's client→socket hash keeps every flow on one worker.
    pub udp: Vec<(String, Vec<std::net::UdpSocket>)>,
    /// One ICMP datagram socket per worker and family, when any pool uses an
    /// `icmp` health check. Needs CAP_NET_RAW to open — the master has it.
    pub icmp4: Vec<std::net::UdpSocket>,
    pub icmp6: Vec<std::net::UdpSocket>,
}

impl BoundListeners {
    /// The subset a given worker uses. The struct itself stays alive in the
    /// child (it never returns), so the raw TCP fds remain valid.
    fn for_worker(&self, index: usize, cfg: &Config) -> Result<proxy::WorkerSockets> {
        let tcp = self.tcp.iter().map(|(a, l)| (a.clone(), l.as_raw_fd())).collect();
        let udp = self
            .udp
            .iter()
            .map(|(a, socks)| Ok((a.clone(), socks[index].try_clone()?)))
            .collect::<std::io::Result<HashMap<_, _>>>()?;
        // Private hand-off socket for Pingora (see proxy::run). The worker
        // creates it, so it goes in the worker-owned socket directory.
        let upgrade_sock = crate::control::fanout::worker_socket_dir(&cfg.keel.control_socket)
            .join(format!("upgrade-{index}.sock"))
            .to_string_lossy()
            .into_owned();
        let icmp4 = self.icmp4.get(index).map(|s| s.try_clone()).transpose()?;
        let icmp6 = self.icmp6.get(index).map(|s| s.try_clone()).transpose()?;
        Ok(proxy::WorkerSockets { tcp, udp, icmp4, icmp6, upgrade_sock })
    }

    fn fds(&self) -> Vec<RawFd> {
        let tcp = self.tcp.iter().map(|(_, l)| l.as_raw_fd());
        let udp = self.udp.iter().flat_map(|(_, s)| s.iter().map(AsRawFd::as_raw_fd));
        let icmp = self.icmp4.iter().chain(&self.icmp6).map(AsRawFd::as_raw_fd);
        tcp.chain(udp).chain(icmp).collect()
    }
}

/// Who a child becomes. `None` everywhere when the master is not root: then
/// there is nothing to drop, and every process runs as the invoking user.
#[derive(Clone)]
struct Account {
    name: String,
    uid: Uid,
    gid: Gid,
    /// Supplementary groups: the control worker's includes `keel.group`.
    groups: Vec<Gid>,
}

struct Accounts {
    worker: Account,
    control: Account,
}

/// Resolve both users up front, so a missing one fails startup instead of
/// every forked child.
fn resolve_accounts(cfg: &Config) -> Result<Option<Accounts>> {
    use nix::unistd::{Group, User};
    if !nix::unistd::getuid().is_root() {
        return Ok(None);
    }
    let Ok(Some(group)) = Group::from_name(&cfg.keel.group) else {
        anyhow::bail!("group '{}' not found — cannot drop privileges (set keel.group)", cfg.keel.group);
    };
    let Ok(Some(user)) = User::from_name(&cfg.keel.user) else {
        anyhow::bail!("user '{}' not found — cannot drop privileges (set keel.user)", cfg.keel.user);
    };
    let Ok(Some(control)) = User::from_name(&cfg.keel.control_user) else {
        anyhow::bail!(
            "user '{}' not found — the control worker runs as its own user (create it, or set keel.control_user)",
            cfg.keel.control_user
        );
    };
    Ok(Some(Accounts {
        worker: Account { name: user.name, uid: user.uid, gid: group.gid, groups: Vec::new() },
        control: Account { name: control.name, uid: control.uid, gid: control.gid, groups: vec![group.gid] },
    }))
}

/// Make every directory Keel uses at runtime exist, owned by the process
/// that writes it. Only called as root.
///
/// ```text
/// runtime dir (/var/run/keel)  root            0755  instance socket: control user, 0660
///   workers/                   keel.user       0750  worker sockets; the control worker is in the group
///   challenges/                control user    0750  HTTP-01 tokens; read by the workers through the group
/// state_dir (/var/lib/keel)    control user    0700  node key, Raft store (CA keys), ACME
/// config_dir                   control user    0700  written after each applied version, if writable
/// access_log.dir               keel.user       as found; taken once
/// ```
fn prepare_dirs(cfg: &Config, accounts: &Accounts) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (worker, control) = (&accounts.worker, &accounts.control);
    let mode = |dir: &std::path::Path, mode: u32| {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("master: cannot set the mode of {}", dir.display()))
    };
    let create = |dir: &std::path::Path| {
        std::fs::create_dir_all(dir).with_context(|| format!("master: cannot create {}", dir.display()))
    };

    // Root-owned: a process that can write the directory could unlink the
    // instance socket and bind its own in its place.
    let runtime = cfg.keel.runtime_dir();
    create(&runtime)?;
    nix::unistd::chown(&runtime, Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))?;
    mode(&runtime, 0o755)?;

    let workers = crate::control::fanout::worker_socket_dir(&cfg.keel.control_socket);
    create(&workers)?;
    nix::unistd::chown(&workers, Some(worker.uid), Some(worker.gid))?;
    mode(&workers, 0o750)?;

    let challenges = crate::acme::challenge_dir(&cfg.keel);
    create(&challenges)?;
    nix::unistd::chown(&challenges, Some(control.uid), Some(worker.gid))?;
    mode(&challenges, 0o750)?;

    let state = std::path::Path::new(&cfg.keel.state_dir);
    create(state)?;
    take_tree(state, control.uid, control.gid)?;
    mode(state, 0o700)?;
    if let Some(acme) = cfg.acme_effective() {
        let storage = std::path::Path::new(&acme.storage);
        create(storage)?;
        take_tree(storage, control.uid, control.gid)?;
        mode(storage, 0o700)?;
    }

    // A read-only config directory (a mounted ConfigMap, say) is served
    // from; only writing applied versions back to it fails, and is logged.
    let config = std::path::Path::new(&cfg.keel.config_dir);
    let owned = create(config).and_then(|()| take_tree(config, control.uid, control.gid)).and_then(|()| mode(config, 0o700));
    if let Err(e) = owned {
        warn!(error = %format!("{e:#}"), "master: config directory not writable; applied versions are not written back");
    }

    // Access logs are opened by the workers, after the drop.
    if cfg.access_log.enabled && cfg.access_log.dir != "-" {
        let logs = std::path::Path::new(&cfg.access_log.dir);
        create(logs)?;
        take_ownership(logs, worker.uid, worker.gid)?;
    }
    info!(runtime = %runtime.display(), state = cfg.keel.state_dir, "master: directories ready");
    Ok(())
}

/// Give `path` to `uid` unless that user already owns it. Once Keel's user
/// owns a directory it is left alone, so a group an administrator sets
/// afterwards (say, for a log shipper) survives restarts.
fn take_ownership(path: &std::path::Path, uid: Uid, gid: Gid) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::symlink_metadata(path).with_context(|| format!("master: cannot stat {}", path.display()))?.uid();
    if owner == uid.as_raw() {
        return Ok(());
    }
    nix::unistd::chown(path, Some(uid), Some(gid)).with_context(|| format!("master: cannot chown {}", path.display()))
}

/// [`take_ownership`] for a directory and everything below it: a state or
/// config directory an earlier version gave to `keel.user` moves to the
/// control user.
fn take_tree(path: &std::path::Path, uid: Uid, gid: Gid) -> Result<()> {
    take_ownership(path, uid, gid)?;
    if std::fs::symlink_metadata(path)?.is_dir() {
        for entry in std::fs::read_dir(path)? {
            take_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// Bind every configured listener while still root. `per_listener` UDP and
/// ICMP sockets are opened per listener — one per worker (an unread
/// SO_REUSEPORT socket would swallow its share of datagrams).
fn bind_listeners(cfg: &Config, per_listener: usize) -> Result<BoundListeners> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::ToSocketAddrs;

    let mut tcp = Vec::new();
    let mut udp = Vec::new();
    let (mut icmp4, mut icmp6) = (Vec::new(), Vec::new());
    if crate::health::icmp::needed(cfg) {
        for _ in 0..per_listener {
            icmp4.push(crate::health::icmp::open_socket(false).context("master: cannot open ICMPv4 socket")?);
            match crate::health::icmp::open_socket(true) {
                Ok(s) => icmp6.push(s),
                // IPv6 may be disabled on the host; v6 backends then pass unchecked.
                Err(e) => warn!(error = %e, "master: cannot open ICMPv6 socket — icmp checks of IPv6 backends pass unconditionally"),
            }
        }
        info!(sockets = icmp4.len(), "master: opened ICMP sockets for health checks");
    }
    for l in &cfg.listeners {
        if l.udp_pool.is_some() {
            let socks = (0..per_listener)
                .map(|_| crate::udp::bind_reuseport(&l.address))
                .collect::<std::io::Result<Vec<_>>>()
                .with_context(|| format!("master: cannot bind UDP listener {}", l.address))?;
            info!(address = l.address, sockets = socks.len(), "master: bound UDP listener");
            udp.push((l.address.clone(), socks));
            continue;
        }
        let bound = (|| -> std::io::Result<std::net::TcpListener> {
            let addr = l
                .address
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address"))?;
            let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
            s.set_reuse_address(true)?; // what Pingora sets on its own listeners
            s.set_nonblocking(true)?;
            s.bind(&addr.into())?;
            s.listen(LISTENER_BACKLOG)?;
            Ok(s.into())
        })()
        .with_context(|| format!("master: cannot bind listener {}", l.address))?;
        info!(address = l.address, "master: bound listener");
        tcp.push((l.address.clone(), bound));
    }
    Ok(BoundListeners { tcp, udp, icmp4, icmp6 })
}

/// The instance control socket and the `control.remote` port, bound by the
/// master and kept open for its lifetime, so a control worker that dies
/// never leaves them unbound. Only the control worker serves them.
struct ControlListeners {
    socket: std::os::unix::net::UnixListener,
    /// (device, inode) of the socket file this master created.
    socket_file: Option<(u64, u64)>,
    remote: Option<std::net::TcpListener>,
}

fn file_id(path: &str) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

impl ControlListeners {
    /// Remove the socket file if it is still the one bound here: a new
    /// instance may already have bound the path while this one drained.
    fn remove_socket(&self, path: &str) {
        if self.socket_file.is_some() && self.socket_file == file_id(path) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn fds(&self) -> Vec<RawFd> {
        std::iter::once(self.socket.as_raw_fd()).chain(self.remote.as_ref().map(AsRawFd::as_raw_fd)).collect()
    }
}

fn bind_control(cfg: &Config, accounts: Option<&Accounts>) -> Result<ControlListeners> {
    use std::os::unix::fs::PermissionsExt;
    let path = &cfg.keel.control_socket;
    std::fs::create_dir_all(cfg.keel.runtime_dir()).with_context(|| format!("master: cannot create the directory of {path}"))?;
    let _ = std::fs::remove_file(path);
    let socket = std::os::unix::net::UnixListener::bind(path).with_context(|| format!("master: cannot bind {path}"))?;
    // The control protocol can drain backends and push config to the whole
    // cluster: anyone who can open the socket owns the proxy. Operators reach
    // it through the control user's group; workers cannot.
    if let Some(a) = accounts {
        nix::unistd::chown(path.as_str(), Some(a.control.uid), Some(a.control.gid))
            .with_context(|| format!("master: cannot chown {path}"))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("master: cannot restrict {path}"))?;

    let remote = match cfg.control.as_ref().and_then(|c| c.remote.as_ref()) {
        Some(r) => {
            // A literal ip:port, checked at load: nothing to resolve.
            let addr: std::net::SocketAddr = r.address.parse().context("control.remote.address")?;
            Some(std::net::TcpListener::bind(addr).with_context(|| format!("master: cannot bind control.remote {addr}"))?)
        }
        None => None,
    };
    info!(socket = path, remote = remote.is_some(), "master: control listeners bound");
    Ok(ControlListeners { socket, socket_file: file_id(path), remote })
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sighup(_: libc::c_int) {
    RELOAD.store(true, Ordering::SeqCst);
}

/// The running control worker and the master's end of its link.
struct Control {
    pid: Pid,
    link: std::os::unix::net::UnixStream,
    started: Instant,
}

/// Entry point for the master.
pub fn run(cfg: Config, force_new_cluster: bool) -> Result<()> {
    install_signal_handlers()?;
    let accounts = resolve_accounts(&cfg)?;
    match &accounts {
        Some(a) => prepare_dirs(&cfg, a)?,
        None => {
            info!("master: not root — every process runs as the invoking user");
            for dir in [crate::control::fanout::worker_socket_dir(&cfg.keel.control_socket), crate::acme::challenge_dir(&cfg.keel)] {
                std::fs::create_dir_all(&dir).with_context(|| format!("master: cannot create {}", dir.display()))?;
            }
        }
    }

    // The TCP hand-off to Pingora rides on its upgrade channel, which is
    // Linux-only; unprivileged dev runs on any platform let each worker bind
    // its own listeners.
    let bound = if cfg!(target_os = "linux") && accounts.is_some() {
        Some(bind_listeners(&cfg, cfg.keel.workers)?)
    } else {
        None
    };
    let control_listeners = bind_control(&cfg, accounts.as_ref())?;
    let data_fds = bound.as_ref().map(BoundListeners::fds).unwrap_or_default();

    let mut control = Some(spawn_control(&cfg, &control_listeners, &data_fds, accounts.as_ref(), force_new_cluster)?);
    let mut control_delay = Duration::from_secs(1);
    let mut control_due: Option<Instant> = None;

    let worker_cfg = cfg.for_workers();
    let n = cfg.keel.workers;
    info!(workers = n, "master: spawning workers");
    // (pid, index): a replacement worker keeps the index of the one it
    // replaces, so it inherits the same UDP socket of the SO_REUSEPORT group.
    let mut pids: Vec<(Pid, usize)> = Vec::with_capacity(n);
    for i in 0..n {
        let private = private_fds(&control_listeners, control.as_ref());
        let pid = spawn_worker(&worker_cfg, i, bound.as_ref(), accounts.as_ref(), &private)?;
        info!(pid = pid.as_raw(), index = i, "master: worker started");
        pids.push((pid, i));
    }

    loop {
        std::thread::sleep(TICK);

        if SHUTDOWN.load(Ordering::SeqCst) {
            info!("master: shutdown signal received, stopping");
            // SIGTERM = Pingora's plain graceful exit (finish in-flight, no
            // upgrade-socket handoff like SIGQUIT).
            let children: Vec<Pid> = pids.iter().map(|(p, _)| *p).chain(control.as_ref().map(|c| c.pid)).collect();
            for pid in &children {
                let _ = signal::kill(*pid, Signal::SIGTERM);
            }
            for pid in &children {
                let _ = waitpid(*pid, None);
            }
            control_listeners.remove_socket(&cfg.keel.control_socket);
            info!("master: all children stopped, exiting");
            return Ok(());
        }

        if RELOAD.swap(false, Ordering::SeqCst) {
            match &control {
                Some(c) => {
                    info!("master: reload signal received (SIGHUP), passing it to the control worker");
                    control_worker::send_to_control(&c.link, &MasterMessage::Reload);
                }
                None => warn!("master: SIGHUP while the control worker is restarting; send it again"),
            }
        }

        loop {
            let (dead, how) = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => (pid, format!("exit code {code}")),
                Ok(WaitStatus::Signaled(pid, sig, _)) => (pid, format!("signal {sig:?}")),
                Ok(WaitStatus::StillAlive) | Ok(WaitStatus::Continued(_)) => break,
                Ok(_) => continue,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => {
                    error!(error = %e, "master: waitpid error");
                    break;
                }
            };
            if control.as_ref().is_some_and(|c| c.pid == dead) {
                let c = control.take().expect("checked");
                // Quick deaths back off; a control worker that ran a while
                // is restarted at once.
                control_delay = if c.started.elapsed() < CONTROL_STABLE {
                    (control_delay * 2).min(CONTROL_BACKOFF_MAX)
                } else {
                    Duration::from_secs(1)
                };
                warn!(pid = dead.as_raw(), how, delay = ?control_delay, "master: control worker died, restarting");
                control_due = Some(Instant::now() + control_delay);
                continue;
            }
            warn!(pid = dead.as_raw(), how, "master: worker died, restarting");
            let index = pids.iter().find(|(p, _)| *p == dead).map(|(_, i)| *i).unwrap_or(pids.len());
            pids.retain(|(p, _)| *p != dead);
            let private = private_fds(&control_listeners, control.as_ref());
            let pid = spawn_worker(&worker_cfg, index, bound.as_ref(), accounts.as_ref(), &private)?;
            pids.push((pid, index));
            info!(index, pid = pid.as_raw(), "master: replacement worker started");
            if let Some(c) = &control {
                control_worker::send_to_control(&c.link, &MasterMessage::WorkerRestarted { index });
            }
        }

        if control.is_none() && control_due.is_some_and(|due| Instant::now() >= due) {
            control_due = None;
            control = Some(spawn_control(&cfg, &control_listeners, &data_fds, accounts.as_ref(), force_new_cluster)?);
        }
    }
}

/// Descriptors a worker must not keep: the control listeners and the link
/// to the control worker.
fn private_fds(control_listeners: &ControlListeners, control: Option<&Control>) -> Vec<RawFd> {
    control_listeners.fds().into_iter().chain(control.map(|c| c.link.as_raw_fd())).collect()
}

/// Fork the control worker. It keeps the control listeners and its end of
/// a fresh socketpair, and closes the data-plane sockets.
fn spawn_control(
    cfg: &Config,
    listeners: &ControlListeners,
    data_fds: &[RawFd],
    accounts: Option<&Accounts>,
    force_new_cluster: bool,
) -> Result<Control> {
    let (link, child_end) = std::os::unix::net::UnixStream::pair().context("master: socketpair")?;
    // The master never waits on the control worker: a message it cannot
    // take right now is dropped rather than stalling supervision.
    link.set_nonblocking(true)?;
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            drop(child_end);
            info!(pid = child.as_raw(), "master: control worker started");
            Ok(Control { pid: child, link, started: Instant::now() })
        }
        ForkResult::Child => {
            close_fds(data_fds);
            close_fds(&[link.as_raw_fd()]);
            become_account(accounts.map(|a| &a.control), "control");
            let inherited = match (listeners.socket.try_clone(), listeners.remote.as_ref().map(|r| r.try_clone()).transpose()) {
                (Ok(control_socket), Ok(remote)) => control_worker::Inherited { control_socket, remote, master: child_end },
                (Err(e), _) | (_, Err(e)) => {
                    error!(error = %e, "control: cannot take over the control listeners");
                    std::process::exit(1);
                }
            };
            control_worker::run(cfg.clone(), inherited, force_new_cluster)
        }
    }
}

/// Fork a worker. Returns the child PID in the master. The child never returns.
fn spawn_worker(
    cfg: &Config,
    index: usize,
    bound: Option<&BoundListeners>,
    accounts: Option<&Accounts>,
    private: &[RawFd],
) -> Result<Pid> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            // A worker must not hold, let alone serve, the control listeners
            // or talk on the control worker's link.
            close_fds(private);
            become_account(accounts.map(|a| &a.worker), "worker");
            let sockets = match bound.map(|b| b.for_worker(index, cfg)) {
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    error!(error = %e, "worker: cannot take over inherited sockets");
                    std::process::exit(1);
                }
                None => None,
            };
            info!(index, inherited = sockets.is_some(), "worker: started");
            proxy::run(cfg, index, sockets)
        }
    }
}

fn close_fds(fds: &[RawFd]) {
    for fd in fds {
        // SAFETY: inherited copies of the master's descriptors that this
        // child must not keep; closing them does not affect the master's.
        unsafe { libc::close(*fd) };
    }
}

/// Become `account` for good. When running as root this is a hard
/// requirement: if any step fails the child exits rather than run as root.
/// Without an account (not root) there is nothing to drop.
fn become_account(account: Option<&Account>, role: &str) {
    use nix::unistd::{getuid, setgid, setuid};
    let Some(a) = account else { return };

    // Order matters: supplementary groups, then gid, then uid — uid last so
    // the earlier privileged calls still succeed. setuid alone does NOT
    // remove root's supplementary groups.
    if let Err(e) = set_groups(&a.groups) {
        error!(role, error = %e, "setgroups failed, refusing to run as root");
        std::process::exit(1);
    }
    if let Err(e) = setgid(a.gid) {
        error!(role, error = %e, "setgid failed, refusing to run as root");
        std::process::exit(1);
    }
    if let Err(e) = setuid(a.uid) {
        error!(role, error = %e, "setuid failed, refusing to run as root");
        std::process::exit(1);
    }
    if getuid().is_root() {
        error!(role, "still root after the privilege drop, refusing to continue");
        std::process::exit(1);
    }
    info!(role, user = a.name, "dropped privileges");
}

/// nix exposes `setgroups` only off Apple targets; macOS is dev-only and runs
/// unprivileged, so the no-op there is never reached in a real drop.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn set_groups(groups: &[Gid]) -> nix::Result<()> {
    nix::unistd::setgroups(groups)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn set_groups(_: &[Gid]) -> nix::Result<()> {
    Ok(())
}

fn install_signal_handlers() -> Result<()> {
    // SIGTERM (docker stop, systemd, K8s), SIGINT (foreground ^C), and SIGQUIT
    // all trigger graceful shutdown: children are stopped and reaped, then
    // the master exits. Without a SIGTERM handler the master would die on
    // the default action and orphan its children, which keep serving.
    let shutdown = SigAction::new(SigHandler::Handler(handle_shutdown), SaFlags::SA_RESTART, SigSet::empty());
    let sighup = SigAction::new(SigHandler::Handler(handle_sighup), SaFlags::SA_RESTART, SigSet::empty());
    unsafe {
        signal::sigaction(Signal::SIGTERM, &shutdown)?;
        signal::sigaction(Signal::SIGINT, &shutdown)?;
        signal::sigaction(Signal::SIGQUIT, &shutdown)?;
        signal::sigaction(Signal::SIGHUP, &sighup)?;
        // A write to a control worker that just died must not kill the master.
        signal::sigaction(Signal::SIGPIPE, &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()))?;
    }
    Ok(())
}
