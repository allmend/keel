//! `keel upgrade`: the master executes the binary on disk in place. Same
//! PID, so supervisors (systemd, container init) see nothing change, and the
//! workers stay its children. What the master holds crosses the exec in the
//! environment: its sockets (the descriptors stay open), the metrics region
//! and its workers. The new image adopts them, starts a control worker and
//! new workers, and once those are ready the old workers drain.
//!
//! An exec that succeeds cannot be taken back, so the new binary is tried
//! first: it must run and accept the node's config.

use std::os::fd::RawFd;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const ENV: &str = "KEEL_UPGRADE";

/// What a master hands its re-executed self.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Handover {
    pub tcp: Vec<(String, RawFd)>,
    pub udp: Vec<(String, Vec<RawFd>)>,
    pub icmp4: Option<RawFd>,
    pub icmp6: Option<RawFd>,
    pub control_socket: RawFd,
    pub control_socket_file: Option<(u64, u64)>,
    pub control_remote: Option<RawFd>,
    pub metrics: RawFd,
    pub region: Option<RawFd>,
    /// Running workers by generation, the current one first.
    pub generations: Vec<Handed>,
}

/// One generation of workers crossing the exec.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Handed {
    pub slot: usize,
    /// Replaced already, finishing its connections.
    pub draining: bool,
    /// (pid, index)
    pub workers: Vec<(i32, usize)>,
}

impl Handover {
    fn fds(&self) -> Vec<RawFd> {
        let tcp = self.tcp.iter().map(|(_, fd)| *fd);
        let udp = self.udp.iter().flat_map(|(_, fds)| fds.iter().copied());
        let rest = [self.icmp4, self.icmp6, Some(self.control_socket), self.control_remote, Some(self.metrics), self.region];
        tcp.chain(udp).chain(rest.into_iter().flatten()).collect()
    }
}

/// The handover left by the previous image, if this one is an upgrade.
pub fn take() -> Option<Handover> {
    let text = std::env::var(ENV).ok()?;
    // SAFETY: single-threaded at this point (the master starts no threads).
    unsafe { std::env::remove_var(ENV) };
    match serde_json::from_str(&text) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!(error = %e, "master: unreadable upgrade handover; starting fresh");
            None
        }
    }
}

/// Try the new binary before the exec that cannot be taken back.
pub fn preflight(exe: &Path, node_file: &str) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new(exe).args(args).env_remove(ENV).output().with_context(|| format!("cannot run {}", exe.display()))?;
        if !out.status.success() {
            anyhow::bail!("{} {} failed: {}", exe.display(), args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    run(&["--version"])?;
    run(&["--config", node_file, "config", "validate"])?;
    Ok(())
}

/// Replace this process's image with `exe`, handing `handover` over. Only
/// returns when the exec failed; then nothing has changed.
pub fn exec(exe: &Path, handover: &Handover) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    for fd in handover.fds() {
        // SAFETY: clearing FD_CLOEXEC on descriptors this process owns.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    let text = match serde_json::to_string(handover) {
        Ok(t) => t,
        Err(e) => return e.into(),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let err = std::process::Command::new(exe).args(args).env(ENV, text).exec();
    anyhow::Error::new(err).context(format!("cannot execute {}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handover_survives_the_environment() {
        let h = Handover {
            tcp: vec![("0.0.0.0:80".into(), 7)],
            udp: vec![("0.0.0.0:53".into(), vec![8, 9])],
            icmp4: Some(10),
            control_socket: 11,
            control_socket_file: Some((1, 2)),
            metrics: 12,
            region: Some(13),
            generations: vec![Handed { slot: 0, draining: false, workers: vec![(100, 0), (101, 1)] }],
            ..Default::default()
        };
        let back: Handover = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
        assert_eq!(h.fds(), [7, 8, 9, 10, 11, 12, 13]);
    }
}
