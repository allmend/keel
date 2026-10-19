//! Node metrics, counted in shared memory (see [`crate::shm`]).
//!
//! The master creates the region before forking. Workers count into their
//! own stripe; the control worker serves `GET /metrics` on the socket the
//! master bound, summing the stripes. One endpoint per node, node totals
//! only, and no worker can make a counter go down: a replacement worker
//! keeps counting in its predecessor's stripe. Counters live as long as the
//! master.
//!
//! A series is a family and its label values. Hot paths hold handles
//! resolved once ([`RequestSeries`], [`BackendSeries`]); everything else goes
//! through a per-process cache of resolved series.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::shm::{Layout, Region};

const LATENCY_BUCKETS: [f64; 11] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
/// One cell per bucket, one above the last, and the sum in microseconds.
const HISTOGRAM_CELLS: usize = LATENCY_BUCKETS.len() + 2;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Counter,
    Histogram,
    /// Summed over the workers: each counts its own share.
    Gauge,
    /// A state every worker reports for itself; the node shows the lowest
    /// (`Min`) or highest (`Max`) of the workers that reported one. Stored
    /// as value + 1, so 0 means "not reported".
    State(Merge),
}

#[derive(Clone, Copy, PartialEq)]
enum Merge {
    Min,
    Max,
}

impl Kind {
    fn cells(self) -> u32 {
        match self {
            Kind::Histogram => HISTOGRAM_CELLS as u32,
            _ => 1,
        }
    }
}

struct Family {
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
    kind: Kind,
}

const POOL_VHOST: &[&str] = &["pool", "vhost"];
const POOL_BACKEND: &[&str] = &["pool", "backend"];
const POOL_REASON: &[&str] = &["pool", "reason"];

macro_rules! families {
    ($($id:ident = $name:literal, $kind:expr, $labels:expr, $help:literal;)*) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[derive(Clone, Copy)]
        #[repr(u8)]
        enum F { $($id),* }
        const FAMILIES: &[Family] = &[$(Family { name: $name, help: $help, labels: $labels, kind: $kind }),*];
    };
}

families! {
    REQUESTS = "keel_requests_total", Kind::Counter, &["pool", "vhost", "status"], "Total HTTP requests proxied";
    REQUEST_DURATION = "keel_request_duration_seconds", Kind::Histogram, POOL_VHOST, "End-to-end request duration in seconds";
    REQUEST_BYTES_IN = "keel_request_bytes_in_total", Kind::Counter, POOL_VHOST, "Total bytes received from clients";
    REQUEST_BYTES_OUT = "keel_request_bytes_out_total", Kind::Counter, POOL_VHOST, "Total bytes sent to clients";
    LB_ERRORS = "keel_lb_errors_total", Kind::Counter, &["pool", "vhost", "reason"], "Load-balancer-originated errors before a backend was reached";
    BACKEND_REQUESTS = "keel_backend_requests_total", Kind::Counter, &["pool", "backend", "status"], "Total HTTP requests forwarded per backend";
    BACKEND_DURATION = "keel_backend_response_duration_seconds", Kind::Histogram, POOL_BACKEND, "Time from backend selected to response complete";
    BACKEND_CONNECTION_ERRORS = "keel_backend_connection_errors_total", Kind::Counter, POOL_BACKEND, "Failed connections to backends";
    ACTIVE_CONNECTIONS = "keel_active_connections", Kind::Gauge, POOL_BACKEND, "Current active upstream connections per backend";
    BACKEND_HEALTHY = "keel_backend_healthy", Kind::State(Merge::Min), POOL_BACKEND, "Backend health state (1 = healthy, 0 = unhealthy)";
    BACKEND_DRAIN_STATE = "keel_backend_drain_state", Kind::State(Merge::Min), POOL_BACKEND, "Backend drain state (0 = active, 1 = draining, 2 = removed)";
    BACKEND_EJECTED = "keel_backend_ejected", Kind::State(Merge::Max), POOL_BACKEND, "Backend passively ejected after consecutive traffic failures (1 = ejected)";
    BACKEND_EJECTIONS = "keel_backend_ejections_total", Kind::Counter, POOL_BACKEND, "Passive ejections per backend";
    PROXY_PROTOCOL_ERRORS = "keel_proxy_protocol_errors_total", Kind::Counter, &["listener"], "Connections or datagrams on a proxy_protocol listener rejected for a missing or malformed PROXY header";
    TCP_CONNECTIONS = "keel_tcp_connections_total", Kind::Counter, POOL_BACKEND, "Total TCP (L4) connections accepted per pool and backend";
    TCP_BYTES_IN = "keel_tcp_bytes_in_total", Kind::Counter, POOL_BACKEND, "Total bytes received from TCP (L4) clients";
    TCP_BYTES_OUT = "keel_tcp_bytes_out_total", Kind::Counter, POOL_BACKEND, "Total bytes sent to TCP (L4) clients";
    TCP_ERRORS = "keel_tcp_errors_total", Kind::Counter, POOL_REASON, "TCP (L4) connections that ended in an error";
    UDP_FLOWS = "keel_udp_flows_total", Kind::Counter, POOL_BACKEND, "Total UDP (L4) flows opened per pool and backend";
    UDP_PACKETS_IN = "keel_udp_packets_in_total", Kind::Counter, POOL_BACKEND, "Total datagrams received from UDP (L4) clients";
    UDP_PACKETS_OUT = "keel_udp_packets_out_total", Kind::Counter, POOL_BACKEND, "Total datagrams sent to UDP (L4) clients";
    UDP_BYTES_IN = "keel_udp_bytes_in_total", Kind::Counter, POOL_BACKEND, "Total bytes received from UDP (L4) clients";
    UDP_BYTES_OUT = "keel_udp_bytes_out_total", Kind::Counter, POOL_BACKEND, "Total bytes sent to UDP (L4) clients";
    CACHE_REQUESTS = "keel_cache_requests_total", Kind::Counter, &["pool", "vhost", "result"], "HTTP requests with caching on, by result (hit, miss)";
    CACHE_HITS = "keel_cache_hits_total", Kind::Counter, &["tier"], "Cache hits by the tier that served them (memory, disk)";
    UDP_ERRORS = "keel_udp_errors_total", Kind::Counter, POOL_REASON, "UDP (L4) datagrams dropped or flows ended by an error";
}

