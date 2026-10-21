//! When a worker is done. A worker counts what it still serves (HTTP
//! requests, TCP connections, UDP flows); once Pingora shuts down, the
//! worker exits when that count reaches zero, or at its limit: `grace_period`
//! when Keel stops, `drain_timeout` when a newer generation replaced it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tracing::info;

static OPEN: AtomicUsize = AtomicUsize::new(0);
static DRAINING: AtomicBool = AtomicBool::new(false);

/// One thing the worker serves, counted while it lives.
pub struct Open(());

impl Open {
    pub fn new() -> Self {
        OPEN.fetch_add(1, Ordering::Relaxed);
        Open(())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A newer generation replaced this worker: what is open stays open.
pub fn draining() -> bool {
    DRAINING.load(Ordering::Relaxed)
}

/// What the worker still serves.
pub fn open() -> usize {
    OPEN.load(Ordering::Relaxed)
}

/// SIGUSR1 from the master: a newer generation took over; stop accepting
/// and finish what is open, up to `drain_timeout`.
pub struct Watcher {
    pub grace_period: Duration,
    pub drain_timeout: Duration,
}

#[async_trait]
impl BackgroundService for Watcher {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut usr1) = signal(SignalKind::user_defined1()) else { return };
        loop {
            tokio::select! {
                _ = usr1.recv() => {
                    DRAINING.store(true, Ordering::Relaxed);
                    info!(open = open(), "worker: replaced, draining");
                    // Pingora's graceful shutdown: stop accepting, keep serving.
                    // SAFETY: signalling our own process.
                    unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
            }
        }
        let limit = if DRAINING.load(Ordering::Relaxed) { self.drain_timeout } else { self.grace_period };
        let started = Instant::now();
        while open() > 0 && started.elapsed() < limit {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        info!(open = open(), drained = open() == 0, "worker: exiting");
        std::process::exit(0);
    }
}
