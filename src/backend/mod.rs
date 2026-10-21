use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use pingora::lb::{
    discovery::Static,
    selection::{Consistent, RoundRobin, Random},
    Backend, Backends, LoadBalancer,
};
use std::collections::{BTreeSet, HashMap};
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
    /// is unconditionally healthy. The verdict is the control worker's, read
    /// from the shared memory region.
    pub checked: AtomicBool,
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
            passive_failures: AtomicU32::new(0),
            ejected_until_ms: AtomicU64::new(0),
            eject_reason: Mutex::new(None),
        }
    }

    fn ejected_at(&self, now_ms: u64) -> bool {
        let until = self.ejected_until_ms.load(Ordering::Relaxed);
        until != 0 && now_ms < until
    }
}

/// Per-backend runtime state shared across all algorithm variants.
pub struct BackendEntry {
    pub drain_state: AtomicU8,
    pub connections: AtomicI64,
    pub health: HealthState,
    /// The address as text, for metric labels and status output,
    /// formatted once instead of on every request.
    label: String,
    /// This backend's metric series, resolved once.
    series: crate::metrics::BackendSeries,
    /// Kept only to finish its connections: the config no longer has it.
    orphan: AtomicBool,
}

impl BackendEntry {
    fn new(pool: &str, addr: SocketAddr) -> Self {
        let label = addr.to_string();
        let series = crate::metrics::BackendSeries::new(pool, &label);
        series.set_drain_state(DRAIN_ACTIVE);
        series.set_ejected(false);
        BackendEntry {
            drain_state: AtomicU8::new(DRAIN_ACTIVE),
            connections: AtomicI64::new(0),
            health: HealthState::new(),
            series,
            label,
            orphan: AtomicBool::new(false),
        }
    }

    /// This backend's metric series.
    pub fn series(&self) -> &crate::metrics::BackendSeries {
        &self.series
    }

    /// Stop new connections; a backend this worker holds no connection to is
    /// removed at once, as no release will come to remove it later.
    fn start_drain(&self) {
        let state = if self.connections.load(Ordering::Acquire) > 0 { DRAIN_DRAINING } else { DRAIN_REMOVED };
        self.drain_state.store(state, Ordering::Release);
        self.series.set_drain_state(state);
    }

    /// One more connection to this backend.
    fn acquire(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
        self.series.connection_opened();
    }

    /// Eligible for new traffic: not draining, not failing its check, and
    /// not passively ejected.
    fn available(&self, now_ms: u64) -> bool {
        self.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE && self.healthy() && !self.health.ejected_at(now_ms)
    }

    /// The control worker's verdict for a checked pool: no traffic until its
    /// first probe. Without a check, healthy.
    fn healthy(&self) -> bool {
        !self.health.checked.load(Ordering::Relaxed) || self.series.verdict() == Some(true)
    }