// Header counters, each written by one kind of process.
/// Master: worker exits by reason.
const G_RESTARTS_EXIT: usize = 0;
const G_RESTARTS_SIGNAL: usize = 1;
/// Master: workers running and configured.
const G_WORKERS: usize = 2;
const G_WORKERS_CONFIGURED: usize = 3;
/// Master: start time, unix seconds.
const G_START: usize = 4;
/// Control worker: workers that did not answer the last liveness check.
const G_UNRESPONSIVE: usize = 5;
/// Any process: series that did not fit.
const G_DROPPED: usize = 6;

// The region and this process's place in it

static REGION: OnceLock<Region> = OnceLock::new();
/// The stripe this process writes: its worker index.
static STRIPE: AtomicUsize = AtomicUsize::new(0);
/// Resolved series of the slow path, by key.
static CACHE: OnceLock<Mutex<HashMap<Vec<u8>, Option<u32>>>> = OnceLock::new();
/// Where updates go for a series that did not fit.
static SINK: [AtomicU64; HISTOGRAM_CELLS] = [const { AtomicU64::new(0) }; HISTOGRAM_CELLS];
static FULL_LOGGED: AtomicBool = AtomicBool::new(false);

fn region() -> &'static Region {
    // Only a process the master did not set up gets here: tests and the CLI.
    REGION.get_or_init(|| Region::create(layout_for(1, 0)).expect("map the metrics region"))
}

/// The region's size for a config: what its pools, backends and routes can
/// use, four times over for reloads, and never less than 64Ki cells per
/// worker. Unused pages cost no memory.
fn layout_for(stripes: usize, estimate: usize) -> Layout {
    let cells = (estimate * 4).max(1 << 16).next_power_of_two();
    Layout { stripes, cells, slots: cells * 2, arena: cells * 64 }
}

fn estimate(cfg: &Config) -> usize {
    let backends: usize = cfg.pools.values().map(|p| p.backends.len()).sum();
    let routes: usize = cfg.vhosts.iter().map(|v| v.routes.len().max(1)).sum();
    // Per route: ten statuses, a histogram, bytes, errors. Per backend: the
    // same plus its gauges and the L4 counters.
    64 + routes * 48 + backends * 64 + cfg.pools.len() * 16 + cfg.listeners.len() * 2
}

/// Master, before forking: map the region every child inherits.
pub fn create_region(cfg: &Config) -> anyhow::Result<()> {
    // One stripe per worker, and the last one for the control worker.
    let layout = layout_for(cfg.keel.workers.max(1) + 1, estimate(cfg));
    let region = Region::create(layout).map_err(|e| anyhow::anyhow!("master: cannot map the metrics region: {e}"))?;
    region.global(G_WORKERS_CONFIGURED).store(cfg.keel.workers as u64, Ordering::Relaxed);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    region.global(G_START).store(now, Ordering::Relaxed);
    REGION.set(region).map_err(|_| anyhow::anyhow!("metrics region created twice"))
}

/// Master: a worker exited and was replaced; `signaled` when a signal ended it.
pub fn worker_restarted(signaled: bool) {
    region().global(if signaled { G_RESTARTS_SIGNAL } else { G_RESTARTS_EXIT }).fetch_add(1, Ordering::Relaxed);
}

/// Master: how many workers are running.
pub fn set_workers(n: usize) {
    region().global(G_WORKERS).store(n as u64, Ordering::Relaxed);
}

/// Control worker: how many workers missed the last liveness check.
pub fn set_unresponsive(n: usize) {
    region().global(G_UNRESPONSIVE).store(n as u64, Ordering::Relaxed);
}

