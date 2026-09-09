use crate::{config::Config, proxy};
use anyhow::{Context, Result};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info, warn};

/// Same backlog Pingora uses for the listeners it binds itself.
const LISTENER_BACKLOG: i32 = 65535;

/// Listening sockets bound by the root master before any worker is forked.
/// Workers inherit them across `fork` and never bind privileged ports
/// themselves — the nginx model.
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
        // Private hand-off socket for Pingora (see proxy::run). Lives next to
        // the control socket, which the worker user can already write to.
        let dir = std::path::Path::new(&cfg.keel.control_socket)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/var/run/keel"));
        let upgrade_sock = dir.join(format!("upgrade-{index}.sock")).to_string_lossy().into_owned();
        let icmp4 = self.icmp4.get(index).map(|s| s.try_clone()).transpose()?;
        let icmp6 = self.icmp6.get(index).map(|s| s.try_clone()).transpose()?;
        Ok(proxy::WorkerSockets { tcp, udp, icmp4, icmp6, upgrade_sock })
    }
}

/// Make every directory Keel writes to at runtime exist and belong to the
/// worker user: the control-socket directory (control socket, fd hand-off
/// sockets), the control CA directory, and the ACME storage. All are
/// written after the privilege drop; a root-owned directory (the container
/// image default) would make each of them fail. Only called as root.
fn prepare_runtime_dir(cfg: &Config) -> Result<()> {
    use nix::unistd::{chown, Group, User};

    let uid = User::from_name(&cfg.keel.user)?.map(|u| u.uid);
    let gid = Group::from_name(&cfg.keel.group)?.map(|g| g.gid);

    let socket_dir = std::path::Path::new(&cfg.keel.control_socket)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/var/run/keel"));
    let mut dirs = vec![socket_dir];
    if let Some(remote) = cfg.control.as_ref().and_then(|c| c.remote.as_ref()) {
        dirs.push(std::path::PathBuf::from(&remote.ca_dir));
    }
    if let Some(acme) = cfg.acme_effective() {
        dirs.push(std::path::PathBuf::from(&acme.storage));
    }
    for dir in dirs {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("master: cannot create {}", dir.display()))?;
        chown(&dir, uid, gid).with_context(|| format!("master: cannot chown {}", dir.display()))?;
        info!(dir = %dir.display(), user = cfg.keel.user, "master: runtime directory ready");
    }
    Ok(())
}

/// Single-process modes (cluster) have no master to bind for them. When
/// started as root on Linux, bind everything in-process, drop privileges,
/// and hand the sockets back as if a master had passed them down. Returns
/// `None` when nothing was bound (unprivileged, or not Linux), in which
/// case the process binds its own listeners as before.
pub fn take_privileges_single_process(cfg: &Config) -> Result<Option<proxy::WorkerSockets>> {
    if !(cfg!(target_os = "linux") && nix::unistd::getuid().is_root()) {
        info!("not root (or not Linux) — running as the invoking user");
        return Ok(None);
    }
    preflight_privileges(&cfg.keel.user, &cfg.keel.group)?;
    prepare_runtime_dir(cfg)?;
    let bound = bind_listeners(cfg, 1)?;
    drop_privileges(&cfg.keel.user, &cfg.keel.group);
    let sockets = bound.for_worker(0, cfg)?;
    // The raw TCP fds in `sockets` belong to `bound`; keep them open for the
    // life of the process.
    std::mem::forget(bound);
    Ok(Some(sockets))
}

/// Bind every configured listener while still root. `per_listener` UDP and
/// ICMP sockets are opened per listener — one per worker in master mode,
/// one in single-process mode (an unread SO_REUSEPORT socket would swallow
/// its share of datagrams).
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

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sighup(_: libc::c_int) {
    RELOAD.store(true, Ordering::SeqCst);
}