    /// Checked, and no probe has answered yet.
    fn initializing(&self) -> bool {
        self.health.checked.load(Ordering::Relaxed) && self.series.verdict().is_none()
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
    /// Checked, waiting for its first probe.
    pub initializing: bool,
    /// Passively ejected right now; the reason says what traffic failed.
    pub ejected: bool,
    pub eject_reason: Option<String>,
}

// Pool variants

#[derive(Clone)]
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
/// Entries are shared between versions, so a backend that stays keeps its
/// connections, drain state, health and ejection across a reload.
pub type DrainTable = HashMap<String, HashMap<SocketAddr, Arc<BackendEntry>>>;

/// One version of the pools, built from one config.
pub struct Pools {
    pools: HashMap<String, Pool>,
    drain: DrainTable,
    /// Pools with passive detection enabled.
    passive: HashMap<String, PassiveRule>,
    /// Pool name → the backends, weights and algorithm it was built from,
    /// so a reload rebuilds only the balancers that changed.
    built: HashMap<String, String>,
}

impl Pools {
    /// Build every pool of `cfg`. Entries and balancers of `previous` are
    /// kept where nothing changed; backends and pools `cfg` no longer has
    /// stay as draining entries until their last connection ends.
    pub fn build(cfg: &crate::config::Config, previous: Option<&Pools>) -> Result<Pools> {
        let mut pools = HashMap::new();
        let mut drain = DrainTable::new();
        let mut passive = HashMap::new();
        let mut built = HashMap::new();
        for (name, pool_cfg) in &cfg.pools {
            if pool_cfg.passive.enabled {
                passive.insert(name.clone(), PassiveRule { failures: pool_cfg.passive.failures, eject_for: pool_cfg.passive.eject_for });
            }
            let recipe = serde_json::to_string(&(&pool_cfg.algorithm, &pool_cfg.backends))?;
            let old = previous.filter(|p| p.built.get(name) == Some(&recipe)).and_then(|p| p.pools.get(name));
            let pool = match old {
                Some(pool) => pool.clone(),
                None => build_pool(name, pool_cfg)?,
            };
            let entries = drain.entry(name.clone()).or_default();
            for b in &pool_cfg.backends {
                let addr: SocketAddr = b.address.parse().with_context(|| format!("invalid address: {}", b.address))?;
                let kept = previous.and_then(|p| p.drain.get(name)?.get(&addr)).cloned();
                let entry = kept.unwrap_or_else(|| Arc::new(BackendEntry::new(name, addr)));
                if entry.orphan.swap(false, Ordering::Relaxed) {
                    // Removed earlier and back now: active again. A drain the
                    // cluster holds is applied after this, by the applier.
                    entry.drain_state.store(DRAIN_ACTIVE, Ordering::Release);
                    entry.series.set_drain_state(DRAIN_ACTIVE);
                }
                entry.health.checked.store(pool_cfg.health_check.is_some(), Ordering::Relaxed);
                entries.insert(addr, entry);
            }
            pools.insert(name.clone(), pool);
            built.insert(name.clone(), recipe);
        }
        // What the config dropped drains, and goes once nothing is open.
        for (name, old_entries) in previous.map(|p| &p.drain).into_iter().flatten() {
            for (addr, entry) in old_entries {
                if drain.get(name).is_some_and(|e| e.contains_key(addr)) {
                    continue;
                }
                if !entry.orphan.swap(true, Ordering::Relaxed) && entry.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE {
                    entry.start_drain();
                    tracing::info!(pool = name, backend = entry.label, "reload: backend removed from config, draining");
                }
                if entry.drain_state.load(Ordering::Relaxed) != DRAIN_REMOVED {
                    drain.entry(name.clone()).or_default().insert(*addr, Arc::clone(entry));
                }
            }
        }
        Ok(Pools { pools, drain, passive, built })
    }
}

/// The selection algorithm of one pool, over its configured backends.
fn build_pool(name: &str, cfg: &crate::config::Pool) -> Result<Pool> {
    use crate::config::LbAlgorithm;
    let addrs: Vec<&str> = cfg.backends.iter().map(|b| b.address.as_str()).collect();
    let weights: Vec<usize> = cfg.backends.iter().map(|b| b.weight as usize).collect();
    let lb = |e: anyhow::Error| anyhow::anyhow!("pool '{name}': {e}");
    Ok(match cfg.algorithm {
        LbAlgorithm::RoundRobin => Pool::RoundRobin(Arc::new(build_lb(&addrs, &weights).map_err(lb)?)),
        LbAlgorithm::Random => Pool::Random(Arc::new(build_lb(&addrs, &weights).map_err(lb)?)),
        LbAlgorithm::ConsistentHash => Pool::ConsistentHash(Arc::new(build_lb(&addrs, &weights).map_err(lb)?)),
        LbAlgorithm::LeastConnections => Pool::LeastConn(Arc::new(LeastConnPool::build(&addrs, &weights).map_err(lb)?)),
    })
}

/// Manages all configured backend pools, their selection algorithms, and per-backend
/// drain state / connection counters. A reload swaps in a new [`Pools`].
pub struct PoolRegistry {
    state: ArcSwap<Pools>,
    /// Reference point for ejection deadlines (ms since here fit an atomic).
    epoch: Instant,
}

impl PoolRegistry {
    /// Construct from pre-built pools.
    #[cfg(test)]
    pub fn new(pools: HashMap<String, Pool>, drain: DrainTable, passive: HashMap<String, PassiveRule>) -> Self {
        Self::from_pools(Pools { pools, drain, passive, built: HashMap::new() })
    }