/// Worker, first thing: write stripe `index` from now on. Gauges and states
/// left by a predecessor describe connections and views that died with it.
pub fn init_worker(index: usize) {
    STRIPE.store(index, Ordering::Relaxed);
    reset_gauges(region(), index);
}

/// Control worker, first thing: write the last stripe. Its health verdicts
/// survive a restart of the control worker, so nothing is reset.
pub fn init_prober() {
    STRIPE.store(prober_stripe(), Ordering::Relaxed);
}

/// The stripe the control worker writes.
fn prober_stripe() -> usize {
    region().layout().stripes - 1
}

fn reset_gauges(region: &Region, stripe: usize) {
    for s in region.each_series() {
        let Some(family) = s.key.first().and_then(|f| FAMILIES.get(*f as usize)) else { continue };
        if matches!(family.kind, Kind::Gauge | Kind::State(_)) {
            region.cell(stripe, s.first_cell).store(0, Ordering::Relaxed);
        }
    }
}

fn key(family: F, labels: &[&str]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + labels.iter().map(|l| l.len() + 1).sum::<usize>());
    key.push(family as u8);
    for (i, l) in labels.iter().enumerate() {
        if i > 0 {
            key.push(0);
        }
        key.extend_from_slice(l.as_bytes());
    }
    key
}

/// The first cell of a series, added if new; `None` when the region is full.
fn first_cell(family: F, labels: &[&str]) -> Option<u32> {
    let key = key(family, labels);
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().unwrap().get(&key) {
        return *found;
    }
    let found = region().series(&key, FAMILIES[family as usize].kind.cells());
    if found.is_none() {
        region().global(G_DROPPED).fetch_add(1, Ordering::Relaxed);
        if !FULL_LOGGED.swap(true, Ordering::Relaxed) {
            warn!(metric = FAMILIES[family as usize].name, "metrics: shared memory region full, new series are not recorded");
        }
    }
    cache.lock().unwrap().insert(key, found);
    found
}

/// This process's cells of a series.
fn cells(family: F, labels: &[&str]) -> &'static [AtomicU64] {
    cells_in(STRIPE.load(Ordering::Relaxed), family, labels)
}

/// The cells of a series in `stripe`.
fn cells_in(stripe: usize, family: F, labels: &[&str]) -> &'static [AtomicU64] {
    let n = FAMILIES[family as usize].kind.cells() as usize;
    match first_cell(family, labels) {
        Some(first) => {
            let region: &'static Region = region();
            // Consecutive cells of one stripe.
            let first = region.cell(stripe, first) as *const AtomicU64;
            // SAFETY: the series owns `n` consecutive cells in this stripe.
            unsafe { std::slice::from_raw_parts(first, n) }
        }
        None => &SINK[..n],
    }
}

// Handles

