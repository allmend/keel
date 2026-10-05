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
    /// The address as written in `keel.yaml`, which is what a later reload is
    /// compared against. The map is keyed by the resolved address, so without
    /// this a hostname-configured backend could not be matched back to its
    /// config entry without resolving it again.
    pub config_address: String,
    /// The resolved address as text, for metric labels and status output,
    /// formatted once instead of on every request.
    label: String,
    /// `keel_active_connections` for this backend, bound once.
    active_gauge: prometheus::Gauge,
}

impl BackendEntry {
    fn new(pool: &str, addr: SocketAddr, config_address: &str) -> Self {
        let label = addr.to_string();
        BackendEntry {
            drain_state: AtomicU8::new(DRAIN_ACTIVE),
            connections: AtomicI64::new(0),
            health: HealthState::new(),
            config_address: config_address.to_owned(),
            active_gauge: crate::metrics::active_connections_gauge(pool, &label),
            label,
        }
    }

    /// One more connection to this backend.
    fn acquire(&self) {
        let count = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
        self.active_gauge.set(count as f64);
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

/// Drain state and counters per pool, per resolved backend address. Keyed by
/// pool first so the same address can appear in several pools, and so the
/// per-request lookups take the address as it is, without formatting it.
pub type DrainTable = HashMap<String, HashMap<SocketAddr, BackendEntry>>;

/// Manages all configured backend pools, their selection algorithms, and per-backend
/// drain state / connection counters.
pub struct PoolRegistry {
    pools: HashMap<String, Pool>,
    drain: DrainTable,
    /// Pools with passive detection enabled.
    passive: HashMap<String, PassiveRule>,
    /// Reference point for ejection deadlines (ms since here fit an atomic).
    epoch: Instant,
}

impl PoolRegistry {
    /// Construct from pre-built pools (see proxy::build_pools).
    pub fn new(
        pools: HashMap<String, Pool>,
        drain: DrainTable,
        passive: HashMap<String, PassiveRule>,
    ) -> Self {
        PoolRegistry { pools, drain, passive, epoch: Instant::now() }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn entry(&self, pool_name: &str, addr: SocketAddr) -> Option<&BackendEntry> {
        self.drain.get(pool_name)?.get(&addr)
    }

    /// Every backend entry with the pool it belongs to.
    fn entries(&self) -> impl Iterator<Item = (&str, &BackendEntry)> {
        self.drain
            .iter()
            .flat_map(|(pool, entries)| entries.values().map(move |e| (pool.as_str(), e)))
    }

    /// Select a backend from the named pool. Skips draining/removed backends.
    /// `key` is used for consistent hashing; ignored by round-robin and random.
    pub fn select(&self, pool_name: &str, key: &[u8]) -> Option<SocketAddr> {
        let pool = self.pools.get(pool_name)?;
        let empty = HashMap::new();
        let entries = self.drain.get(pool_name).unwrap_or(&empty);
        let now = self.now_ms();

        // Pingora's own `healthy` flag is always true here: no Pingora
        // health check is attached, Keel keeps health in `drain`.
        let available = |b: &Backend, _: bool| {
            b.addr.as_inet().is_some_and(|a| is_available(entries, a, now))
        };
        let picked = match pool {
            Pool::RoundRobin(lb) => lb.select_with(key, 256, available),
            Pool::Random(lb) => lb.select_with(key, 256, available),
            Pool::ConsistentHash(lb) => lb.select_with(key, 256, available),
            Pool::LeastConn(lc) => return lc.select(entries, now),
        };
        let addr = *picked?.addr.as_inet()?;
        if let Some(entry) = entries.get(&addr) {
            entry.acquire();
        }
        Some(addr)
    }

    /// Passive detection: a real request, connection, or flow to `addr`
    /// failed before the backend answered. After `failures` consecutive
    /// failures the backend is ejected for `eject_for` — unless it is the
    /// last available backend of the pool, which is never ejected.
    pub fn report_failure(&self, pool_name: &str, addr: SocketAddr, what: &str) {
        let Some(rule) = self.passive.get(pool_name) else { return };
        let Some(entry) = self.entry(pool_name, addr) else { return };
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
        crate::metrics::record_ejection(pool_name, &entry.label, true);
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
        if let Some(entry) = self.entry(pool_name, addr) {
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
        for (pool, entry) in self.entries() {
            let until = entry.health.ejected_until_ms.load(Ordering::Relaxed);
            if until != 0 && now >= until {
                entry.health.ejected_until_ms.store(0, Ordering::Release);
                *entry.health.eject_reason.lock().unwrap() = None;
                crate::metrics::record_ejection(pool, &entry.label, false);
                tracing::info!(pool, backend = entry.label, "passive: ejection expired, backend re-admitted");
                readmitted += 1;
            }
        }
        readmitted
    }

    fn available_count(&self, pool_name: &str, now_ms: u64) -> usize {
        self.drain
            .get(pool_name)
            .map_or(0, |entries| entries.values().filter(|e| e.available(now_ms)).count())
    }

    /// Decrement the connection counter for `addr` in `pool_name`.
    /// If the backend is draining and this was the last connection, marks it removed.
    pub fn release(&self, pool_name: &str, addr: SocketAddr) {
        if let Some(entry) = self.entry(pool_name, addr) {
            let prev = entry.connections.fetch_sub(1, Ordering::Relaxed);
            entry.active_gauge.set((prev - 1).max(0) as f64);
            // Auto-remove when last draining connection finishes
            if entry.drain_state.load(Ordering::Acquire) == DRAIN_DRAINING && prev <= 1 {
                entry.drain_state.store(DRAIN_REMOVED, Ordering::Release);
                crate::metrics::set_drain_state(pool_name, &entry.label, DRAIN_REMOVED);
                tracing::info!(pool = pool_name, backend = %addr, "drain complete: backend removed");
            }
        }
    }

    /// Initiate drain for a backend. Returns false if the backend was not found.
    #[allow(dead_code)]
    pub fn drain_backend(&self, pool_name: &str, addr: SocketAddr) -> bool {
        if let Some(entry) = self.entry(pool_name, addr) {
            entry.drain_state.store(DRAIN_DRAINING, Ordering::Release);
            crate::metrics::set_drain_state(pool_name, &entry.label, DRAIN_DRAINING);
            tracing::info!(pool = pool_name, backend = %addr, "drain initiated");
            true
        } else {
            false
        }
    }

    /// Re-activate a backend (re-add after drain).
    #[allow(dead_code)]
    pub fn activate_backend(&self, pool_name: &str, addr: SocketAddr) -> bool {
        if let Some(entry) = self.entry(pool_name, addr) {
            entry.drain_state.store(DRAIN_ACTIVE, Ordering::Release);
            crate::metrics::set_drain_state(pool_name, &entry.label, DRAIN_ACTIVE);
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
        let Some(entry) = self.entry(pool_name, addr) else { return };
        match entry.health.observe(ok, reason, healthy_after, unhealthy_after) {
            Some(true) => {
                crate::metrics::set_backend_healthy(pool_name, &entry.label, true);
                tracing::info!(pool = pool_name, backend = %addr, "health: backend healthy");
            }
            Some(false) => {
                crate::metrics::set_backend_healthy(pool_name, &entry.label, false);
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
        for entry in self.drain.get(pool_name).into_iter().flat_map(HashMap::values) {
            entry.health.checked.store(true, Ordering::Relaxed);
            crate::metrics::set_backend_healthy(pool_name, &entry.label, true);
        }
    }

    /// Current active connections for a backend (used for status reporting).
    #[allow(dead_code)]
    pub fn connections(&self, pool_name: &str, addr: SocketAddr) -> i64 {
        self.entry(pool_name, addr)
            .map(|e| e.connections.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// The pool selects by the key passed to `select` (consistent hashing);
    /// the other algorithms ignore it, so callers can skip building it.
    pub fn uses_key(&self, pool_name: &str) -> bool {
        matches!(self.pools.get(pool_name), Some(Pool::ConsistentHash(_)))
    }

    /// Returns true if the named pool exists.
    pub fn has_pool(&self, pool_name: &str) -> bool {
        self.pools.contains_key(pool_name)
    }

    /// All backends across all pools, sorted by pool then address.
    pub fn all_backends(&self) -> Vec<BackendStatus> {
        let now = self.now_ms();
        let mut result: Vec<BackendStatus> =
            self.entries().map(|(pool, entry)| status_of(pool, entry, now)).collect();
        result.sort_by(|a, b| a.pool.cmp(&b.pool).then(a.address.cmp(&b.address)));
        result
    }

    /// All backends in a specific pool, sorted by address.
    pub fn backends_for_pool(&self, pool_name: &str) -> Vec<BackendStatus> {
        let now = self.now_ms();
        let mut result: Vec<BackendStatus> = self
            .drain
            .get(pool_name)
            .into_iter()
            .flat_map(HashMap::values)
            .map(|entry| status_of(pool_name, entry, now))
            .collect();
        result.sort_by(|a, b| a.address.cmp(&b.address));
        result
    }

    /// Initiate drain for all pools containing `addr`. Returns the pool names where found.
    pub fn drain_by_address(&self, addr: &str) -> Vec<String> {
        let mut found = Vec::new();
        for (pool, entry) in self.entries() {
            if entry.label == addr {
                entry.drain_state.store(DRAIN_DRAINING, Ordering::Release);
                crate::metrics::set_drain_state(pool, addr, DRAIN_DRAINING);
                tracing::info!(pool, backend = addr, "control: drain initiated");
                found.push(pool.to_owned());
            }
        }
        found.sort();
        found
    }

    /// Sync drain state after a config reload.
    ///
    /// Backends present in the drain table but absent from `cfg` are moved to
    /// DRAINING so no new connections are sent to them. New backends and
    /// algorithm/weight changes require a restart and are logged accordingly.
    pub fn sync_from_config(&self, cfg: &crate::config::Config) {
        use std::collections::HashSet;

        for (pool_name, pool_cfg) in &cfg.pools {
            let entries = self.drain.get(pool_name.as_str()).into_iter().flat_map(HashMap::values);

            // Compare on the address as configured, which is what the
            // entry remembers. Resolving again here would put a blocking
            // getaddrinfo on the async reload task, and would also drain a
            // backend whose hostname merely resolved to a different IP than
            // it did at startup.
            let configured: HashSet<&str> =
                pool_cfg.backends.iter().map(|b| b.address.as_str()).collect();

            // Drain backends removed from config.
            for entry in entries.clone() {
                let gone = !configured.contains(entry.config_address.as_str());
                if gone && entry.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE {
                    entry.drain_state.store(DRAIN_DRAINING, Ordering::Release);
                    crate::metrics::set_drain_state(pool_name, &entry.label, DRAIN_DRAINING);
                    tracing::info!(
                        pool = pool_name,
                        backend = entry.label,
                        "hot reload: backend removed from config, draining"
                    );
                }
            }

            // Warn about additions that need a restart.
            let known: HashSet<&str> = entries.map(|e| e.config_address.as_str()).collect();

            for addr in &configured {
                if !known.contains(addr) {
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

fn is_available(entries: &HashMap<SocketAddr, BackendEntry>, addr: &SocketAddr, now_ms: u64) -> bool {
    entries
        .get(addr)
        .map(|e| e.available(now_ms))
        .unwrap_or(true) // unknown backends are treated as available
}

fn status_of(pool: &str, entry: &BackendEntry, now_ms: u64) -> BackendStatus {
    let checked = entry.health.checked.load(Ordering::Relaxed);
    let ejected = entry.health.ejected_at(now_ms);
    BackendStatus {
        pool: pool.to_owned(),
        address: entry.label.clone(),
        drain_state: entry.drain_state.load(Ordering::Relaxed),
        connections: entry.connections.load(Ordering::Relaxed).max(0),
        healthy: checked.then(|| entry.health.healthy.load(Ordering::Relaxed)),
        health_reason: if checked { entry.health.reason() } else { None },
        ejected,
        eject_reason: if ejected { entry.health.eject_reason.lock().unwrap().clone() } else { None },
    }
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

    fn select(&self, entries: &HashMap<SocketAddr, BackendEntry>, now_ms: u64) -> Option<SocketAddr> {
        let addr = self
            .backends
            .iter()
            .filter(|e| is_available(entries, &e.addr, now_ms))
            .min_by_key(|e| entries.get(&e.addr).map(|d| d.connections.load(Ordering::Relaxed)).unwrap_or(0))?
            .addr;
        if let Some(entry) = entries.get(&addr) {
            entry.acquire();
        }
        Some(addr)
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
/// Create the drain entries for a pool. Each item is the backend's address as
/// configured paired with the address it resolved to; the map is keyed by the
/// resolved one, which is what selection and drain commands use.
pub fn build_drain_entries(pool_name: &str, addrs: &[(&str, &str)], drain: &mut DrainTable) -> Result<()> {
    let entries = drain.entry(pool_name.to_owned()).or_default();
    for (configured, resolved) in addrs {
        let addr: SocketAddr = resolved.parse().with_context(|| format!("invalid address: {resolved}"))?;
        entries.entry(addr).or_insert_with(|| BackendEntry::new(pool_name, addr, configured));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(eject_for: Duration) -> PoolRegistry {
        let addrs = ["127.0.0.1:1", "127.0.0.1:2"];
        let mut drain = HashMap::new();
        let pairs: Vec<(&str, &str)> = addrs.iter().map(|a| (*a, *a)).collect();
        build_drain_entries("p", &pairs, &mut drain).unwrap();
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

    /// A reload must not touch a pool whose backends are configured as
    /// hostnames. The drain table is keyed by the resolved address, so
    /// matching on that key drained every backend and 502'd the whole pool.
    #[test]
    fn reload_keeps_hostname_backends_active() {
        // As build_pools records it: configured "backend1:80", resolved to an IP.
        let mut drain = HashMap::new();
        build_drain_entries("web", &[("backend1:80", "10.0.0.7:80")], &mut drain).unwrap();
        let reg = PoolRegistry::new(HashMap::new(), drain, HashMap::new());

        let cfg: crate::config::Config =
            serde_yml::from_str("pools:\n  web:\n    backends:\n      - address: backend1:80\n")
                .expect("config parses");
        reg.sync_from_config(&cfg);

        assert_eq!(
            reg.drain["web"][&"10.0.0.7:80".parse().unwrap()].drain_state.load(Ordering::Relaxed),
            DRAIN_ACTIVE,
            "a backend still present in config must stay active across a reload"
        );
    }

    /// A hostname that now resolves elsewhere must not look like a removal:
    /// the entry is matched on what the config says, not on the resolved IP.
    #[test]
    fn reload_ignores_a_changed_resolution() {
        let mut drain = HashMap::new();
        build_drain_entries("web", &[("backend1:80", "10.0.0.7:80")], &mut drain).unwrap();
        let reg = PoolRegistry::new(HashMap::new(), drain, HashMap::new());

        let cfg: crate::config::Config =
            serde_yml::from_str("pools:\n  web:\n    backends:\n      - address: backend1:80\n")
                .expect("config parses");
        reg.sync_from_config(&cfg);
        assert_eq!(
            reg.drain["web"][&"10.0.0.7:80".parse().unwrap()].drain_state.load(Ordering::Relaxed),
            DRAIN_ACTIVE
        );
    }

    /// The other half of the same comparison: a backend genuinely gone from
    /// config still drains.
    #[test]
    fn reload_drains_backends_removed_from_config() {
        let mut drain = HashMap::new();
        build_drain_entries(
            "web",
            &[("127.0.0.1:8080", "127.0.0.1:8080"), ("127.0.0.2:8080", "127.0.0.2:8080")],
            &mut drain,
        )
        .unwrap();
        let reg = PoolRegistry::new(HashMap::new(), drain, HashMap::new());

        let cfg: crate::config::Config =
            serde_yml::from_str("pools:\n  web:\n    backends:\n      - address: 127.0.0.1:8080\n")
                .expect("config parses");
        reg.sync_from_config(&cfg);

        assert_eq!(
            reg.drain["web"][&"127.0.0.1:8080".parse().unwrap()].drain_state.load(Ordering::Relaxed),
            DRAIN_ACTIVE
        );
        assert_eq!(
            reg.drain["web"][&"127.0.0.2:8080".parse().unwrap()].drain_state.load(Ordering::Relaxed),
            DRAIN_DRAINING
        );
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