/// Entry point for the master (root) process.
///
/// Spawns `cfg.keel.workers` children, each of which drops privileges and
/// runs the Pingora data plane. The master supervises them — restarting any
/// that die unexpectedly — and handles SIGHUP (reload) and SIGQUIT (shutdown).
pub fn run_master(cfg: Config) -> Result<()> {
    install_signal_handlers()?;

    // Validate that workers will be able to drop privileges before forking any,
    // so a misconfigured user/group fails fast instead of fork/exit looping.
    preflight_privileges(&cfg.keel.user, &cfg.keel.group)?;

    // Bind in the master only when there are privileges to drop: that is the
    // case where workers could not bind ports below 1024 themselves. The TCP
    // hand-off to Pingora rides on its upgrade channel, which is Linux-only;
    // unprivileged dev runs on any platform keep binding in the worker.
    let bound = if cfg!(target_os = "linux") && nix::unistd::getuid().is_root() {
        prepare_runtime_dir(&cfg)?;
        Some(bind_listeners(&cfg, cfg.keel.workers)?)
    } else {
        info!("master: not root (or not Linux) — workers bind their own listeners");
        None
    };

    let n = cfg.keel.workers;
    info!(workers = n, "master: spawning workers");

    // (pid, index): a replacement worker keeps the index of the one it
    // replaces, so it inherits the same UDP socket of the SO_REUSEPORT group.
    let mut pids: Vec<(Pid, usize)> = Vec::with_capacity(n);
    for i in 0..n {
        let pid = spawn_worker(&cfg, i, bound.as_ref())?;
        info!(pid = pid.as_raw(), index = i, "master: worker started");
        pids.push((pid, i));
    }

    // Supervision loop
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            info!("master: shutdown signal received, stopping workers");
            for (pid, _) in &pids {
                // SIGTERM = Pingora's plain graceful exit (finish in-flight,
                // no upgrade-socket handoff like SIGQUIT).
                let _ = signal::kill(*pid, Signal::SIGTERM);
            }
            for (pid, _) in &pids {
                let _ = waitpid(*pid, None);
            }
            info!("master: all workers stopped, exiting");
            return Ok(());
        }

        if RELOAD.swap(false, Ordering::SeqCst) {
            info!("master: reload signal received (SIGHUP), forwarding to workers");
            for (pid, _) in &pids {
                let _ = signal::kill(*pid, Signal::SIGHUP);
            }
        }

        // Collect dead workers and restart them
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(dead, code)) => {
                    warn!(pid = dead.as_raw(), exit_code = code, "master: worker died, restarting");
                    let index = respawn(&cfg, &mut pids, dead, bound.as_ref())?;
                    info!(index, "master: replacement worker started");
                }
                Ok(WaitStatus::Signaled(dead, sig, _)) => {
                    warn!(pid = dead.as_raw(), signal = ?sig, "master: worker killed, restarting");
                    let index = respawn(&cfg, &mut pids, dead, bound.as_ref())?;
                    info!(index, "master: replacement worker started");
                }
                Ok(WaitStatus::StillAlive) | Ok(WaitStatus::Continued(_)) => break,
                Ok(_) => {}
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => {
                    error!(error = %e, "master: waitpid error");
                    break;
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Replace a dead worker, reusing its index. Returns the index.
fn respawn(
    cfg: &Config,
    pids: &mut Vec<(Pid, usize)>,
    dead: Pid,
    bound: Option<&BoundListeners>,
) -> Result<usize> {
    let index = pids
        .iter()
        .find(|(p, _)| *p == dead)
        .map(|(_, i)| *i)
        .unwrap_or(pids.len());
    pids.retain(|(p, _)| *p != dead);
    let pid = spawn_worker(cfg, index, bound)?;
    pids.push((pid, index));
    Ok(index)
}

/// Fork a worker. Returns the child PID in the master. The child never returns.
fn spawn_worker(cfg: &Config, index: usize, bound: Option<&BoundListeners>) -> Result<Pid> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            drop_privileges(&cfg.keel.user, &cfg.keel.group);
            let sockets = match bound.map(|b| b.for_worker(index, cfg)) {
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    error!(error = %e, "worker: cannot take over inherited sockets");
                    std::process::exit(1);
                }
                None => None,
            };
            run_worker(cfg, index, sockets)
        }
    }
}