#[derive(Clone, Copy)]
pub struct Counter(&'static AtomicU64);

impl Counter {
    fn of(family: F, labels: &[&str]) -> Self {
        Counter(&cells(family, labels)[0])
    }
    pub fn inc(self) {
        self.add(1);
    }
    pub fn add(self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
struct Histogram(&'static [AtomicU64]);

impl Histogram {
    fn of(family: F, labels: &[&str]) -> Self {
        Histogram(cells(family, labels))
    }
    fn observe(self, secs: f64) {
        let bucket = LATENCY_BUCKETS.iter().position(|b| secs <= *b).unwrap_or(LATENCY_BUCKETS.len());
        self.0[bucket].fetch_add(1, Ordering::Relaxed);
        self.0[HISTOGRAM_CELLS - 1].fetch_add((secs * 1e6) as u64, Ordering::Relaxed);
    }
}

/// A gauge summed over the workers; each adds and removes its own share.
#[derive(Clone, Copy)]
struct Gauge(&'static AtomicU64);

impl Gauge {
    fn add(self, n: i64) {
        self.0.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// A state this worker reports, merged with the other workers' on read.
#[derive(Clone, Copy)]
struct State(&'static AtomicU64);

impl State {
    fn set(self, value: u64) {
        self.0.store(value + 1, Ordering::Relaxed);
    }
}

/// Counters per status code, resolved on a code's first use.
struct StatusCounters {
    family: F,
    labels: [String; 2],
    /// First cell + 1 per status code; 0 until first used.
    cells: Box<[AtomicU32]>,
}

impl StatusCounters {
    fn new(family: F, a: &str, b: &str) -> Self {
        StatusCounters { family, labels: [a.to_owned(), b.to_owned()], cells: (0..1000).map(|_| AtomicU32::new(0)).collect() }
    }

    fn inc(&self, status: u16) {
        let slot = &self.cells[(status as usize).min(999)];
        let cell = match slot.load(Ordering::Relaxed) {
            0 => {
                let status = status.to_string();
                let labels = [self.labels[0].as_str(), self.labels[1].as_str(), status.as_str()];
                let Some(first) = first_cell(self.family, &labels) else { return };
                slot.store(first + 1, Ordering::Relaxed);
                first
            }
            n => n - 1,
        };
        region().cell(STRIPE.load(Ordering::Relaxed), cell).fetch_add(1, Ordering::Relaxed);
    }
}

/// Series of one route: requests to `pool` through `vhost`.
pub struct RequestSeries {
    pool: String,
    vhost: String,
    requests: StatusCounters,
    duration: Histogram,
    bytes_in: Counter,
    bytes_out: Counter,
    /// Hits and misses, on the first request with caching on.
    cache: OnceLock<[Counter; 2]>,
}

impl RequestSeries {
    pub fn new(pool: &str, vhost: &str) -> Self {
        let labels = [pool, vhost];
        RequestSeries {
            pool: pool.to_owned(),
            vhost: vhost.to_owned(),
            requests: StatusCounters::new(F::REQUESTS, pool, vhost),
            duration: Histogram::of(F::REQUEST_DURATION, &labels),
            bytes_in: Counter::of(F::REQUEST_BYTES_IN, &labels),
            bytes_out: Counter::of(F::REQUEST_BYTES_OUT, &labels),
            cache: OnceLock::new(),
        }
    }

    /// A request with caching on was answered from the cache, or not.
    pub fn cache_lookup(&self, hit: bool) {
        let [hits, misses] = self.cache.get_or_init(|| {
            let (p, v) = (self.pool.as_str(), self.vhost.as_str());
            [Counter::of(F::CACHE_REQUESTS, &[p, v, "hit"]), Counter::of(F::CACHE_REQUESTS, &[p, v, "miss"])]
        });
        if hit { hits } else { misses }.inc();
    }

    pub fn record(&self, status: u16, secs: f64, bytes_in: usize, bytes_out: usize) {
        self.requests.inc(status);
        self.duration.observe(secs);
        self.bytes_in.add(bytes_in as u64);
        self.bytes_out.add(bytes_out as u64);
    }
}

/// Series of one backend of one pool.
pub struct BackendSeries {
    pool: String,
    backend: String,
    requests: StatusCounters,
    duration: Histogram,
    connection_errors: Counter,
    active: Gauge,
    healthy: State,
    /// The control worker's health verdict, read by the workers.
    verdict: &'static AtomicU64,
    drain: State,
    ejected: State,
    ejections: Counter,
    tcp: OnceLock<TcpSeries>,
    udp: OnceLock<UdpSeries>,
}

impl BackendSeries {
    pub fn new(pool: &str, backend: &str) -> Self {
        let labels = [pool, backend];
        BackendSeries {
            pool: pool.to_owned(),
            backend: backend.to_owned(),
            requests: StatusCounters::new(F::BACKEND_REQUESTS, pool, backend),
            duration: Histogram::of(F::BACKEND_DURATION, &labels),
            connection_errors: Counter::of(F::BACKEND_CONNECTION_ERRORS, &labels),
            active: Gauge(&cells(F::ACTIVE_CONNECTIONS, &labels)[0]),
            healthy: State(&cells(F::BACKEND_HEALTHY, &labels)[0]),
            verdict: &cells_in(prober_stripe(), F::BACKEND_HEALTHY, &labels)[0],
            drain: State(&cells(F::BACKEND_DRAIN_STATE, &labels)[0]),
            ejected: State(&cells(F::BACKEND_EJECTED, &labels)[0]),
            ejections: Counter::of(F::BACKEND_EJECTIONS, &labels),
            tcp: OnceLock::new(),
            udp: OnceLock::new(),
        }
    }

    pub fn record_request(&self, status: u16, secs: f64) {
        self.requests.inc(status);
        self.duration.observe(secs);
    }

    pub fn connection_error(&self) {
        self.connection_errors.inc();
    }

    pub fn connection_opened(&self) {
        self.active.add(1);
    }

    pub fn connection_closed(&self) {
        self.active.add(-1);
    }

    /// Control worker: the backend's health verdict.
    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.set(healthy as u64);
    }

    /// The control worker's health verdict; `None` before its first.
    pub fn verdict(&self) -> Option<bool> {
        match self.verdict.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v == 2),
        }
    }

    pub fn set_drain_state(&self, state: u8) {
        self.drain.set(state as u64);
    }

    pub fn set_ejected(&self, ejected: bool) {
        self.ejected.set(ejected as u64);
        if ejected {
            self.ejections.inc();
        }
    }

    /// The L4 TCP series, added on first use so HTTP pools do not show them.
    pub fn tcp(&self) -> &TcpSeries {
        self.tcp.get_or_init(|| {
            let labels = [self.pool.as_str(), self.backend.as_str()];
            TcpSeries {
                connections: Counter::of(F::TCP_CONNECTIONS, &labels),
                bytes_in: Counter::of(F::TCP_BYTES_IN, &labels),
                bytes_out: Counter::of(F::TCP_BYTES_OUT, &labels),
            }
        })
    }

    /// The UDP series, added on first use.
    pub fn udp(&self) -> &UdpSeries {
        self.udp.get_or_init(|| {
            let labels = [self.pool.as_str(), self.backend.as_str()];
            UdpSeries {
                flows: Counter::of(F::UDP_FLOWS, &labels),
                packets_in: Counter::of(F::UDP_PACKETS_IN, &labels),
                bytes_in: Counter::of(F::UDP_BYTES_IN, &labels),
                packets_out: Counter::of(F::UDP_PACKETS_OUT, &labels),
                bytes_out: Counter::of(F::UDP_BYTES_OUT, &labels),
            }
        })
    }
}

pub struct TcpSeries {
    connections: Counter,
    bytes_in: Counter,
    bytes_out: Counter,
}

impl TcpSeries {
    pub fn connection(&self) {
        self.connections.inc();
    }

    pub fn bytes(&self, bytes_in: u64, bytes_out: u64) {
        self.bytes_in.add(bytes_in);
        self.bytes_out.add(bytes_out);
    }
}

/// Counters of one UDP backend: a plain atomic add per datagram.
#[derive(Clone, Copy)]
pub struct UdpSeries {
    flows: Counter,
    packets_in: Counter,
    bytes_in: Counter,
    packets_out: Counter,
    bytes_out: Counter,
}

impl UdpSeries {
    pub fn flow(&self) {
        self.flows.inc();
    }

    pub fn packet_in(&self, bytes: usize) {
        self.packets_in.inc();
        self.bytes_in.add(bytes as u64);
    }

    pub fn packet_out(&self, bytes: usize) {
        self.packets_out.inc();
        self.bytes_out.add(bytes as u64);
    }
}

// Rare events: looked up through the cache

/// A cache hit served by `tier`: `memory` or `disk`.
pub fn record_cache_hit(tier: &'static str) {
    Counter::of(F::CACHE_HITS, &[tier]).inc();
}

pub fn record_lb_error(pool: &str, vhost: &str, reason: &str) {
    Counter::of(F::LB_ERRORS, &[pool, vhost, reason]).inc();
}

pub fn record_tcp_error(pool: &str, reason: &str) {
    Counter::of(F::TCP_ERRORS, &[pool, reason]).inc();
}

pub fn record_udp_error(pool: &str, reason: &str) {
    Counter::of(F::UDP_ERRORS, &[pool, reason]).inc();
}

pub fn record_proxy_protocol_error(listener: &str) {
    Counter::of(F::PROXY_PROTOCOL_ERRORS, &[listener]).inc();
}

// Reading

/// The node's metrics in the Prometheus text format: every stripe summed.
pub fn render() -> String {
    render_from(region())
}

fn render_from(region: &Region) -> String {
    let stripes = region.layout().stripes;
    // Family → labels → merged cells. Keys repeat only after a process died
    // while adding one; merging by key covers that too.
    let mut merged: BTreeMap<u8, BTreeMap<Vec<u8>, Vec<u64>>> = BTreeMap::new();
    for s in region.each_series() {
        let Some(family) = s.key.first().and_then(|f| FAMILIES.get(*f as usize)) else { continue };
        let values = (0..s.cells).map(|c| {
            let per_stripe = (0..stripes).map(|i| region.cell(i, s.first_cell + c).load(Ordering::Relaxed));
            match family.kind {
                Kind::Counter | Kind::Histogram | Kind::Gauge => per_stripe.fold(0u64, u64::wrapping_add),
                Kind::State(merge) => {
                    let set = per_stripe.filter(|v| *v != 0);
                    let v = match merge {
                        Merge::Min => set.min(),
                        Merge::Max => set.max(),
                    };
                    v.unwrap_or(0)
                }
            }
        });
        let slot = merged.entry(s.key[0]).or_default().entry(s.key[1..].to_vec()).or_insert_with(|| vec![0; s.cells as usize]);
        for (into, v) in slot.iter_mut().zip(values) {
            *into = match family.kind {
                Kind::State(Merge::Min) if *into != 0 && v != 0 => (*into).min(v),
                Kind::State(_) => (*into).max(v),
                _ => into.wrapping_add(v),
            };
        }
    }

    let mut out = String::new();
    for (id, series) in &merged {
        let family = &FAMILIES[*id as usize];
        let rows: Vec<_> = series.iter().filter(|(_, v)| !matches!(family.kind, Kind::State(_)) || v[0] != 0).collect();
        if rows.is_empty() {
            continue;
        }
        let kind = match family.kind {
            Kind::Counter => "counter",
            Kind::Histogram => "histogram",
            Kind::Gauge | Kind::State(_) => "gauge",
        };
        let _ = writeln!(out, "# HELP {} {}\n# TYPE {} {kind}", family.name, family.help, family.name);
        for (labels, v) in rows {
            let labels = label_text(family.labels, labels);
            match family.kind {
                Kind::Counter => sample(&mut out, family.name, "", &labels, v[0]),
                Kind::Gauge => sample(&mut out, family.name, "", &labels, v[0] as i64),
                Kind::State(_) => sample(&mut out, family.name, "", &labels, v[0] - 1),
                Kind::Histogram => {
                    let mut cumulative = 0;
                    for (i, le) in LATENCY_BUCKETS.iter().map(f64::to_string).chain(["+Inf".to_owned()]).enumerate() {
                        cumulative += v[i];
                        let sep = if labels.is_empty() { "" } else { "," };
                        let _ = writeln!(out, "{}_bucket{{{labels}{sep}le=\"{le}\"}} {cumulative}", family.name);
                    }
                    sample(&mut out, family.name, "_sum", &labels, v[HISTOGRAM_CELLS - 1] as f64 / 1e6);
                    sample(&mut out, family.name, "_count", &labels, cumulative);
                }
            }
        }
    }
    render_pool_backends(&mut out, &merged);
    render_globals(&mut out, region);
    out
}

/// States a backend counts in, by precedence.
const POOL_STATES: [&str; 4] = ["available", "unhealthy", "ejected", "draining"];

/// `keel_pool_backends{pool, state}`, from the merged per-backend states:
/// every backend not removed counts in exactly one state, `draining` before
/// `unhealthy` before `ejected`. A backend any worker ejected counts as
/// ejected, as `keel_backend_ejected` reads.
fn render_pool_backends(out: &mut String, merged: &BTreeMap<u8, BTreeMap<Vec<u8>, Vec<u64>>>) {
    // State cells hold value + 1; 0 is unset.
    let state = |family: F, key: &[u8]| merged.get(&(family as u8)).and_then(|m| m.get(key)).map_or(0, |v| v[0]);
    let mut pools: BTreeMap<&[u8], [u64; 4]> = BTreeMap::new();
    for key in merged.get(&(F::BACKEND_DRAIN_STATE as u8)).into_iter().flat_map(BTreeMap::keys) {
        let drain = state(F::BACKEND_DRAIN_STATE, key);
        if drain == 0 {
            continue;
        }
        let pool = key.split(|b| *b == 0).next().unwrap_or_default();
        let counts = pools.entry(pool).or_default();
        let slot = match (drain - 1, state(F::BACKEND_HEALTHY, key), state(F::BACKEND_EJECTED, key)) {
            (2, _, _) => continue,
            (1, _, _) => 3,
            (_, 1, _) => 1,
            (_, _, 2) => 2,
            _ => 0,
        };
        counts[slot] += 1;
    }
    if pools.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "# HELP keel_pool_backends Backends per pool by state: available, unhealthy, ejected, draining\n# TYPE keel_pool_backends gauge"
    );
    for (pool, counts) in pools {
        for (name, n) in POOL_STATES.iter().zip(counts) {
            let labels = format!("{},state=\"{name}\"", label_text(&["pool"], pool));
            sample(out, "keel_pool_backends", "", &labels, n);
        }
    }
}

fn render_globals(out: &mut String, region: &Region) {
    let g = |i: usize| region.global(i).load(Ordering::Relaxed);
    let _ = writeln!(
        out,
        "# HELP keel_worker_restarts_total Workers the master replaced, by how they ended\n\
         # TYPE keel_worker_restarts_total counter\n\
         keel_worker_restarts_total{{reason=\"exit\"}} {}\n\
         keel_worker_restarts_total{{reason=\"signal\"}} {}\n\
         # HELP keel_workers Worker processes running\n# TYPE keel_workers gauge\nkeel_workers {}\n\
         # HELP keel_workers_configured Worker processes configured (keel.workers)\n# TYPE keel_workers_configured gauge\nkeel_workers_configured {}\n\
         # HELP keel_workers_unresponsive Workers that did not answer the last liveness check\n# TYPE keel_workers_unresponsive gauge\nkeel_workers_unresponsive {}\n\
         # HELP keel_start_time_seconds When the master started, unix time; counters count from here\n# TYPE keel_start_time_seconds gauge\nkeel_start_time_seconds {}\n\
         # HELP keel_metrics_series_dropped_total Series not recorded because the shared memory region was full\n# TYPE keel_metrics_series_dropped_total counter\nkeel_metrics_series_dropped_total {}",
        g(G_RESTARTS_EXIT),
        g(G_RESTARTS_SIGNAL),
        g(G_WORKERS),
        g(G_WORKERS_CONFIGURED),
        g(G_UNRESPONSIVE),
        g(G_START),
        g(G_DROPPED),
    );
}

fn sample(out: &mut String, name: &str, suffix: &str, labels: &str, value: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "{name}{suffix} {value}");
    } else {
        let _ = writeln!(out, "{name}{suffix}{{{labels}}} {value}");
    }
}

fn label_text(names: &[&str], values: &[u8]) -> String {
    let mut out = String::new();
    for (i, (name, value)) in names.iter().zip(values.split(|b| *b == 0)).enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{name}=\"");
        for c in String::from_utf8_lossy(value).chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out
}

// Serving

/// `GET /metrics` on the listener the master bound, served by the control
/// worker.
pub struct MetricsServer {
    pub listener: std::net::TcpListener,
}

impl MetricsServer {
    pub async fn serve(self, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
        let listener = match self.listener.set_nonblocking(true).and_then(|()| tokio::net::TcpListener::from_std(self.listener)) {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "metrics: cannot serve the inherited listener");
                return;
            }
        };
        if let Ok(addr) = listener.local_addr() {
            info!(address = %addr, "metrics: serving");
        }
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                result = listener.accept() => match result {
                    Ok((stream, _)) => { tokio::spawn(answer(stream)); }
                    Err(e) => error!(error = %e, "metrics: accept error"),
                }
            }
        }
    }
}

