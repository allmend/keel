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

mod upgrade;

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

/// A child that dies sooner than this after starting is restarted with a
/// growing delay, so a start that keeps failing (a backend that does not
/// resolve, a refused join, a broken store) does not spin. One that ran
/// longer is restarted at once.
const STABLE: Duration = Duration::from_secs(10);
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The restart delay of one child: the control worker, or one worker index.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { next: BACKOFF_FIRST }
    }
}

impl Backoff {
    /// How long to wait before restarting a child that ran for `ran`.
    fn after_death(&mut self, ran: Duration) -> Duration {
        if ran >= STABLE {
            self.next = BACKOFF_FIRST;
            return Duration::ZERO;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(BACKOFF_MAX);
        delay
    }
}

/// Listening sockets bound by the master before any worker is forked (on
/// Linux, root or not). Workers inherit them across `fork`: they never bind
/// privileged ports themselves, and every worker accepts on the same socket.
pub struct BoundListeners {
    /// One listening socket per TCP listener, shared by every worker; the
    /// kernel spreads `accept` across the processes.
    pub tcp: Vec<(String, std::net::TcpListener)>,
    /// One socket per worker per UDP listener, all in one SO_REUSEPORT group,
    /// so the kernel's client→socket hash keeps every flow on one worker.
    pub udp: Vec<(String, Vec<std::net::UdpSocket>)>,
    /// One ICMP datagram socket per family for the control worker, which
    /// runs the health checks, when any pool uses an `icmp` check. Needs
    /// CAP_NET_RAW or `ping_group_range`: a root master has it, a non-root
    /// one may not (checks then pass).
    pub icmp4: Option<std::net::UdpSocket>,
    pub icmp6: Option<std::net::UdpSocket>,
}

impl BoundListeners {
    /// The sockets a previous binary of this master bound, still open.
    fn adopt(h: &upgrade::Handover) -> BoundListeners {
        use std::os::fd::FromRawFd;
        // SAFETY: descriptors this process held before its exec, open across it.
        unsafe {
            BoundListeners {
                tcp: h.tcp.iter().map(|(a, fd)| (a.clone(), std::net::TcpListener::from_raw_fd(*fd))).collect(),
                udp: h.udp.iter().map(|(a, fds)| (a.clone(), fds.iter().map(|fd| std::net::UdpSocket::from_raw_fd(*fd)).collect())).collect(),
                icmp4: h.icmp4.map(|fd| std::net::UdpSocket::from_raw_fd(fd)),
                icmp6: h.icmp6.map(|fd| std::net::UdpSocket::from_raw_fd(fd)),
            }
        }
    }

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
        Ok(proxy::WorkerSockets { tcp, udp, upgrade_sock })
    }

    /// The data plane's sockets, which the control worker closes.
    fn fds(&self) -> Vec<RawFd> {
        let tcp = self.tcp.iter().map(|(_, l)| l.as_raw_fd());
        let udp = self.udp.iter().flat_map(|(_, s)| s.iter().map(AsRawFd::as_raw_fd));
        tcp.chain(udp).collect()
    }

    /// The control worker's ICMP sockets, which a worker closes.
    fn icmp_fds(&self) -> Vec<RawFd> {
        self.icmp4.iter().chain(&self.icmp6).map(AsRawFd::as_raw_fd).collect()
    }