    pub fn from_pools(pools: Pools) -> Self {
        PoolRegistry { state: ArcSwap::from_pointee(pools), epoch: Instant::now() }
    }

    /// Build the pools of a new config against the current ones: pools,
    /// backends, weights, algorithms, passive rules and health-check
    /// coverage. Fallible; nothing changes until [`commit`](Self::commit).
    pub fn prepare(&self, cfg: &crate::config::Config) -> Result<Pools> {
        Pools::build(cfg, Some(&self.state.load()))
    }

    pub fn commit(&self, pools: Pools) {
        self.state.store(Arc::new(pools));
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// A backend's entry in the current version.
    pub fn backend(&self, pool_name: &str, addr: SocketAddr) -> Option<Arc<BackendEntry>> {
        self.state.load().drain.get(pool_name)?.get(&addr).cloned()
    }

    /// Every backend entry with the pool it belongs to.
    fn entries(&self) -> Vec<(String, Arc<BackendEntry>)> {
        let s = self.state.load();
        s.drain.iter().flat_map(|(pool, entries)| entries.values().map(move |e| (pool.clone(), Arc::clone(e)))).collect()
    }

    /// Select a backend from the named pool. Skips draining/removed backends.
    /// `key` is used for consistent hashing; ignored by round-robin and random.
    pub fn select(&self, pool_name: &str, key: &[u8]) -> Option<SocketAddr> {
        self.select_except(pool_name, key, None)
    }

    /// Another backend of the pool than `addr` can take traffic.
    pub fn has_available_except(&self, pool_name: &str, addr: SocketAddr) -> bool {
        let now = self.now_ms();
        self.state.load().drain.get(pool_name).is_some_and(|entries| entries.iter().any(|(a, e)| *a != addr && e.available(now)))
    }

    /// As [`select`](Self::select), never picking `except`: a retry's
    /// other backend.
    pub fn select_except(&self, pool_name: &str, key: &[u8], except: Option<SocketAddr>) -> Option<SocketAddr> {
        let s = self.state.load();
        let pool = s.pools.get(pool_name)?;
        let empty = HashMap::new();
        let entries = s.drain.get(pool_name).unwrap_or(&empty);
        let now = self.now_ms();

        // Pingora's own `healthy` flag is always true here: no Pingora
        // health check is attached, Keel keeps health in `drain`.
        let available = |b: &Backend, _: bool| {
            b.addr.as_inet().is_some_and(|a| Some(*a) != except && is_available(entries, a, now))
        };
        let picked = match pool {
            Pool::RoundRobin(lb) => lb.select_with(key, 256, available),
            Pool::Random(lb) => lb.select_with(key, 256, available),
            Pool::ConsistentHash(lb) => lb.select_with(key, 256, available),
            Pool::LeastConn(lc) => return lc.select(entries, now, except),
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
        let s = self.state.load();
        let Some(rule) = s.passive.get(pool_name) else { return };
        let Some(entry) = s.drain.get(pool_name).and_then(|e| e.get(&addr)) else { return };
        let now = self.now_ms();
        if entry.health.ejected_at(now) {
            return;
        }
        let n = entry.health.passive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n < rule.failures {
            return;
        }
        entry.health.passive_failures.store(0, Ordering::Relaxed);
        let available = s.drain.get(pool_name).map_or(0, |entries| entries.values().filter(|e| e.available(now)).count());
        if available <= 1 {
            tracing::debug!(pool = pool_name, backend = %addr, "passive: last available backend, not ejecting");
            return;
        }
        let until = now + rule.eject_for.as_millis() as u64;
        entry.health.ejected_until_ms.store(until.max(1), Ordering::Release);
        let reason = format!("{n} consecutive {what} failures");
        *entry.health.eject_reason.lock().unwrap() = Some(reason.clone());
        entry.series.set_ejected(true);
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
        let s = self.state.load();
        if !s.passive.contains_key(pool_name) {
            return;
        }
        if let Some(entry) = s.drain.get(pool_name).and_then(|e| e.get(&addr)) {
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
                entry.series.set_ejected(false);
                tracing::info!(pool, backend = entry.label, "passive: ejection expired, backend re-admitted");
                readmitted += 1;
            }
        }
        readmitted
    }

    /// Decrement the connection counter for `addr` in `pool_name`.
    /// If the backend is draining and this was the last connection, marks it removed.
    pub fn release(&self, pool_name: &str, addr: SocketAddr) {
        if let Some(entry) = self.backend(pool_name, addr) {
            let prev = entry.connections.fetch_sub(1, Ordering::Relaxed);
            if prev > 0 {
                entry.series.connection_closed();
            }
            // Auto-remove when last draining connection finishes
            if entry.drain_state.load(Ordering::Acquire) == DRAIN_DRAINING && prev <= 1 {
                entry.drain_state.store(DRAIN_REMOVED, Ordering::Release);
                entry.series.set_drain_state(DRAIN_REMOVED);
                tracing::info!(pool = pool_name, backend = %addr, "drain complete: backend removed");
            }
        }
    }

    /// Whether a checked backend still waits for its first probe.
    pub fn initializing(&self) -> bool {
        self.state.load().drain.values().flat_map(HashMap::values).any(|e| e.initializing())
    }

    /// Mark every backend of a pool as covered by an active check: its
    /// health is the control worker's verdict, and status says so.
    #[cfg(test)]
    pub fn mark_checked(&self, pool_name: &str) {
        for entry in self.state.load().drain.get(pool_name).into_iter().flat_map(HashMap::values) {
            entry.health.checked.store(true, Ordering::Relaxed);
        }
    }

    /// The pool selects by the key passed to `select` (consistent hashing);
    /// the other algorithms ignore it, so callers can skip building it.
    pub fn uses_key(&self, pool_name: &str) -> bool {
        matches!(self.state.load().pools.get(pool_name), Some(Pool::ConsistentHash(_)))
    }

    /// Returns true if the named pool exists.
    pub fn has_pool(&self, pool_name: &str) -> bool {
        self.state.load().pools.contains_key(pool_name)
    }

    /// All backends across all pools, sorted by pool then address.
    pub fn all_backends(&self) -> Vec<BackendStatus> {
        let now = self.now_ms();
        let mut result: Vec<BackendStatus> = self.entries().iter().map(|(pool, entry)| status_of(pool, entry, now)).collect();
        result.sort_by(|a, b| a.pool.cmp(&b.pool).then(a.address.cmp(&b.address)));
        result
    }

    /// All backends in a specific pool, sorted by address.
    pub fn backends_for_pool(&self, pool_name: &str) -> Vec<BackendStatus> {
        let now = self.now_ms();
        let s = self.state.load();
        let mut result: Vec<BackendStatus> =
            s.drain.get(pool_name).into_iter().flat_map(HashMap::values).map(|entry| status_of(pool_name, entry, now)).collect();
        result.sort_by(|a, b| a.address.cmp(&b.address));
        result
    }

    /// Drain every backend `addr` names, in every pool. Returns the pools.
    pub fn drain_by_address(&self, addr: &str) -> Vec<String> {
        self.each_named(addr, |pool, entry| {
            entry.start_drain();
            tracing::info!(pool, backend = entry.label, "control: drain initiated");
        })
    }

    /// Return every backend `addr` names to service, in every pool. Its open
    /// connections are untouched. Returns the pools.
    pub fn activate_by_address(&self, addr: &str) -> Vec<String> {
        self.each_named(addr, |pool, entry| {
            entry.drain_state.store(DRAIN_ACTIVE, Ordering::Release);
            entry.series.set_drain_state(DRAIN_ACTIVE);
            tracing::info!(pool, backend = entry.label, "control: backend activated");
        })
    }

    /// Backends a reload dropped are no longer the operator's to drain or
    /// activate; they only finish.
    fn each_named(&self, addr: &str, mut f: impl FnMut(&str, &BackendEntry)) -> Vec<String> {
        let mut found = Vec::new();
        for (pool, entry) in self.entries().iter().filter(|(_, e)| !e.orphan.load(Ordering::Relaxed) && names(addr, &e.label)) {
            f(pool, entry);
            found.push(pool.clone());
        }
        found.sort();
        found.dedup();
        found
    }
}

// Helpers

/// `addr` names a backend: `ip:port` exactly, or an IP for every port on it.
pub fn names(addr: &str, label: &str) -> bool {
    if addr == label {
        return true;
    }
    match (addr.parse::<std::net::IpAddr>(), label.parse::<SocketAddr>()) {
        (Ok(ip), Ok(backend)) => backend.ip() == ip,
        _ => false,
    }
}

fn is_available(entries: &HashMap<SocketAddr, Arc<BackendEntry>>, addr: &SocketAddr, now_ms: u64) -> bool {
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
        healthy: checked.then(|| entry.healthy()),
        initializing: entry.initializing(),
        ejected,
        eject_reason: if ejected { entry.health.eject_reason.lock().unwrap().clone() } else { None },
    }
}

// Least connections

pub struct LeastConnEntry {
    pub addr: SocketAddr,
    pub weight: i64,
}

pub struct LeastConnPool {
    backends: Vec<LeastConnEntry>,
}

impl LeastConnPool {
    pub fn build(addrs: &[&str], weights: &[usize]) -> Result<Self> {
        let backends = addrs
            .iter()
            .zip(weights)
            .map(|(a, &weight)| {
                let addr: SocketAddr = a.parse().with_context(|| format!("invalid address: {a}"))?;
                Ok(LeastConnEntry { addr, weight: weight as i64 })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(LeastConnPool { backends })
    }

    /// The available backend with the fewest connections per unit of
    /// weight, as nginx's weighted least_conn; ties go to the first listed.
    fn select(&self, entries: &HashMap<SocketAddr, Arc<BackendEntry>>, now_ms: u64, except: Option<SocketAddr>) -> Option<SocketAddr> {
        let conns = |e: &LeastConnEntry| entries.get(&e.addr).map(|d| d.connections.load(Ordering::Relaxed)).unwrap_or(0);
        let addr = self
            .backends
            .iter()
            .filter(|e| Some(e.addr) != except && is_available(entries, &e.addr, now_ms))
            // a/wa < b/wb without division: a*wb < b*wa.
            .min_by(|a, b| (conns(a) * b.weight).cmp(&(conns(b) * a.weight)))?
            .addr;
        if let Some(entry) = entries.get(&addr) {
            entry.acquire();
        }
        Some(addr)
    }
}

// Load balancer builder

/// Build a Pingora LoadBalancer over the weighted backends.
pub fn build_lb<S>(addrs: &[&str], weights: &[usize]) -> Result<LoadBalancer<S>>
where
    S: pingora::lb::selection::BackendSelection + Send + Sync + 'static,
    S::Iter: pingora::lb::selection::BackendIter,
{
    use std::future::Future;
    use std::task::{Context as TaskContext, Poll, Waker};

    let backends: BTreeSet<Backend> = addrs
        .iter()
        .zip(weights.iter())
        .map(|(addr, &weight)| {
            Backend::new_with_weight(addr, weight).map_err(|e| anyhow::anyhow!("{e}"))
        })
        .collect::<Result<_>>()?;

    let lb = LoadBalancer::from_backends(Backends::new(Static::new(backends)));
    // Static discovery answers at once, so one poll completes the update
    // (Pingora's own `try_from_iter` does the same).
    let polled = std::pin::pin!(lb.update()).poll(&mut TaskContext::from_waker(Waker::noop()));
    match polled {
        Poll::Ready(result) => result.map_err(|e| anyhow::anyhow!("{e}"))?,
        Poll::Pending => anyhow::bail!("static backend discovery did not complete"),
    }
    Ok(lb)
}

/// Create the drain entries for a pool's backends.
#[cfg(test)]
pub fn build_drain_entries(pool_name: &str, addrs: &[&str], drain: &mut DrainTable) -> Result<()> {
    let entries = drain.entry(pool_name.to_owned()).or_default();
    for address in addrs {
        let addr: SocketAddr = address.parse().with_context(|| format!("invalid address: {address}"))?;
        entries.entry(addr).or_insert_with(|| Arc::new(BackendEntry::new(pool_name, addr)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_names_a_backend_exactly_or_by_ip() {
        assert!(names("10.0.0.1:80", "10.0.0.1:80"));
        assert!(names("10.0.0.1", "10.0.0.1:80"));
        assert!(names("::1", "[::1]:80"));
        assert!(!names("10.0.0.1:81", "10.0.0.1:80"));
        assert!(!names("10.0.0.10", "10.0.0.1:80"));
    }

    fn registry(eject_for: Duration) -> PoolRegistry {
        registry_of(["127.0.0.1:1", "127.0.0.1:2"], eject_for)
    }

    /// Distinct addresses keep a test's metric series, the health verdict
    /// among them, apart from other tests'.
    fn registry_of(addrs: [&str; 2], eject_for: Duration) -> PoolRegistry {
        let mut drain = HashMap::new();
        build_drain_entries("p", &addrs, &mut drain).unwrap();
        let mut pools = HashMap::new();
        pools.insert("p".to_owned(), Pool::LeastConn(Arc::new(LeastConnPool::build(&addrs, &[1, 1]).unwrap())));
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

    /// A backend written differently but at the same address is the same
    /// backend.
    #[test]
    fn reload_matches_backends_by_address() {
        let mut drain = HashMap::new();
        build_drain_entries("web", &["[2001:db8::7]:80"], &mut drain).unwrap();
        let reg = PoolRegistry::new(HashMap::new(), drain, HashMap::new());

        let cfg: crate::config::Config =
            crate::config::parse("pool \"web\" {\n  backend \"[2001:db8:0::7]:80\" {}\n}\n")
                .expect("config parses");
        reg.commit(reg.prepare(&cfg).unwrap());
        assert_eq!(
            reg.backend("web", "[2001:db8::7]:80".parse().unwrap()).unwrap().drain_state.load(Ordering::Relaxed),
            DRAIN_ACTIVE
        );
    }

    fn pools(text: &str) -> crate::config::Config {
        crate::config::parse(text).expect("config parses")
    }

    fn drain_state(reg: &PoolRegistry, pool: &str, addr: &str) -> Option<u8> {
        reg.backend(pool, addr.parse().unwrap()).map(|e| e.drain_state.load(Ordering::Relaxed))
    }

    /// Pools, backends and weights change live; what stays keeps its state.
    #[test]
    fn a_reload_adds_removes_and_keeps_backends() {
        let one = "pool \"web\" {\n  backend \"127.0.1.1:80\" {}\n  backend \"127.0.1.2:80\" {}\n}\n";
        let reg = PoolRegistry::from_pools(Pools::build(&pools(one), None).unwrap());
        // An open connection to .2, and an operator's drain of .1.
        let two: SocketAddr = "127.0.1.2:80".parse().unwrap();
        reg.backend("web", two).unwrap().acquire();
        reg.drain_by_address("127.0.1.1:80");

        // .2 gone, .3 new, a pool added.
        let next = "pool \"web\" {\n  backend \"127.0.1.1:80\" {}\n  backend \"127.0.1.3:80\" { weight = 3 }\n}\npool \"api\" {\n  backend \"127.0.1.9:80\" {}\n}\n";
        reg.commit(reg.prepare(&pools(next)).unwrap());
        assert_eq!(drain_state(&reg, "web", "127.0.1.1:80"), Some(DRAIN_REMOVED), "a kept backend keeps its drain");
        assert_eq!(drain_state(&reg, "web", "127.0.1.2:80"), Some(DRAIN_DRAINING), "removed, one connection still open");
        assert_eq!(reg.select("web", b""), Some("127.0.1.3:80".parse().unwrap()), "the new backend takes traffic");
        assert_eq!(reg.select("api", b""), Some("127.0.1.9:80".parse().unwrap()), "and the new pool");
        assert!(reg.drain_by_address("127.0.1.2:80").is_empty(), "a removed backend is not the operator's to drain");

        // Its last connection ends: gone from the next version.
        reg.release("web", two);
        assert_eq!(drain_state(&reg, "web", "127.0.1.2:80"), Some(DRAIN_REMOVED));
        reg.commit(reg.prepare(&pools(next)).unwrap());
        assert_eq!(drain_state(&reg, "web", "127.0.1.2:80"), None);

        // Back in the config: active again.
        reg.commit(reg.prepare(&pools(one)).unwrap());
        assert_eq!(drain_state(&reg, "web", "127.0.1.2:80"), Some(DRAIN_ACTIVE));
        assert!(!reg.has_pool("api"));
    }

    /// A pool that gains a health check stops sending to backends without a
    /// verdict; one that loses its check sends to all.
    #[test]
    fn a_reload_follows_health_check_coverage() {
        let plain = "pool \"web\" {\n  backend \"127.0.2.1:80\" {}\n}\n";
        let checked = "pool \"web\" {\n  health_check \"tcp\" {}\n\n  backend \"127.0.2.1:80\" {}\n}\n";
        let reg = PoolRegistry::from_pools(Pools::build(&pools(plain), None).unwrap());
        assert!(reg.select("web", b"").is_some());
        reg.commit(reg.prepare(&pools(checked)).unwrap());
        assert!(reg.initializing() && reg.select("web", b"").is_none());
        reg.commit(reg.prepare(&pools(plain)).unwrap());
        assert!(!reg.initializing() && reg.select("web", b"").is_some());
    }

    #[test]
    fn a_drain_with_open_connections_ends_with_the_last_one() {
        let reg = registry_of(["127.0.0.8:1", "127.0.0.8:2"], Duration::from_secs(60));
        let a: SocketAddr = "127.0.0.8:1".parse().unwrap();
        let state = |reg: &PoolRegistry| reg.backend("p", a).unwrap().drain_state.load(Ordering::Relaxed);
        reg.backend("p", a).unwrap().acquire();
        reg.backend("p", a).unwrap().acquire();
        assert_eq!(reg.drain_by_address("127.0.0.8:1"), ["p"]);
        assert_eq!(state(&reg), DRAIN_DRAINING);
        reg.release("p", a);
        assert_eq!(state(&reg), DRAIN_DRAINING);
        reg.release("p", a);
        assert_eq!(state(&reg), DRAIN_REMOVED);
        // Drained with nothing open: removed at once.
        assert_eq!(reg.drain_by_address("127.0.0.8:2"), ["p"]);
        assert_eq!(reg.backend("p", "127.0.0.8:2".parse().unwrap()).unwrap().drain_state.load(Ordering::Relaxed), DRAIN_REMOVED);
    }

    #[test]
    fn a_checked_pool_follows_the_control_workers_verdict() {
        let reg = registry_of(["127.0.0.9:1", "127.0.0.9:2"], Duration::from_secs(60));
        let a: SocketAddr = "127.0.0.9:1".parse().unwrap();
        let verdict = crate::metrics::BackendSeries::new("p", "127.0.0.9:1");
        verdict.set_healthy(false);
        assert_eq!(selections(&reg, 6).len(), 2, "an unchecked pool ignores verdicts");
        reg.mark_checked("p");
        // No verdict for the second yet: initializing, no traffic.
        assert!(reg.select("p", b"").is_none() && reg.initializing());
        let b = reg.backends_for_pool("p").into_iter().find(|b| b.address == "127.0.0.9:2").unwrap();
        assert!(b.initializing && b.healthy == Some(false), "initializing");
        crate::metrics::BackendSeries::new("p", "127.0.0.9:2").set_healthy(true);
        assert!(!reg.initializing());
        assert_eq!(selections(&reg, 6), ["127.0.0.9:2".parse().unwrap()].into_iter().collect());
        assert_eq!(reg.backends_for_pool("p").iter().find(|b| b.address == a.to_string()).unwrap().healthy, Some(false));
        verdict.set_healthy(true);
        assert_eq!(selections(&reg, 6).len(), 2);
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

    /// A pool of :1 (weight 1) and :2 (weight 3).
    fn weighted(pool: Pool) -> PoolRegistry {
        let mut drain = HashMap::new();
        build_drain_entries("p", &["127.0.0.1:1", "127.0.0.1:2"], &mut drain).unwrap();
        let mut pools = HashMap::new();
        pools.insert("p".to_owned(), pool);
        PoolRegistry::new(pools, drain, HashMap::new())
    }

    const ADDRS: [&str; 2] = ["127.0.0.1:1", "127.0.0.1:2"];
    const WEIGHTS: [usize; 2] = [1, 3];

    /// Selections that went to the weight-1 backend out of `n`, each with its
    /// own key and released at once.
    fn light_share(reg: &PoolRegistry, n: usize) -> usize {
        let light: SocketAddr = "127.0.0.1:1".parse().unwrap();
        (0..n)
            .filter(|i| {
                let a = reg.select("p", format!("10.0.{}.{}", i / 256, i % 256).as_bytes()).expect("a backend");
                reg.release("p", a);
                a == light
            })
            .count()
    }

    #[test]
    fn round_robin_honours_weights() {
        let reg = weighted(Pool::RoundRobin(Arc::new(build_lb(&ADDRS, &WEIGHTS).unwrap())));
        assert_eq!(light_share(&reg, 400), 100);
    }

    #[test]
    fn random_honours_weights() {
        let reg = weighted(Pool::Random(Arc::new(build_lb(&ADDRS, &WEIGHTS).unwrap())));
        let light = light_share(&reg, 4000);
        assert!((850..=1150).contains(&light), "{light} of 4000, expected about 1000");
    }

    #[test]
    fn consistent_hash_honours_weights() {
        let reg = weighted(Pool::ConsistentHash(Arc::new(build_lb(&ADDRS, &WEIGHTS).unwrap())));
        let light = light_share(&reg, 4000);
        assert!((700..=1300).contains(&light), "{light} of 4000 keys, expected about 1000");
    }

    /// Connections held open spread 1:3: the weight-3 backend takes a new
    /// one until it holds three times as many.
    #[test]
    fn least_connections_honours_weights() {
        let reg = weighted(Pool::LeastConn(Arc::new(LeastConnPool::build(&ADDRS, &WEIGHTS).unwrap())));
        let light: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let held: Vec<SocketAddr> = (0..40).map(|_| reg.select("p", b"").expect("a backend")).collect();
        assert_eq!(held.iter().filter(|a| **a == light).count(), 10);
    }
}
