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

/// Drop root privileges to `user`:`group`. Logs and continues on failure
/// (e.g. when running as non-root in dev).
fn drop_privileges(user: &str, group: &str) {
    use nix::unistd::{setgid, setuid, Group, User};

    if let Ok(Some(g)) = Group::from_name(group) {
        if let Err(e) = setgid(g.gid) {
            warn!(group, error = %e, "worker: could not setgid (running as non-root?)");
        }
    } else {
        warn!(group, "worker: group not found, skipping setgid");
    }

    if let Ok(Some(u)) = User::from_name(user) {
        if let Err(e) = setuid(u.uid) {
            warn!(user, error = %e, "worker: could not setuid (running as non-root?)");
        }
    } else {
        warn!(user, "worker: user not found, skipping setuid");
    }
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