    /// The ICMP sockets, for the control worker.
    fn icmp(&self) -> std::io::Result<crate::health::icmp::IcmpSockets> {
        Ok(crate::health::icmp::IcmpSockets {
            v4: self.icmp4.as_ref().map(|s| s.try_clone()).transpose()?,
            v6: self.icmp6.as_ref().map(|s| s.try_clone()).transpose()?,
        })
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
    if !nix::unistd::getuid().is_root() {
        return Ok(None);
    }
    let Some(gid) = group_id(&cfg.keel.group) else {
        anyhow::bail!("group '{}' not found — cannot drop privileges (set keel.group)", cfg.keel.group);
    };
    let Some(user) = user_of(&cfg.keel.user) else {
        anyhow::bail!("user '{}' not found — cannot drop privileges (set keel.user)", cfg.keel.user);
    };
    let Some(control) = user_of(&cfg.keel.control_user) else {
        anyhow::bail!(
            "user '{}' not found — the control worker runs as its own user (create it, or set keel.control_user)",
            cfg.keel.control_user
        );
    };
    Ok(Some(Accounts {
        worker: Account { gid, ..user },
        control: Account { groups: vec![gid], ..control },
    }))
}

/// A user by name, or by number when the system knows no such name (a
/// container without /etc/passwd): that uid, with the gid of the same number.
fn user_of(name: &str) -> Option<Account> {
    use nix::unistd::{Gid, Uid, User};
    let account = |name: String, uid, gid| Account { name, uid, gid, groups: Vec::new() };
    if let Ok(Some(u)) = User::from_name(name) {
        return Some(account(u.name, u.uid, u.gid));
    }
    let id: u32 = name.parse().ok()?;
    Some(match User::from_uid(Uid::from_raw(id)) {
        Ok(Some(u)) => account(u.name, u.uid, u.gid),
        _ => account(name.to_owned(), Uid::from_raw(id), Gid::from_raw(id)),
    })
}

/// A group by name, or by number.
fn group_id(name: &str) -> Option<nix::unistd::Gid> {
    use nix::unistd::{Gid, Group};
    match Group::from_name(name) {
        Ok(Some(group)) => Some(group.gid),
        _ => name.parse().ok().map(Gid::from_raw),
    }
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

/// Bind every configured listener before forking (as root, before the
/// privilege drop). `per_listener` UDP sockets are opened per listener —
/// one per worker (an unread SO_REUSEPORT socket would swallow its share of
/// datagrams).
/// Bind every listener of `cfg` on this node. With `previous`, a socket it
/// already holds for an address is shared, not bound again: old and new
/// workers then accept from the same queue.
fn bind_listeners(cfg: &Config, per_listener: usize, root: bool, previous: Option<&BoundListeners>) -> Result<BoundListeners> {
    use socket2::{Domain, Protocol, Socket, Type};

    let mut tcp = Vec::new();
    let mut udp = Vec::new();
    let (mut icmp4, mut icmp6) = (None, None);
    if let Some(p) = previous.filter(|_| crate::health::icmp::needed(cfg)) {
        icmp4 = p.icmp4.as_ref().map(|s| s.try_clone()).transpose()?;
        icmp6 = p.icmp6.as_ref().map(|s| s.try_clone()).transpose()?;
    } else if crate::health::icmp::needed(cfg) {
        match crate::health::icmp::open_socket(false) {
            Ok(s) => icmp4 = Some(s),
            Err(e) if root => return Err(e).context("master: cannot open ICMPv4 socket"),
            // Without root it takes `net.ipv4.ping_group_range`.
            Err(e) => warn!(error = %e, "master: cannot open ICMPv4 socket — icmp checks of IPv4 backends pass unconditionally"),
        }
        match crate::health::icmp::open_socket(true) {
            Ok(s) => icmp6 = Some(s),
            // IPv6 may be disabled on the host; v6 backends then pass unchecked.
            Err(e) => warn!(error = %e, "master: cannot open ICMPv6 socket — icmp checks of IPv6 backends pass unconditionally"),
        }
        info!(v4 = icmp4.is_some(), v6 = icmp6.is_some(), "master: opened ICMP sockets for health checks");
    }
    for l in &cfg.listeners {
        let (here, elsewhere) = l.split_here();
        if !elsewhere.is_empty() {
            let skipped: Vec<String> = elsewhere.iter().map(ToString::to_string).collect();
            info!(listener = l.name, skipped = skipped.join(", "), "master: addresses not on this node");
        }
        for addr in here {
            let address = addr.to_string();
            if l.protocol == crate::config::Protocol::Udp {
                let held = previous.and_then(|p| p.udp.iter().find(|(a, s)| *a == address && s.len() == per_listener));
                if let Some((_, socks)) = held {
                    udp.push((address, socks.iter().map(|s| s.try_clone()).collect::<std::io::Result<_>>()?));
                    continue;
                }
                let socks = (0..per_listener)
                    .map(|_| crate::udp::bind_reuseport(&address))
                    .collect::<std::io::Result<Vec<_>>>()
                    .with_context(|| format!("master: cannot bind listener {} on {address}", l.name))?;
                info!(listener = l.name, address, sockets = socks.len(), "master: bound UDP listener");
                udp.push((address, socks));
                continue;
            }
            if let Some((_, held)) = previous.and_then(|p| p.tcp.iter().find(|(a, _)| *a == address)) {
                tcp.push((address, held.try_clone()?));
                continue;
            }
            let bound = (|| -> std::io::Result<std::net::TcpListener> {
                let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
                s.set_reuse_address(true)?; // what Pingora sets on its own listeners
                if addr.is_ipv6() {
                    s.set_only_v6(true)?;
                }
                s.set_nonblocking(true)?;
                s.bind(&addr.into())?;
                s.listen(LISTENER_BACKLOG)?;
                Ok(s.into())
            })()
            .with_context(|| format!("master: cannot bind listener {} on {address}", l.name))?;
            info!(listener = l.name, address, "master: bound listener");
            tcp.push((address, bound));
        }
    }
    Ok(BoundListeners { tcp, udp, icmp4, icmp6 })
}

/// The instance control socket, the `control.remote` port and the metrics
/// port, bound by the master and kept open for its lifetime, so a control
/// worker that dies never leaves them unbound. Only the control worker
/// serves them.
struct ControlListeners {
    socket: std::os::unix::net::UnixListener,
    /// (device, inode) of the socket file this master created.
    socket_file: Option<(u64, u64)>,
    remote: Option<std::net::TcpListener>,
    metrics: std::net::TcpListener,
}

fn file_id(path: &str) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

impl ControlListeners {
    fn adopt(h: &upgrade::Handover) -> ControlListeners {
        use std::os::fd::FromRawFd;
        // SAFETY: descriptors this process held before its exec, open across it.
        unsafe {
            ControlListeners {
                socket: std::os::unix::net::UnixListener::from_raw_fd(h.control_socket),
                socket_file: h.control_socket_file,
                remote: h.control_remote.map(|fd| std::net::TcpListener::from_raw_fd(fd)),
                metrics: std::net::TcpListener::from_raw_fd(h.metrics),
            }
        }
    }

    /// The listeners `next` asks for: an unchanged address keeps its socket,
    /// a changed one is bound anew. The control socket path cannot change.
    fn rebind(&self, cfg: &Config, next: &Config) -> Result<ControlListeners> {
        let remote_of = |c: &Config| c.control.as_ref().and_then(|c| c.remote.as_ref()).map(|r| r.address.clone());
        let remote = match remote_of(next) {
            None => None,
            Some(a) if Some(&a) == remote_of(cfg).as_ref() => self.remote.as_ref().map(|r| r.try_clone()).transpose()?,
            Some(a) => {
                let addr: std::net::SocketAddr = a.parse().context("control.remote.address")?;
                Some(std::net::TcpListener::bind(addr).with_context(|| format!("master: cannot bind control.remote {addr}"))?)
            }
        };
        let metrics = if next.metrics.address == cfg.metrics.address {
            self.metrics.try_clone()?
        } else {
            std::net::TcpListener::bind(&next.metrics.address)
                .with_context(|| format!("master: cannot bind metrics.address {}", next.metrics.address))?
        };
        info!(remote = remote.is_some(), metrics = next.metrics.address, "master: control listeners bound");
        Ok(ControlListeners { socket: self.socket.try_clone()?, socket_file: self.socket_file, remote, metrics })
    }

    /// Remove the socket file if it is still the one bound here: a new
    /// instance may already have bound the path while this one drained.
    fn remove_socket(&self, path: &str) {
        if self.socket_file.is_some() && self.socket_file == file_id(path) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn fds(&self) -> Vec<RawFd> {
        [self.socket.as_raw_fd(), self.metrics.as_raw_fd()].into_iter().chain(self.remote.as_ref().map(AsRawFd::as_raw_fd)).collect()
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
    let metrics = std::net::TcpListener::bind(&cfg.metrics.address)
        .with_context(|| format!("master: cannot bind metrics.address {}", cfg.metrics.address))?;
    info!(socket = path, remote = remote.is_some(), metrics = cfg.metrics.address, "master: control listeners bound");
    Ok(ControlListeners { socket, socket_file: file_id(path), remote, metrics })
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
/// `keel config reload` reached the control worker: read node.hcl again.
static NODE: AtomicBool = AtomicBool::new(false);
/// SIGUSR2 or `keel upgrade`: execute the binary on disk in place.
static UPGRADE: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_upgrade(_: libc::c_int) {
    UPGRADE.store(true, Ordering::SeqCst);
}

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
    let mut cfg = cfg;
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
    // Linux-only; elsewhere each worker binds its own listeners with
    // SO_REUSEPORT (proxy::listen). Not root does not matter: without the
    // shared socket all workers but one would wait for a port that never
    // frees.
    // Where the binary is now: an upgrade executes what is there then.
    let exe = std::env::current_exe().context("master: cannot find its own binary")?;
    // After keel upgrade the sockets, the metrics region and the workers are
    // the previous image's, still open and running.
    let handover = upgrade::take();
    let bound = match &handover {
        Some(h) if cfg!(target_os = "linux") => Some(BoundListeners::adopt(h)),
        _ if cfg!(target_os = "linux") => Some(bind_listeners(&cfg, cfg.keel.workers, accounts.is_some(), None)?),
        _ => None,
    };
    let mut control_listeners = match &handover {
        Some(h) => ControlListeners::adopt(h),
        None => bind_control(&cfg, accounts.as_ref())?,
    };
    match handover.as_ref().and_then(|h| h.region) {
        Some(fd) => crate::metrics::adopt_region(fd, &cfg)?,
        None => crate::metrics::create_region(&cfg)?,
    }
    // The previous image's workers: they drain once ours are ready.
    let mut inherited: Vec<Generation> = handover
        .iter()
        .flat_map(|h| &h.generations)
        .map(|h| {
            let mut g = Generation::new(h.slot, cfg.for_workers());
            g.workers = h.workers.iter().map(|(pid, index)| (Pid::from_raw(*pid), *index, Instant::now())).collect();
            g.closing = h.draining;
            g
        })
        .collect();
    if handover.is_some() {
        info!(generations = inherited.len(), "master: upgraded; taking over from the previous binary");
    }

    let mut control = Some(spawn_control(&cfg, &control_listeners, bound.as_ref(), accounts.as_ref(), force_new_cluster)?);
    let mut control_backoff = Backoff::default();
    let mut control_due: Option<Instant> = None;

    let root = accounts.is_some();
    let mut bound = bound;
    let slot = (0..crate::metrics::GENERATION_SLOTS).find(|s| !inherited.iter().any(|g| g.slot == *s)).unwrap_or(0);
    let mut current = Generation::new(slot, cfg.for_workers());
    // Workers of the previous binary's current generation hand their UDP flows on.
    let predecessors: Vec<usize> = inherited.iter().find(|g| !g.closing).map(|g| g.workers.iter().map(|(_, i, _)| *i).collect()).unwrap_or_default();
    info!(workers = current.cfg.keel.workers, "master: spawning workers");
    // Worker indexes waiting for their restart, and when it is due. Only the
    // current generation's workers come back; each index backs off on its own.
    let mut backoff = vec![Backoff::default(); current.cfg.keel.workers];
    let mut due: Vec<(usize, Instant)> = Vec::new();
    for i in 0..current.cfg.keel.workers {
        let private = private_fds(&control_listeners, control.as_ref(), bound.as_ref());
        let pid = spawn_worker(&current.cfg, i, current.stripe(i), predecessors.contains(&i), bound.as_ref(), accounts.as_ref(), &private)?;
        info!(pid = pid.as_raw(), index = i, "master: worker started");
        current.workers.push((pid, i, Instant::now()));
        // After an upgrade the previous workers still answer on these
        // sockets: the control worker hands the state to this pid.
        if let (Some(c), false) = (&control, inherited.is_empty()) {
            control_worker::send_to_control(&c.link, &MasterMessage::WorkerRestarted { index: i, pid: Some(pid.as_raw() as u32) });
        }
    }
    crate::metrics::set_workers(current.workers.len());
    // A generation being started for a new version, with its sockets, and
    // the generations a newer one replaced, finishing their connections.
    let mut pending: Option<(Generation, Option<BoundListeners>, Instant)> = None;
    let mut draining: Vec<Generation> = Vec::new();
    let mut nudged = Vec::new();
    // A nudge that came while a generation was starting waits for it.
    let mut wanted = false;
    // The next generation starts from the node file, not from a version the
    // control worker handed over.
    let mut node_changed = false;

    loop {
        std::thread::sleep(TICK);

        // After an upgrade: the previous binary's workers drain once ours
        // have the node's state.
        if !inherited.is_empty() && current.ready(&cfg.keel.control_socket) {
            for mut g in inherited.drain(..) {
                if !g.closing {
                    g.stop(Signal::SIGUSR1);
                }
                g.closing = false;
                draining.push(g);
            }
            info!("master: upgrade complete; the previous binary's workers drain");
            crate::metrics::set_draining(draining.iter().map(|g| g.workers.len()).sum());
        }

        if UPGRADE.swap(false, Ordering::SeqCst) {
            if pending.is_some() || !inherited.is_empty() {
                warn!("master: upgrade asked while new workers are starting; ask again when they are ready");
            } else if let Err(e) = upgrade::preflight(&exe, &cfg.path) {
                error!(error = %format!("{e:#}"), "master: upgrade refused; the new binary does not run with this config");
            } else {
                info!(binary = %exe.display(), "master: upgrading; stopping the control worker first");
                if let Some(c) = control.take() {
                    stop_and_reap(c.pid, Duration::from_secs(15));
                }
                let handover = upgrade::Handover {
                    tcp: bound.iter().flat_map(|b| b.tcp.iter().map(|(a, l)| (a.clone(), l.as_raw_fd()))).collect(),
                    udp: bound.iter().flat_map(|b| b.udp.iter().map(|(a, s)| (a.clone(), s.iter().map(AsRawFd::as_raw_fd).collect()))).collect(),
                    icmp4: bound.as_ref().and_then(|b| b.icmp4.as_ref().map(AsRawFd::as_raw_fd)),
                    icmp6: bound.as_ref().and_then(|b| b.icmp6.as_ref().map(AsRawFd::as_raw_fd)),
                    control_socket: control_listeners.socket.as_raw_fd(),
                    control_socket_file: control_listeners.socket_file,
                    control_remote: control_listeners.remote.as_ref().map(AsRawFd::as_raw_fd),
                    metrics: control_listeners.metrics.as_raw_fd(),
                    region: crate::metrics::region_fd(),
                    generations: std::iter::once(&current)
                        .map(|g| (g, false))
                        .chain(draining.iter().map(|g| (g, true)))
                        .map(|(g, draining)| upgrade::Handed {
                            slot: g.slot,
                            draining,
                            workers: g.workers.iter().map(|(p, i, _)| (p.as_raw(), *i)).collect(),
                        })
                        .collect(),
                };
                let e = upgrade::exec(&exe, &handover);
                error!(error = %format!("{e:#}"), "master: upgrade failed; carrying on with this binary");
                control = Some(spawn_control(&cfg, &control_listeners, bound.as_ref(), accounts.as_ref(), force_new_cluster)?);
            }
        }

        if SHUTDOWN.load(Ordering::SeqCst) {
            info!("master: shutdown signal received, stopping");
            // SIGTERM = Pingora's plain graceful exit (finish in-flight, no
            // upgrade-socket handoff like SIGQUIT).
            let generations = std::iter::once(&current).chain(pending.as_ref().map(|(g, _, _)| g)).chain(&draining);
            let children: Vec<Pid> =
                generations.flat_map(|g| g.workers.iter().map(|(p, _, _)| *p)).chain(control.as_ref().map(|c| c.pid)).collect();
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

        if RELOAD.load(Ordering::SeqCst) || NODE.swap(false, Ordering::SeqCst) {
            // The node file is the operator's: the master reads it itself.
            let node_file = crate::config::read_node_file(&cfg.path).unwrap_or_else(|_| cfg.node_file.clone());
            if node_file != cfg.node_file {
                match with_node_file(&cfg, &node_file, &current.cfg.files) {
                    Err(e) => error!(error = %format!("{e:#}"), "master: node file not applied; it does not load"),
                    Ok(next) if !needs_new_master(&cfg, &next).is_empty() => error!(
                        settings = needs_new_master(&cfg, &next).join(", "),
                        "master: node file not applied; these need a new master (keel upgrade)"
                    ),
                    Ok(next) => {
                        info!("master: node file changed; restarting the control worker with it");
                        let addresses = |c: &Config| (c.metrics.address.clone(), c.control.as_ref().and_then(|c| c.remote.as_ref()).map(|r| r.address.clone()));
                        if addresses(&cfg) != addresses(&next) {
                            match control_listeners.rebind(&cfg, &next) {
                                Ok(l) => control_listeners = l,
                                Err(e) => error!(error = %format!("{e:#}"), "master: new metrics or control address not bound; keeping the old ones"),
                            }
                        }
                        if let Some(c) = control.take() {
                            let _ = signal::kill(c.pid, Signal::SIGTERM);
                            let _ = waitpid(c.pid, None);
                        }
                        let workers_changed = next.keel.workers != cfg.keel.workers;
                        cfg = next;
                        control = Some(spawn_control(&cfg, &control_listeners, bound.as_ref(), accounts.as_ref(), force_new_cluster)?);
                        control_due = None;
                        if workers_changed {
                            wanted = true;
                            node_changed = true;
                        }
                    }
                }
            }
            match &control {
                // `keel config reload` reached the control worker already.
                _ if !RELOAD.swap(false, Ordering::SeqCst) => {}
                Some(c) => {
                    info!("master: reload signal received (SIGHUP), passing it to the control worker");
                    control_worker::send_to_control(&c.link, &MasterMessage::Reload);
                }
                None => warn!("master: SIGHUP while the control worker is restarting; send it again"),
            }
        }

        // The control worker's one message: a version needs new workers.
        if let Some(c) = &control {
            wanted |= read_nudge(&c.link, &mut nudged);
            if wanted && pending.is_none() {
                wanted = false;
                let spawn = Spawn { control_listeners: &control_listeners, control: Some(c), accounts: accounts.as_ref() };
                // The version the control worker handed over: the master reads
                // it from the state directory itself and validates it like
                // any config. A node file change runs the current files.
                let next = if std::mem::take(&mut node_changed) {
                    with_node_file(&cfg, &cfg.node_file, &current.cfg.files)
                } else {
                    crate::cluster::applied::read_for_master(std::path::Path::new(&cfg.keel.state_dir))
                        .and_then(|files| with_node_file(&cfg, &cfg.node_file, &files))
                };
                match next.and_then(|next| start_generation(next, &current, &draining, bound.as_ref(), root, &spawn)) {
                    Ok((generation, sockets)) => pending = Some((generation, sockets, Instant::now())),
                    Err(e) => {
                        error!(error = %format!("{e:#}"), "master: new workers not started; the current ones keep serving");
                        crate::metrics::replacement_failed();
                    }
                }
            }
        }

        loop {
            let (dead, how, signaled) = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => (pid, format!("exit code {code}"), false),
                Ok(WaitStatus::Signaled(pid, sig, _)) => (pid, format!("signal {sig:?}"), true),
                Ok(WaitStatus::StillAlive) | Ok(WaitStatus::Continued(_)) => break,
                Ok(_) => continue,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => {
                    error!(error = %e, "master: waitpid error");
                    break;
                }
            };
            let _ = std::fs::remove_file(crate::control::fanout::worker_socket_dir(&cfg.keel.control_socket).join(format!("ready-{}", dead.as_raw())));
            if control.as_ref().is_some_and(|c| c.pid == dead) {
                let c = control.take().expect("checked");
                let delay = control_backoff.after_death(c.started.elapsed());
                warn!(pid = dead.as_raw(), how, delay = ?delay, "master: control worker died, restarting");
                control_due = Some(Instant::now() + delay);
                continue;
            }
            if let Some(g) = inherited.iter_mut().find(|g| g.has(dead)) {
                g.remove(dead);
                inherited.retain(|g| !g.workers.is_empty());
                continue;
            }
            if let Some(g) = draining.iter_mut().find(|g| g.has(dead)) {
                let index = g.remove(dead);
                crate::metrics::clear_stripe(g.stripe(index));
                info!(pid = dead.as_raw(), index, how, "master: replaced worker finished");
                draining.retain(|g| !g.workers.is_empty());
                crate::metrics::set_draining(draining.iter().map(|g| g.workers.len()).sum());
                continue;
            }
            if pending.as_ref().is_some_and(|(g, _, _)| g.has(dead)) {
                let (g, _, _) = pending.take().expect("checked");
                error!(pid = dead.as_raw(), how, "master: a new worker died at start; the current ones keep serving");
                g.stop(Signal::SIGKILL);
                crate::metrics::replacement_failed();
                continue;
            }
            let Some(pos) = current.workers.iter().position(|(p, _, _)| *p == dead) else { continue };
            let (_, index, started) = current.workers.remove(pos);
            let delay = backoff[index].after_death(started.elapsed());
            warn!(index, pid = dead.as_raw(), how, delay = ?delay, "master: worker died, restarting");
            crate::metrics::set_workers(current.workers.len());
            crate::metrics::worker_restarted(signaled);
            due.push((index, Instant::now() + delay));
        }

        // A new generation takes over once every worker has the node's state.
        if let Some((g, _, started)) = &pending {
            if g.ready(&cfg.keel.control_socket) {
                let (g, sockets, _) = pending.take().expect("checked");
                info!(workers = g.workers.len(), "master: new workers ready; the replaced ones drain");
                let old = std::mem::replace(&mut current, g);
                old.stop(Signal::SIGUSR1);
                draining.push(old);
                // Past the limit, the oldest still draining closes what it holds.
                let open = draining.iter().filter(|g| !g.closing).count();
                if open > MAX_DRAINING {
                    if let Some(oldest) = draining.iter_mut().find(|g| !g.closing) {
                        warn!(workers = oldest.workers.len(), "master: too many generations draining; closing the oldest");
                        oldest.stop(Signal::SIGINT);
                        oldest.closing = true;
                    }
                }
                if sockets.is_some() {
                    bound = sockets;
                }
                backoff = vec![Backoff::default(); current.cfg.keel.workers];
                due.clear();
                crate::metrics::set_workers(current.workers.len());
                crate::metrics::set_workers_configured(current.cfg.keel.workers);
                crate::metrics::set_draining(draining.iter().map(|g| g.workers.len()).sum());
            } else if started.elapsed() > READY_TIMEOUT {
                let (g, _, _) = pending.take().expect("checked");
                error!("master: new workers not ready in time; the current ones keep serving");
                g.stop(Signal::SIGKILL);
                crate::metrics::replacement_failed();
            }
        }

        let now = Instant::now();
        let ready: Vec<usize> = due.iter().filter(|(_, at)| *at <= now).map(|(index, _)| *index).collect();
        due.retain(|(_, at)| *at > now);
        for index in ready {
            let private = private_fds(&control_listeners, control.as_ref(), bound.as_ref());
            let pid = spawn_worker(&current.cfg, index, current.stripe(index), false, bound.as_ref(), accounts.as_ref(), &private)?;
            current.workers.push((pid, index, Instant::now()));
            crate::metrics::set_workers(current.workers.len());
            info!(index, pid = pid.as_raw(), "master: replacement worker started");
            if let Some(c) = &control {
                control_worker::send_to_control(&c.link, &MasterMessage::WorkerRestarted { index, pid: Some(pid.as_raw() as u32) });
            }
        }

        if control.is_none() && control_due.is_some_and(|due| Instant::now() >= due) {
            control_due = None;
            control = Some(spawn_control(&cfg, &control_listeners, bound.as_ref(), accounts.as_ref(), force_new_cluster)?);
        }
    }
}

/// Stop a child and wait for it, killing it after `wait`.
fn stop_and_reap(pid: Pid, wait: Duration) {
    let _ = signal::kill(pid, Signal::SIGTERM);
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !matches!(waitpid(pid, Some(WaitPidFlag::WNOHANG)), Ok(WaitStatus::StillAlive)) {
            return;
        }
        std::thread::sleep(TICK);
    }
    let _ = signal::kill(pid, Signal::SIGKILL);
    let _ = waitpid(pid, None);
}

/// Generations draining at once; a further one closes the oldest's
/// connections first.
const MAX_DRAINING: usize = 2;

/// How long new workers may take to have the node's state.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// One set of workers started from one config.
struct Generation {
    /// Its metrics stripes (see metrics::worker_stripe).
    slot: usize,
    cfg: Config,
    /// (pid, index, started)
    workers: Vec<(Pid, usize, Instant)>,
    /// Told to close what it holds.
    closing: bool,
}

impl Generation {
    fn new(slot: usize, cfg: Config) -> Self {
        Generation { slot, cfg, workers: Vec::new(), closing: false }
    }

    fn stripe(&self, index: usize) -> usize {
        crate::metrics::worker_stripe(self.slot, index).expect("validated against the region")
    }

    fn has(&self, pid: Pid) -> bool {
        self.workers.iter().any(|(p, _, _)| *p == pid)
    }

    /// Forget a worker that exited; its index.
    fn remove(&mut self, pid: Pid) -> usize {
        let pos = self.workers.iter().position(|(p, _, _)| *p == pid).expect("checked");
        self.workers.remove(pos).1
    }

    fn stop(&self, sig: Signal) {
        for (pid, _, _) in &self.workers {
            let _ = signal::kill(*pid, sig);
        }
    }

    /// Every worker marked itself ready: it has the node's applied state.
    /// Only the marker's existence is read, nothing in it.
    fn ready(&self, control_socket: &str) -> bool {
        let dir = crate::control::fanout::worker_socket_dir(control_socket);
        self.workers.iter().all(|(pid, _, _)| dir.join(format!("ready-{}", pid.as_raw())).exists())
    }
}

/// `files` assembled against `node_file`, at the master's node path.
fn with_node_file(cfg: &Config, node_file: &str, files: &crate::config::FileSet) -> Result<Config> {
    let mut next = crate::config::assemble(node_file, files)?;
    next.path = cfg.path.clone();
    next.node_file = node_file.to_owned();
    Ok(next)
}

/// Node settings only a new master can change: the process users, and the
/// paths every process was started with.
fn needs_new_master(old: &Config, new: &Config) -> Vec<&'static str> {
    let (a, b) = (&old.keel, &new.keel);
    [
        ("keel.user", a.user != b.user),
        ("keel.group", a.group != b.group),
        ("keel.control_user", a.control_user != b.control_user),
        ("keel.control_socket", a.control_socket != b.control_socket),
        ("keel.state_dir", a.state_dir != b.state_dir),
        ("keel.config_dir", a.config_dir != b.config_dir),
    ]
    .into_iter()
    .filter_map(|(name, changed)| changed.then_some(name))
    .collect()
}

/// What forking a worker needs besides its config.
#[derive(Clone, Copy)]
struct Spawn<'a> {
    control_listeners: &'a ControlListeners,
    control: Option<&'a Control>,
    accounts: Option<&'a Accounts>,
}

/// Start a generation for `next`; sockets the master already holds are
/// shared, new ones bound.
fn start_generation(
    next: Config,
    current: &Generation,
    draining: &[Generation],
    bound: Option<&BoundListeners>,
    root: bool,
    spawn: &Spawn,
) -> Result<(Generation, Option<BoundListeners>)> {
    let Spawn { control_listeners, control, accounts } = *spawn;
    let busy: Vec<usize> = std::iter::once(current).chain(draining).map(|g| g.slot).collect();
    // A new worker whose index the current generation has gets its UDP flows.
    let predecessors: Vec<usize> = current.workers.iter().map(|(_, index, _)| *index).collect();
    let slot = (0..crate::metrics::GENERATION_SLOTS).find(|s| !busy.contains(s)).context("no generation slot free")?;
    let workers = next.keel.workers;
    if crate::metrics::worker_stripe(slot, workers.saturating_sub(1)).is_none() {
        anyhow::bail!("keel.workers = {workers} is more than this master can run; it needs keel upgrade");
    }
    let sockets = match bound {
        Some(b) => Some(bind_listeners(&next, workers, root, Some(b))?),
        None => None,
    };
    let mut g = Generation::new(slot, next.for_workers());
    info!(workers, "master: starting new workers for a new version");
    for index in 0..workers {
        let private = private_fds(control_listeners, control, sockets.as_ref());
        let pid = spawn_worker(&g.cfg, index, g.stripe(index), predecessors.contains(&index), sockets.as_ref(), accounts, &private)?;
        g.workers.push((pid, index, Instant::now()));
        if let Some(c) = control {
            control_worker::send_to_control(&c.link, &MasterMessage::WorkerRestarted { index, pid: Some(pid.as_raw() as u32) });
        }
    }
    Ok((g, sockets))
}

/// Whether the control worker sent the replace nudge since the last call.
/// Anything else on the link is dropped unread.
fn read_nudge(link: &std::os::unix::net::UnixStream, buf: &mut Vec<u8>) -> bool {
    use std::io::Read;
    let mut chunk = [0u8; 64];
    let mut nudged = false;
    loop {
        match (&*link).read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        while let Some(end) = buf.iter().position(|b| *b == b'\n') {
            nudged |= buf[..=end] == *control_worker::REPLACE_NUDGE;
            if buf[..=end] == *control_worker::NODE_NUDGE {
                NODE.store(true, Ordering::SeqCst);
            }
            if buf[..=end] == *control_worker::UPGRADE_NUDGE {
                UPGRADE.store(true, Ordering::SeqCst);
            }
            buf.drain(..=end);
        }
        // A line longer than the nudge is not one.
        if buf.len() > control_worker::REPLACE_NUDGE.len() {
            buf.clear();
        }
    }
    nudged
}

/// Descriptors a worker must not keep: the control listeners, the link to
/// the control worker, and its ICMP sockets.
fn private_fds(control_listeners: &ControlListeners, control: Option<&Control>, bound: Option<&BoundListeners>) -> Vec<RawFd> {
    let icmp = bound.map(BoundListeners::icmp_fds).unwrap_or_default();
    control_listeners.fds().into_iter().chain(control.map(|c| c.link.as_raw_fd())).chain(icmp).collect()
}

/// Fork the control worker. It keeps the control listeners and its end of
/// a fresh socketpair, and closes the data-plane sockets.
fn spawn_control(
    cfg: &Config,
    listeners: &ControlListeners,
    bound: Option<&BoundListeners>,
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
            close_fds(&bound.map(BoundListeners::fds).unwrap_or_default());
            close_fds(&[link.as_raw_fd()]);
            become_account(accounts.map(|a| &a.control), "control");
            let inherited = match (
                listeners.socket.try_clone(),
                listeners.remote.as_ref().map(|r| r.try_clone()).transpose(),
                listeners.metrics.try_clone(),
                bound.map(BoundListeners::icmp).transpose(),
            ) {
                (Ok(control_socket), Ok(remote), Ok(metrics), Ok(icmp)) => control_worker::Inherited {
                    control_socket,
                    remote,
                    metrics,
                    master: child_end,
                    icmp: icmp.unwrap_or_default(),
                },
                (Err(e), _, _, _) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
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
    stripe: usize,
    successor: bool,
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
            proxy::run(cfg, index, stripe, successor, sockets)
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
        signal::sigaction(Signal::SIGUSR2, &SigAction::new(SigHandler::Handler(handle_upgrade), SaFlags::SA_RESTART, SigSet::empty()))?;
        // A write to a control worker that just died must not kill the master.
        signal::sigaction(Signal::SIGPIPE, &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{group_id, user_of};

    #[test]
    fn users_and_groups_by_name_or_number() {
        assert_eq!(user_of("0").map(|a| (a.uid.as_raw(), a.gid.as_raw())), Some((0, 0)));
        let unknown = user_of("424242").expect("a number is a user");
        assert_eq!((unknown.name.as_str(), unknown.uid.as_raw(), unknown.gid.as_raw()), ("424242", 424242, 424242));
        assert!(user_of("keel-no-such-user").is_none());
        assert_eq!(group_id("424242").map(|g| g.as_raw()), Some(424242));
        assert!(group_id("keel-no-such-group").is_none());
    }

    #[test]
    fn quick_deaths_back_off_and_a_long_run_resets() {
        let mut b = super::Backoff::default();
        let quick = std::time::Duration::from_millis(50);
        let delays: Vec<u64> = (0..7).map(|_| b.after_death(quick).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(b.after_death(super::STABLE), std::time::Duration::ZERO, "ran a while: at once");
        assert_eq!(b.after_death(quick).as_secs(), 1, "and the next quick death starts over");
    }
}
