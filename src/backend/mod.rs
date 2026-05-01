use anyhow::{Context, Result};
use pingora::lb::{
    selection::{Consistent, RoundRobin, Random},
    Backend, LoadBalancer,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;

// DRAIN STATE

pub const DRAIN_ACTIVE: u8 = 0;
pub const DRAIN_DRAINING: u8 = 1;
pub const DRAIN_REMOVED: u8 = 2;

/// Per-backend runtime state shared across all algorithm variants.
pub struct BackendEntry {
    pub drain_state: AtomicU8,
    pub connections: AtomicI64,
}

impl BackendEntry {
    fn new() -> Self {
        BackendEntry {
            drain_state: AtomicU8::new(DRAIN_ACTIVE),
            connections: AtomicI64::new(0),
        }
    }
}

/// Snapshot of a backend's runtime state for status reporting.
pub struct BackendStatus {
    pub pool: String,
    pub address: String,
    pub drain_state: u8,
    pub connections: i64,
}

// POOL VARIANTS

pub enum Pool {
    RoundRobin(Arc<LoadBalancer<RoundRobin>>),
    Random(Arc<LoadBalancer<Random>>),
    ConsistentHash(Arc<LoadBalancer<Consistent>>),
    LeastConn(Arc<LeastConnPool>),
}

// POOL REGISTRY

/// Manages all configured backend pools, their selection algorithms, and per-backend
/// drain state / connection counters.
pub struct PoolRegistry {
    pools: HashMap<String, Pool>,
    /// Flat map of all backend addresses across all pools.
    /// Key: "pool_name/addr:port" so the same IP can appear in multiple pools.
    drain: HashMap<String, BackendEntry>,
}

impl PoolRegistry {
    /// Construct from pre-built pools (see proxy::build_pools).
    pub fn new(pools: HashMap<String, Pool>, drain: HashMap<String, BackendEntry>) -> Self {
        PoolRegistry { pools, drain }
    }

    /// Select a backend from the named pool. Skips draining/removed backends.
    /// `key` is used for consistent hashing; ignored by round-robin and random.
    pub fn select(&self, pool_name: &str, key: &[u8]) -> Option<SocketAddr> {
        let pool = self.pools.get(pool_name)?;
        let drain = &self.drain;

        match pool {
            Pool::RoundRobin(lb) => {
                lb.select_with(key, 256, |b, healthy| {
                    healthy && is_active(drain, pool_name, &b.addr.to_string())
                })
                .and_then(|b| track_and_return(drain, pool_name, b))
            }
            Pool::Random(lb) => {
                lb.select_with(key, 256, |b, healthy| {
                    healthy && is_active(drain, pool_name, &b.addr.to_string())
                })
                .and_then(|b| track_and_return(drain, pool_name, b))
            }
            Pool::ConsistentHash(lb) => {
                lb.select_with(key, 256, |b, healthy| {
                    healthy && is_active(drain, pool_name, &b.addr.to_string())
                })
                .and_then(|b| track_and_return(drain, pool_name, b))
            }
            Pool::LeastConn(lc) => lc.select(pool_name, drain),
        }
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
                Some(BackendStatus {
                    pool: pool.to_owned(),
                    address: addr.to_owned(),
                    drain_state: entry.drain_state.load(Ordering::Relaxed),
                    connections: entry.connections.load(Ordering::Relaxed).max(0),
                })
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
                BackendStatus {
                    pool: pool_name.to_owned(),
                    address: addr.to_owned(),
                    drain_state: entry.drain_state.load(Ordering::Relaxed),
                    connections: entry.connections.load(Ordering::Relaxed).max(0),
                }
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

// HELPERS

pub fn drain_key(pool: &str, addr: &str) -> String {
    format!("{pool}/{addr}")
}

fn is_active(drain: &HashMap<String, BackendEntry>, pool: &str, addr: &str) -> bool {
    let key = drain_key(pool, addr);
    drain
        .get(&key)
        .map(|e| e.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE)
        .unwrap_or(true) // unknown backends are treated as active
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

// LEAST CONNECTIONS

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

    fn select(&self, pool_name: &str, drain: &HashMap<String, BackendEntry>) -> Option<SocketAddr> {
        self.backends
            .iter()
            .filter(|e| {
                let key = drain_key(pool_name, &e.addr.to_string());
                drain
                    .get(&key)
                    .map(|d| d.drain_state.load(Ordering::Relaxed) == DRAIN_ACTIVE)
                    .unwrap_or(true)
            })
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

// LOAD BALANCER BUILDER

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