async fn answer(mut stream: tokio::net::TcpStream) {
    // Only GET /metrics; anything else gets a 404.
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf).await {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let path = path.split('?').next().unwrap_or(path);
    let (status, kind, body) = if method == "GET" && path == "/metrics" {
        ("200 OK", "text/plain; version=0.0.4", render())
    } else {
        ("404 Not Found", "text/plain", "Not Found".to_owned())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(stripes: usize) -> Region {
        Region::create(Layout { stripes, cells: 256, slots: 64, arena: 4096 }).unwrap()
    }

    fn add(r: &Region, family: F, labels: &[&str], per_stripe: &[u64]) -> u32 {
        let first = r.series(&key(family, labels), FAMILIES[family as usize].kind.cells()).unwrap();
        for (stripe, v) in per_stripe.iter().enumerate() {
            r.cell(stripe, first).store(*v, Ordering::Relaxed);
        }
        first
    }

    fn line<'a>(text: &'a str, start: &str) -> Option<&'a str> {
        text.lines().find(|l| l.starts_with(start))
    }

    #[test]
    fn counters_and_gauges_are_summed_over_the_workers() {
        let r = region(3);
        add(&r, F::REQUESTS, &["web", "a.test", "200"], &[1, 2, 3]);
        add(&r, F::ACTIVE_CONNECTIONS, &["web", "10.0.0.1:80"], &[2, (-1i64) as u64, 0]);
        let text = render_from(&r);
        assert_eq!(line(&text, "keel_requests_total{"), Some(r#"keel_requests_total{pool="web",vhost="a.test",status="200"} 6"#));
        assert_eq!(line(&text, "keel_active_connections{"), Some(r#"keel_active_connections{pool="web",backend="10.0.0.1:80"} 1"#));
        assert!(text.contains("# TYPE keel_requests_total counter"));
    }

    #[test]
    fn a_state_is_the_worst_of_the_workers_that_report_it() {
        let r = region(3);
        // Stored as value + 1; 0 = this worker has not reported.
        add(&r, F::BACKEND_HEALTHY, &["web", "a:1"], &[2, 1, 0]);
        add(&r, F::BACKEND_EJECTED, &["web", "a:1"], &[1, 2, 0]);
        add(&r, F::BACKEND_DRAIN_STATE, &["web", "a:1"], &[0, 0, 0]);
        let text = render_from(&r);
        assert_eq!(line(&text, "keel_backend_healthy{"), Some(r#"keel_backend_healthy{pool="web",backend="a:1"} 0"#));
        assert_eq!(line(&text, "keel_backend_ejected{"), Some(r#"keel_backend_ejected{pool="web",backend="a:1"} 1"#));
        assert!(!text.contains("keel_backend_drain_state"), "no worker reported it");
    }

    /// `keel_pool_backends` of `pool` as [available, unhealthy, ejected, draining].
    fn pool_counts(r: &Region, pool: &str) -> Vec<u64> {
        let text = render_from(r);
        POOL_STATES
            .iter()
            .map(|s| {
                let l = line(&text, &format!("keel_pool_backends{{pool=\"{pool}\",state=\"{s}\"}}")).expect("emitted");
                l.rsplit(' ').next().unwrap().parse().unwrap()
            })
            .collect()
    }

    #[test]
    fn backends_count_in_one_state_each_by_precedence() {
        // Stripes 0 and 1 are workers, 2 the control worker's verdicts. States
        // are stored as value + 1.
        let r = region(3);
        let drain = add(&r, F::BACKEND_DRAIN_STATE, &["web", "a:1"], &[1, 1, 0]);
        let ejected = add(&r, F::BACKEND_EJECTED, &["web", "a:1"], &[1, 1, 0]);
        add(&r, F::BACKEND_DRAIN_STATE, &["web", "b:1"], &[1, 1, 0]);
        // A pool without a health check has no verdicts: available.
        assert_eq!(pool_counts(&r, "web"), [2, 0, 0, 0]);

        // Ejected by one worker only: ejected.
        r.cell(1, ejected).store(2, Ordering::Relaxed);
        assert_eq!(pool_counts(&r, "web"), [1, 0, 1, 0]);

        // Failing its check: unhealthy, before ejected.
        let verdict = add(&r, F::BACKEND_HEALTHY, &["web", "a:1"], &[0, 0, 1]);
        assert_eq!(pool_counts(&r, "web"), [1, 1, 0, 0]);

        // Draining in every worker: draining, before unhealthy.
        r.cell(0, drain).store(2, Ordering::Relaxed);
        r.cell(1, drain).store(2, Ordering::Relaxed);
        assert_eq!(pool_counts(&r, "web"), [1, 0, 0, 1]);

        // Removed in every worker: not counted, the pool still is.
        r.cell(0, drain).store(3, Ordering::Relaxed);
        r.cell(1, drain).store(3, Ordering::Relaxed);
        assert_eq!(pool_counts(&r, "web"), [1, 0, 0, 0]);

        // Recovered and re-admitted: available again.
        r.cell(0, drain).store(1, Ordering::Relaxed);
        r.cell(1, drain).store(1, Ordering::Relaxed);
        r.cell(2, verdict).store(2, Ordering::Relaxed);
        r.cell(1, ejected).store(1, Ordering::Relaxed);
        assert_eq!(pool_counts(&r, "web"), [2, 0, 0, 0]);
        // One worker still sends it new connections: not drained yet.
        r.cell(0, drain).store(2, Ordering::Relaxed);
        assert_eq!(pool_counts(&r, "web"), [2, 0, 0, 0]);
    }

    #[test]
    fn histograms_are_cumulative_with_sum_and_count() {
        let r = region(2);
        let first = add(&r, F::REQUEST_DURATION, &["web", "*"], &[]);
        // Stripe 0: one request in the first bucket; stripe 1: one above the last.
        r.cell(0, first).store(1, Ordering::Relaxed);
        r.cell(1, first + LATENCY_BUCKETS.len() as u32).store(1, Ordering::Relaxed);
        r.cell(0, first + HISTOGRAM_CELLS as u32 - 1).store(1_000, Ordering::Relaxed);
        r.cell(1, first + HISTOGRAM_CELLS as u32 - 1).store(20_000_000, Ordering::Relaxed);
        let text = render_from(&r);
        let p = r#"keel_request_duration_seconds"#;
        assert_eq!(line(&text, &format!(r#"{p}_bucket{{pool="web",vhost="*",le="0.005"}}"#)).unwrap().rsplit(' ').next(), Some("1"));
        assert_eq!(line(&text, &format!(r#"{p}_bucket{{pool="web",vhost="*",le="10"}}"#)).unwrap().rsplit(' ').next(), Some("1"));
        assert_eq!(line(&text, &format!(r#"{p}_bucket{{pool="web",vhost="*",le="+Inf"}}"#)).unwrap().rsplit(' ').next(), Some("2"));
        assert_eq!(line(&text, &format!("{p}_sum")), Some(r#"keel_request_duration_seconds_sum{pool="web",vhost="*"} 20.001"#));
        assert_eq!(line(&text, &format!("{p}_count")), Some(r#"keel_request_duration_seconds_count{pool="web",vhost="*"} 2"#));
    }

    #[test]
    fn label_values_are_escaped() {
        let r = region(1);
        add(&r, F::PROXY_PROTOCOL_ERRORS, &["a\"b\\c\nd"], &[1]);
        assert!(render_from(&r).contains(r#"keel_proxy_protocol_errors_total{listener="a\"b\\c\nd"} 1"#));
    }

    #[test]
    fn a_replacement_worker_clears_gauges_and_keeps_counters() {
        let r = region(2);
        let counter = add(&r, F::BACKEND_REQUESTS, &["web", "a:1", "200"], &[5, 7]);
        let gauge = add(&r, F::ACTIVE_CONNECTIONS, &["web", "a:1"], &[3, 4]);
        let state = add(&r, F::BACKEND_HEALTHY, &["web", "a:1"], &[1, 2]);
        reset_gauges(&r, 1);
        assert_eq!(r.cell(1, counter).load(Ordering::Relaxed), 7);
        assert_eq!(r.cell(1, gauge).load(Ordering::Relaxed), 0);
        assert_eq!(r.cell(1, state).load(Ordering::Relaxed), 0);
        assert_eq!(r.cell(0, gauge).load(Ordering::Relaxed), 3);
    }

    #[test]
    fn worker_health_is_always_exported() {
        let text = render_from(&region(1));
        for name in ["keel_worker_restarts_total{reason=\"signal\"} 0", "keel_workers 0", "keel_workers_unresponsive 0", "keel_metrics_series_dropped_total 0"] {
            assert!(text.contains(name), "{name} missing");
        }
    }

    #[test]
    fn handles_count_into_this_process_stripe() {
        let series = RequestSeries::new("handles-test", "h.test");
        series.record(201, 0.02, 10, 20);
        series.record(201, 0.02, 1, 2);
        let text = render();
        assert!(text.contains(r#"keel_requests_total{pool="handles-test",vhost="h.test",status="201"} 2"#));
        assert!(text.contains(r#"keel_request_bytes_out_total{pool="handles-test",vhost="h.test"} 22"#));
    }
}