/// Worker entry point — starts the Pingora data plane. Never returns.
fn run_worker(cfg: &Config, index: usize, sockets: Option<proxy::WorkerSockets>) -> ! {
    info!(index, inherited = sockets.is_some(), "worker: started");
    proxy::run(cfg, sockets)
}

/// Resolve the configured user and group up front (only meaningful as root).
/// Returns an error if either is missing so startup fails before forking workers.
fn preflight_privileges(user: &str, group: &str) -> Result<()> {
    use nix::unistd::{getuid, Group, User};

    if !getuid().is_root() {
        return Ok(());
    }
    if !matches!(Group::from_name(group), Ok(Some(_))) {
        anyhow::bail!("group '{group}' not found — cannot drop privileges (set keel.group)");
    }
    if !matches!(User::from_name(user), Ok(Some(_))) {
        anyhow::bail!("user '{user}' not found — cannot drop privileges (set keel.user)");
    }
    Ok(())
}

/// Drop root privileges to `user`:`group`.
///
/// When running as root this is a hard requirement: if any step fails the worker
/// exits rather than serve traffic with root privileges. When already unprivileged
/// (typical in dev) there is nothing to drop, so it returns quietly.
fn drop_privileges(user: &str, group: &str) {
    use nix::unistd::{getuid, setgid, setuid, Group, User};

    if !getuid().is_root() {
        warn!("worker: not running as root, skipping privilege drop");
        return;
    }

    let gid = match Group::from_name(group) {
        Ok(Some(g)) => g.gid,
        _ => {
            error!(group, "worker: group not found, refusing to run as root");
            std::process::exit(1);
        }
    };
    let uid = match User::from_name(user) {
        Ok(Some(u)) => u.uid,
        _ => {
            error!(user, "worker: user not found, refusing to run as root");
            std::process::exit(1);
        }
    };

    // Order matters: drop supplementary groups, then gid, then uid. uid last so
    // the earlier privileged calls still succeed. setgroups([]) clears root's
    // supplementary groups — setuid alone does NOT remove them.
    if let Err(e) = clear_supplementary_groups() {
        error!(error = %e, "worker: setgroups failed, refusing to run as root");
        std::process::exit(1);
    }
    if let Err(e) = setgid(gid) {
        error!(error = %e, "worker: setgid failed, refusing to run as root");
        std::process::exit(1);
    }
    if let Err(e) = setuid(uid) {
        error!(error = %e, "worker: setuid failed, refusing to run as root");
        std::process::exit(1);
    }

    // Sanity check: privileges must actually be gone.
    if getuid().is_root() {
        error!("worker: still root after privilege drop, refusing to continue");
        std::process::exit(1);
    }

    info!(user, group, "worker: dropped privileges");
}

/// Drop all supplementary groups. nix exposes `setgroups` only off Apple targets;
/// macOS is dev-only and runs unprivileged, so the no-op there is never reached
/// in a real privilege drop.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn clear_supplementary_groups() -> nix::Result<()> {
    nix::unistd::setgroups(&[])
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn clear_supplementary_groups() -> nix::Result<()> {
    Ok(())
}

fn install_signal_handlers() -> Result<()> {
    // SIGTERM (docker stop, systemd, K8s), SIGINT (foreground ^C), and SIGQUIT
    // all trigger graceful shutdown: workers are stopped and reaped, then the
    // master exits. Without a SIGTERM handler the master would die on the
    // default action and orphan its workers, which keep serving.
    let shutdown = SigAction::new(SigHandler::Handler(handle_shutdown), SaFlags::SA_RESTART, SigSet::empty());
    let sighup = SigAction::new(SigHandler::Handler(handle_sighup), SaFlags::SA_RESTART, SigSet::empty());
    unsafe {
        signal::sigaction(Signal::SIGTERM, &shutdown)?;
        signal::sigaction(Signal::SIGINT, &shutdown)?;
        signal::sigaction(Signal::SIGQUIT, &shutdown)?;
        signal::sigaction(Signal::SIGHUP, &sighup)?;
        // Ignore SIGPIPE — broken pipe on a client must not kill the master
        signal::sigaction(
            Signal::SIGPIPE,
            &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()),
        )?;
    }
    Ok(())
}
