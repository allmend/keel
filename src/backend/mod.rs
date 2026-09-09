use anyhow::{Context, Result};
use pingora::lb::{
    selection::{Consistent, RoundRobin, Random},
    Backend, LoadBalancer,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Drain state

pub const DRAIN_ACTIVE: u8 = 0;
pub const DRAIN_DRAINING: u8 = 1;
pub const DRAIN_REMOVED: u8 = 2;

/// Health as seen by the active checker. Keel owns this state rather than
/// Pingora's private health table so that every algorithm (least-connections
/// has no Pingora load balancer), the passive detector, and the CLI all read
/// and write the same thing.
pub struct HealthState {
    /// A health check is configured for this pool; without one the backend
    /// is unconditionally healthy and `reason` stays empty.
    pub checked: AtomicBool,
    pub healthy: AtomicBool,
    consecutive_ok: AtomicU32,
    consecutive_fail: AtomicU32,
    /// Why the last probe failed (kept while healthy too, until the next
    /// success clears it), for `keel status`.
    reason: Mutex<Option<String>>,
    /// Passive detection: consecutive real-traffic failures since the last
    /// success, and the ejection deadline in ms since the registry epoch
    /// (0 = not ejected).
    passive_failures: AtomicU32,
    ejected_until_ms: AtomicU64,
    eject_reason: Mutex<Option<String>>,
}

impl HealthState {
    fn new() -> Self {
        HealthState {
            checked: AtomicBool::new(false),
            healthy: AtomicBool::new(true),
            consecutive_ok: AtomicU32::new(0),
            consecutive_fail: AtomicU32::new(0),
            reason: Mutex::new(None),
            passive_failures: AtomicU32::new(0),
            ejected_until_ms: AtomicU64::new(0),
            eject_reason: Mutex::new(None),
        }
    }

    pub fn reason(&self) -> Option<String> {
        self.reason.lock().unwrap().clone()
    }

    fn ejected_at(&self, now_ms: u64) -> bool {
        let until = self.ejected_until_ms.load(Ordering::Relaxed);
        until != 0 && now_ms < until
    }

    /// Apply one probe result with the pool's flip thresholds. Returns
    /// `Some(new_state)` when the backend flipped.
    pub fn observe(&self, ok: bool, reason: Option<&str>, healthy_after: u32, unhealthy_after: u32) -> Option<bool> {
        self.checked.store(true, Ordering::Relaxed);
        if ok {
            self.consecutive_fail.store(0, Ordering::Relaxed);
            *self.reason.lock().unwrap() = None;
            let n = self.consecutive_ok.fetch_add(1, Ordering::Relaxed) + 1;
            if !self.healthy.load(Ordering::Relaxed) && n >= healthy_after.max(1) {
                self.healthy.store(true, Ordering::Release);
                return Some(true);
            }
        } else {
            self.consecutive_ok.store(0, Ordering::Relaxed);
            *self.reason.lock().unwrap() = reason.map(str::to_owned);
            let n = self.consecutive_fail.fetch_add(1, Ordering::Relaxed) + 1;
            if self.healthy.load(Ordering::Relaxed) && n >= unhealthy_after.max(1) {
                self.healthy.store(false, Ordering::Release);
                return Some(false);
            }
        }
        None
    }
}

/// Per-backend runtime state shared across all algorithm variants.
pub struct BackendEntry {
    pub drain_state: AtomicU8,
    pub connections: AtomicI64,
    pub health: HealthState,
}

impl BackendEntry {
    fn new() -> Self {
        BackendEntry {
            drain_state: AtomicU8::new(DRAIN_ACTIVE),
            connections: AtomicI64::new(0),
            health: HealthState::new(),
        }
    }

    /// Eligible for new traffic: not draining, not failing its check, and
    /// not passively ejected.
    fn available(&self, now_ms: u64) -> bool {
        self.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE
            && self.health.healthy.load(Ordering::Relaxed)
            && !self.health.ejected_at(now_ms)
    }
}

/// Passive (outlier) detection settings for one pool.
#[derive(Clone, Copy)]
pub struct PassiveRule {
    /// Consecutive traffic failures that eject a backend.
    pub failures: u32,
    pub eject_for: Duration,
}

/// Snapshot of a backend's runtime state for status reporting.
pub struct BackendStatus {
    pub pool: String,
    pub address: String,
    pub drain_state: u8,
    pub connections: i64,
    /// `None` when no health check is configured for the pool.
    pub healthy: Option<bool>,
    pub health_reason: Option<String>,
    /// Passively ejected right now; the reason says what traffic failed.
    pub ejected: bool,
    pub eject_reason: Option<String>,
}

// Pool variants

pub enum Pool {
    RoundRobin(Arc<LoadBalancer<RoundRobin>>),
    Random(Arc<LoadBalancer<Random>>),
    ConsistentHash(Arc<LoadBalancer<Consistent>>),
    LeastConn(Arc<LeastConnPool>),
}

// Pool registry

/// Manages all configured backend pools, their selection algorithms, and per-backend
/// drain state / connection counters.
pub struct PoolRegistry {
    pools: HashMap<String, Pool>,
    /// Flat map of all backend addresses across all pools.
    /// Key: "pool_name/addr:port" so the same IP can appear in multiple pools.
    drain: HashMap<String, BackendEntry>,
    /// Pools with passive detection enabled.
    passive: HashMap<String, PassiveRule>,
    /// Reference point for ejection deadlines (ms since here fit an atomic).
    epoch: Instant,
}

impl PoolRegistry {
    /// Construct from pre-built pools (see proxy::build_pools).
    pub fn new(
        pools: HashMap<String, Pool>,
        drain: HashMap<String, BackendEntry>,
        passive: HashMap<String, PassiveRule>,
    ) -> Self {
        PoolRegistry { pools, drain, passive, epoch: Instant::now() }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Select a backend from the named pool. Skips draining/removed backends.
    /// `key` is used for consistent hashing; ignored by round-robin and random.
    pub fn select(&self, pool_name: &str, key: &[u8]) -> Option<SocketAddr> {
        let pool = self.pools.get(pool_name)?;
        let drain = &self.drain;
        let now = self.now_ms();

        match pool {
            // Pingora's own `healthy` flag is always true here: no Pingora
            // health check is attached, Keel keeps health in `drain`.
            Pool::RoundRobin(lb) => {
                lb.select_with(key, 256, |b, _| is_available(drain, pool_name, &b.addr.to_string(), now))
                    .and_then(|b| track_and_return(drain, pool_name, b))
            }
            Pool::Random(lb) => {
                lb.select_with(key, 256, |b, _| is_available(drain, pool_name, &b.addr.to_string(), now))
                    .and_then(|b| track_and_return(drain, pool_name, b))
            }
            Pool::ConsistentHash(lb) => {
                lb.select_with(key, 256, |b, _| is_available(drain, pool_name, &b.addr.to_string(), now))
                    .and_then(|b| track_and_return(drain, pool_name, b))
            }
            Pool::LeastConn(lc) => lc.select(pool_name, drain, now),
        }
    }

    /// Passive detection: a real request, connection, or flow to `addr`
    /// failed before the backend answered. After `failures` consecutive
    /// failures the backend is ejected for `eject_for` — unless it is the
    /// last available backend of the pool, which is never ejected.
    pub fn report_failure(&self, pool_name: &str, addr: SocketAddr, what: &str) {
        let Some(rule) = self.passive.get(pool_name) else { return };
        let key = drain_key(pool_name, &addr.to_string());
        let Some(entry) = self.drain.get(&key) else { return };
        let now = self.now_ms();
        if entry.health.ejected_at(now) {
            return;
        }
        let n = entry.health.passive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n < rule.failures {
            return;
        }
        entry.health.passive_failures.store(0, Ordering::Relaxed);
        if self.available_count(pool_name, now) <= 1 {
            tracing::debug!(pool = pool_name, backend = %addr, "passive: last available backend, not ejecting");
            return;
        }
        let until = now + rule.eject_for.as_millis() as u64;
        entry.health.ejected_until_ms.store(until.max(1), Ordering::Release);
        let reason = format!("{n} consecutive {what} failures");
        *entry.health.eject_reason.lock().unwrap() = Some(reason.clone());
        crate::metrics::record_ejection(pool_name, &addr.to_string(), true);
        tracing::warn!(
            pool = pool_name,
            backend = %addr,
            reason,
            eject_ms = rule.eject_for.as_millis() as u64,
            "passive: backend ejected"
        );
    }

    /// Passive detection: traffic to `addr` succeeded; the failure streak ends.
    pub fn report_success(&self, pool_name: &str, addr: SocketAddr) {
        if !self.passive.contains_key(pool_name) {
            return;
        }
        let key = drain_key(pool_name, &addr.to_string());
        if let Some(entry) = self.drain.get(&key) {
            if entry.health.passive_failures.load(Ordering::Relaxed) != 0 {
                entry.health.passive_failures.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Clear expired ejections (selection already ignores them; this keeps
    /// the log and the gauge honest). Returns the number re-admitted.
    pub fn sweep_ejections(&self) -> usize {
        let now = self.now_ms();
        let mut readmitted = 0;
        for (key, entry) in &self.drain {
            let until = entry.health.ejected_until_ms.load(Ordering::Relaxed);
            if until != 0 && now >= until {
                entry.health.ejected_until_ms.store(0, Ordering::Release);
                *entry.health.eject_reason.lock().unwrap() = None;
                if let Some((pool, addr)) = key.split_once('/') {
                    crate::metrics::record_ejection(pool, addr, false);
                    tracing::info!(pool, backend = addr, "passive: ejection expired, backend re-admitted");
                }
                readmitted += 1;
            }
        }
        readmitted
    }

    fn available_count(&self, pool_name: &str, now_ms: u64) -> usize {
        let prefix = format!("{pool_name}/");
        self.drain
            .iter()
            .filter(|(k, e)| k.starts_with(&prefix) && e.available(now_ms))
            .count()
    }

    /// Decrement the connection counter for `addr` in `pool_name`.
    /// If the backend is draining and this was the last connection, marks it removed.
    pub fn release(&self, pool_name: &str, addr: SocketAddr) {
        let key = drain_key(pool_name, &addr.to_string());
        if let Some(entry) = self.drain.get(&key) {
            let prev = entry.connections.fetch_sub(1, Ordering::Relaxed);
            crate::metrics::set_active_connections(
                pool_name,
                &addr.to_string(),
                (prev - 1).max(0) as f64,
            );
            // Auto-remove when last draining connection finishes
            if entry.drain_state.load(Ordering::Acquire) == DRAIN_DRAINING && prev <= 1 {
                entry.drain_state.store(DRAIN_REMOVED, Ordering::Release);
                crate::metrics::set_drain_state(pool_name, &addr.to_string(), DRAIN_REMOVED);
                tracing::info!(pool = pool_name, backend = %addr, "drain complete: backend removed");
            }
        }
    }

    /// Initiate drain for a backend. Returns false if the backend was not found.
    #[allow(dead_code)]
    pub fn drain_backend(&self, pool_name: &str, addr: SocketAddr) -> bool {
        let key = drain_key(pool_name, &addr.to_string());
        if let Some(entry) = self.drain.get(&key) {
            entry.drain_state.store(DRAIN_DRAINING, Ordering::Release);
            crate::metrics::set_drain_state(pool_name, &addr.to_string(), DRAIN_DRAINING);
            tracing::info!(pool = pool_name, backend = %addr, "drain initiated");
            true
        } else {
            false
        }
    }

    /// Re-activate a backend (re-add after drain).
    #[allow(dead_code)]
    pub fn activate_backend(&self, pool_name: &str, addr: SocketAddr) -> bool {
        let key = drain_key(pool_name, &addr.to_string());
        if let Some(entry) = self.drain.get(&key) {
            entry.drain_state.store(DRAIN_ACTIVE, Ordering::Release);
            crate::metrics::set_drain_state(pool_name, &addr.to_string(), DRAIN_ACTIVE);
            true
        } else {
            false
        }
    }

    /// Record an active-check result. Flips the backend when the pool's
    /// threshold is reached; logs and updates `keel_backend_healthy` on flips.
    pub fn observe_health(
        &self,
        pool_name: &str,
        addr: SocketAddr,
        ok: bool,
        reason: Option<&str>,
        healthy_after: u32,
        unhealthy_after: u32,
    ) {
        let key = drain_key(pool_name, &addr.to_string());
        let Some(entry) = self.drain.get(&key) else { return };
        match entry.health.observe(ok, reason, healthy_after, unhealthy_after) {
            Some(true) => {
                crate::metrics::set_backend_healthy(pool_name, &addr.to_string(), true);
                tracing::info!(pool = pool_name, backend = %addr, "health: backend healthy");
            }
            Some(false) => {
                crate::metrics::set_backend_healthy(pool_name, &addr.to_string(), false);
                tracing::warn!(
                    pool = pool_name,
                    backend = %addr,
                    reason = reason.unwrap_or("unknown"),
                    "health: backend unhealthy"
                );
            }
            None => {}
        }
    }

    /// Mark every backend of a pool as covered by an active check, so status
    /// output distinguishes "healthy" from "not checked".
    pub fn mark_checked(&self, pool_name: &str) {
        let prefix = format!("{pool_name}/");
        for (key, entry) in &self.drain {
            if key.starts_with(&prefix) {
                entry.health.checked.store(true, Ordering::Relaxed);
                let addr = key.trim_start_matches(&prefix);
                crate::metrics::set_backend_healthy(pool_name, addr, true);
            }
        }
    }

    /// Current active connections for a backend (used for status reporting).
    #[allow(dead_code)]
    pub fn connections(&self, pool_name: &str, addr: SocketAddr) -> i64 {
        let key = drain_key(pool_name, &addr.to_string());
        self.drain
            .get(&key)
            .map(|e| e.connections.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Returns true if the named pool exists.
    pub fn has_pool(&self, pool_name: &str) -> bool {
        self.pools.contains_key(pool_name)
    }

    /// All backends across all pools, sorted by pool then address.
    pub fn all_backends(&self) -> Vec<BackendStatus> {
        let mut result: Vec<BackendStatus> = self.drain.iter()
            .filter_map(|(key, entry)| {
                let (pool, addr) = key.split_once('/')?;
                Some(status_of(pool, addr, entry, self.now_ms()))
            })
            .collect();
        result.sort_by(|a, b| a.pool.cmp(&b.pool).then(a.address.cmp(&b.address)));
        result
    }

    /// All backends in a specific pool, sorted by address.
    pub fn backends_for_pool(&self, pool_name: &str) -> Vec<BackendStatus> {
        let prefix = format!("{pool_name}/");
        let mut result: Vec<BackendStatus> = self.drain.iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, entry)| {
                let addr = key.trim_start_matches(&prefix);
                status_of(pool_name, addr, entry, self.now_ms())
            })
            .collect();
        result.sort_by(|a, b| a.address.cmp(&b.address));
        result
    }

    /// Initiate drain for all pools containing `addr`. Returns the pool names where found.
    pub fn drain_by_address(&self, addr: &str) -> Vec<String> {
        let suffix = format!("/{addr}");
        let mut found = Vec::new();
        for (key, entry) in &self.drain {
            if key.ends_with(&suffix) {
                let pool = key.trim_end_matches(&suffix);
                entry.drain_state.store(DRAIN_DRAINING, Ordering::Release);
                crate::metrics::set_drain_state(pool, addr, DRAIN_DRAINING);
                tracing::info!(pool, backend = addr, "control: drain initiated");
                found.push(pool.to_owned());
            }
        }
        found.sort();
        found
    }

    /// Total active connections across all pools for a given backend address.
    pub fn connections_for_address(&self, addr: &str) -> i64 {
        let suffix = format!("/{addr}");
        self.drain.iter()
            .filter(|(key, _)| key.ends_with(&suffix))
            .map(|(_, e)| e.connections.load(Ordering::Relaxed).max(0))
            .sum()
    }

    /// Sync drain state after a config reload.
    ///
    /// Backends present in the drain table but absent from `cfg` are moved to
    /// DRAINING so no new connections are sent to them. New backends and
    /// algorithm/weight changes require a restart and are logged accordingly.
    pub fn sync_from_config(&self, cfg: &crate::config::Config) {
        use std::collections::HashSet;

        for (pool_name, pool_cfg) in &cfg.pools {
            let prefix = format!("{pool_name}/");

            let new_keys: HashSet<String> = pool_cfg
                .backends
                .iter()
                .map(|b| drain_key(pool_name, &b.address))
                .collect();

            // Drain backends removed from config.
            for (key, entry) in &self.drain {
                if !key.starts_with(&prefix) {
                    continue;
                }
                if !new_keys.contains(key) {
                    if entry.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE {
                        entry.drain_state.store(DRAIN_DRAINING, Ordering::Release);
                        let addr = key.trim_start_matches(&prefix);
                        crate::metrics::set_drain_state(pool_name, addr, DRAIN_DRAINING);
                        tracing::info!(
                            pool = pool_name,
                            backend = addr,
                            "hot reload: backend removed from config, draining"
                        );
                    }
                }
            }

            // Warn about additions that need a restart.
            let existing_keys: HashSet<&String> = self.drain
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .collect();

            for key in &new_keys {
                if !existing_keys.contains(key) {
                    let addr = key.trim_start_matches(&prefix);
                    tracing::warn!(
                        pool = pool_name,
                        backend = addr,
                        "hot reload: new backend requires restart to take effect"
                    );
                }
            }
        }
    }
}

// Helpers

pub fn drain_key(pool: &str, addr: &str) -> String {
    format!("{pool}/{addr}")
}

fn is_available(drain: &HashMap<String, BackendEntry>, pool: &str, addr: &str, now_ms: u64) -> bool {
    let key = drain_key(pool, addr);
    drain
        .get(&key)
        .map(|e| e.available(now_ms))
        .unwrap_or(true) // unknown backends are treated as available
}

fn status_of(pool: &str, addr: &str, entry: &BackendEntry, now_ms: u64) -> BackendStatus {
    let checked = entry.health.checked.load(Ordering::Relaxed);
    let ejected = entry.health.ejected_at(now_ms);
    BackendStatus {
        pool: pool.to_owned(),
        address: addr.to_owned(),
        drain_state: entry.drain_state.load(Ordering::Relaxed),
        connections: entry.connections.load(Ordering::Relaxed).max(0),
        healthy: checked.then(|| entry.health.healthy.load(Ordering::Relaxed)),
        health_reason: if checked { entry.health.reason() } else { None },
        ejected,
        eject_reason: if ejected { entry.health.eject_reason.lock().unwrap().clone() } else { None },
    }
}

fn track_and_return(
    drain: &HashMap<String, BackendEntry>,
    pool: &str,
    b: Backend,
) -> Option<SocketAddr> {
    let addr = b.addr.as_inet().copied()?;
    let key = drain_key(pool, &addr.to_string());
    if let Some(entry) = drain.get(&key) {
        let count = entry.connections.fetch_add(1, Ordering::Relaxed) + 1;
        crate::metrics::set_active_connections(pool, &addr.to_string(), count as f64);
    }
    Some(addr)
}

// Least connections

pub struct LeastConnEntry {
    pub addr: SocketAddr,
}

pub struct LeastConnPool {
    backends: Vec<LeastConnEntry>,
}

impl LeastConnPool {
    pub fn build(addrs: &[&str]) -> Result<Self> {
        let backends = addrs
            .iter()
            .map(|a| {
                let addr: SocketAddr = a.parse().with_context(|| format!("invalid address: {a}"))?;
                Ok(LeastConnEntry { addr })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(LeastConnPool { backends })
    }

    fn select(&self, pool_name: &str, drain: &HashMap<String, BackendEntry>, now_ms: u64) -> Option<SocketAddr> {
        self.backends
            .iter()
            .filter(|e| is_available(drain, pool_name, &e.addr.to_string(), now_ms))
            .min_by_key(|e| {
                let key = drain_key(pool_name, &e.addr.to_string());
                drain
                    .get(&key)
                    .map(|d| d.connections.load(Ordering::Relaxed))
                    .unwrap_or(0)
            })
            .map(|e| {
                let key = drain_key(pool_name, &e.addr.to_string());
                if let Some(entry) = drain.get(&key) {
                    let count = entry.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    crate::metrics::set_active_connections(pool_name, &e.addr.to_string(), count as f64);
                }
                e.addr
            })
    }
}

// Load balancer builder

/// Build a Pingora LoadBalancer from backend addresses (unweighted; weights are
/// handled by Pingora internally when using `Backend::new_with_weight`).
pub fn build_lb<S>(addrs: &[&str], weights: &[usize]) -> Result<LoadBalancer<S>>
where
    S: pingora::lb::selection::BackendSelection + Send + Sync + 'static,
    S::Iter: pingora::lb::selection::BackendIter,
{
    let backends: Vec<Backend> = addrs
        .iter()
        .zip(weights.iter())
        .map(|(addr, &weight)| {
            Backend::new_with_weight(addr, weight).map_err(|e| anyhow::anyhow!("{e}"))
        })
        .collect::<Result<_>>()?;

    LoadBalancer::try_from_iter(backends.iter().map(|b| b.addr.to_string()))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Build the drain table from a pool's backends.
pub fn build_drain_entries(
    pool_name: &str,
    addrs: &[&str],
    drain: &mut HashMap<String, BackendEntry>,
) {
    for addr in addrs {
        let key = drain_key(pool_name, addr);
        drain.entry(key).or_insert_with(BackendEntry::new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(eject_for: Duration) -> PoolRegistry {
        let addrs = ["127.0.0.1:1", "127.0.0.1:2"];
        let mut drain = HashMap::new();
        build_drain_entries("p", &addrs, &mut drain);
        let mut pools = HashMap::new();
        pools.insert("p".to_owned(), Pool::LeastConn(Arc::new(LeastConnPool::build(&addrs).unwrap())));
        let mut passive = HashMap::new();
        passive.insert("p".to_owned(), PassiveRule { failures: 3, eject_for });
        PoolRegistry::new(pools, drain, passive)
    }

    /// Distinct backends chosen over `n` selections held open together, so
    /// least-connections spreads across every available backend.
    fn selections(reg: &PoolRegistry, n: usize) -> std::collections::HashSet<SocketAddr> {
        let picked: Vec<SocketAddr> = (0..n).map(|_| reg.select("p", b"").expect("a backend")).collect();
        for a in &picked {
            reg.release("p", *a);
        }
        picked.into_iter().collect()
    }

    #[test]
    fn active_check_flips_on_thresholds() {
        let reg = registry(Duration::from_secs(60));
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        reg.mark_checked("p");
        reg.observe_health("p", a, false, Some("refused"), 2, 3);
        reg.observe_health("p", a, false, Some("refused"), 2, 3);
        assert!(reg.backends_for_pool("p")[0].healthy.unwrap(), "below threshold");
        reg.observe_health("p", a, false, Some("refused"), 2, 3);
        let st = &reg.backends_for_pool("p")[0];
        assert_eq!(st.healthy, Some(false));
        assert_eq!(st.health_reason.as_deref(), Some("refused"));
        assert_eq!(selections(&reg, 6).len(), 1, "only :2 is selectable");
        reg.observe_health("p", a, true, None, 2, 3);
        assert_eq!(reg.backends_for_pool("p")[0].healthy, Some(false));
        reg.observe_health("p", a, true, None, 2, 3);
        assert_eq!(reg.backends_for_pool("p")[0].healthy, Some(true));
        assert_eq!(reg.backends_for_pool("p")[0].health_reason, None);
    }

    #[test]
    fn passive_ejects_after_failures_but_never_the_last_backend() {
        let reg = registry(Duration::from_secs(60));
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2".parse().unwrap();
        assert_eq!(selections(&reg, 6).len(), 2);

        reg.report_failure("p", a, "connect");
        reg.report_failure("p", a, "connect");
        reg.report_success("p", a); // streak reset
        reg.report_failure("p", a, "connect");
        reg.report_failure("p", a, "connect");
        assert!(!reg.backends_for_pool("p")[0].ejected, "success reset the streak");
        reg.report_failure("p", a, "connect");
        let st = &reg.backends_for_pool("p")[0];
        assert!(st.ejected);
        assert_eq!(st.eject_reason.as_deref(), Some("3 consecutive connect failures"));
        assert_eq!(selections(&reg, 6), [b].into_iter().collect());

        // :2 is now the last available backend and must survive its failures.
        for _ in 0..5 {
            reg.report_failure("p", b, "connect");
        }
        assert!(!reg.backends_for_pool("p")[1].ejected);
        assert_eq!(selections(&reg, 3), [b].into_iter().collect());
    }

    #[test]
    fn ejection_expires() {
        let reg = registry(Duration::from_millis(20));
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        for _ in 0..3 {
            reg.report_failure("p", a, "connect");
        }
        assert!(reg.backends_for_pool("p")[0].ejected);
        assert_eq!(reg.sweep_ejections(), 0);
        std::thread::sleep(Duration::from_millis(30));
        assert!(!reg.backends_for_pool("p")[0].ejected, "selection ignores an expired ejection");
        assert_eq!(reg.sweep_ejections(), 1);
        assert_eq!(reg.sweep_ejections(), 0);
        assert_eq!(selections(&reg, 6).len(), 2);
    }
}
