use crate::{config::Config, proxy};
use anyhow::Result;
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info, warn};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigquit(_: libc::c_int) {
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

    let n = cfg.keel.workers;
    info!(workers = n, "master: spawning workers");

    let mut pids: Vec<Pid> = Vec::with_capacity(n);
    for i in 0..n {
        let pid = spawn_worker(&cfg, i)?;
        info!(pid = pid.as_raw(), index = i, "master: worker started");
        pids.push(pid);
    }

    // Supervision loop
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            info!("master: shutdown signal received, stopping workers");
            for pid in &pids {
                let _ = signal::kill(*pid, Signal::SIGQUIT);
            }
            for pid in &pids {
                let _ = waitpid(*pid, None);
            }
            info!("master: all workers stopped, exiting");
            return Ok(());
        }

        if RELOAD.swap(false, Ordering::SeqCst) {
            info!("master: reload signal received (SIGHUP), forwarding to workers");
            for pid in &pids {
                let _ = signal::kill(*pid, Signal::SIGHUP);
            }
        }

        // Collect dead workers and restart them
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(dead, code)) => {
                    warn!(pid = dead.as_raw(), exit_code = code, "master: worker died, restarting");
                    pids.retain(|p| *p != dead);
                    let pid = spawn_worker(&cfg, pids.len())?;
                    info!(pid = pid.as_raw(), "master: replacement worker started");
                    pids.push(pid);
                }
                Ok(WaitStatus::Signaled(dead, sig, _)) => {
                    warn!(pid = dead.as_raw(), signal = ?sig, "master: worker killed, restarting");
                    pids.retain(|p| *p != dead);
                    let pid = spawn_worker(&cfg, pids.len())?;
                    info!(pid = pid.as_raw(), "master: replacement worker started");
                    pids.push(pid);
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

/// Fork a worker. Returns the child PID in the master. The child never returns.
fn spawn_worker(cfg: &Config, index: usize) -> Result<Pid> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            drop_privileges(&cfg.keel.user, &cfg.keel.group);
            run_worker(cfg, index)
        }
    }
}

/// Worker entry point — starts the Pingora data plane. Never returns.
fn run_worker(cfg: &Config, index: usize) -> ! {
    info!(index, "worker: started");
    proxy::run(cfg)
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
    let sigquit = SigAction::new(SigHandler::Handler(handle_sigquit), SaFlags::SA_RESTART, SigSet::empty());
    let sighup = SigAction::new(SigHandler::Handler(handle_sighup), SaFlags::SA_RESTART, SigSet::empty());
    unsafe {
        signal::sigaction(Signal::SIGQUIT, &sigquit)?;
        signal::sigaction(Signal::SIGHUP, &sighup)?;
        // Ignore SIGPIPE — broken pipe on a client must not kill the master
        signal::sigaction(
            Signal::SIGPIPE,
            &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()),
        )?;
    }
    Ok(())
}
