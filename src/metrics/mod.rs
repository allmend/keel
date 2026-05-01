use async_trait::async_trait;
use once_cell::sync::Lazy;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use prometheus::{
    gather, register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec,
    Encoder, GaugeVec, HistogramVec, TextEncoder,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{error, info};

// METRIC HANDLES

const LATENCY_BUCKETS: &[f64] =
    &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

// Request-level

pub static REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "keel_requests_total",
        "Total HTTP requests proxied",
        &["pool", "vhost", "status"]
    )
    .expect("register keel_requests_total")
});

pub static REQUEST_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "keel_request_duration_seconds",
        "End-to-end request duration in seconds",
        &["pool", "vhost"],
        LATENCY_BUCKETS.to_vec()
    )
    .expect("register keel_request_duration_seconds")
});

pub static REQUEST_BYTES_IN: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "keel_request_bytes_in_total",
        "Total bytes received from clients",
        &["pool", "vhost"]
    )
    .expect("register keel_request_bytes_in_total")
});

pub static REQUEST_BYTES_OUT: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "keel_request_bytes_out_total",
        "Total bytes sent to clients",
        &["pool", "vhost"]
    )
    .expect("register keel_request_bytes_out_total")
});

pub static LB_ERRORS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "keel_lb_errors_total",
        "Load-balancer-originated errors before a backend was reached",
        &["pool", "vhost", "reason"]
    )
    .expect("register keel_lb_errors_total")
});

// Backend-level

pub static BACKEND_REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "keel_backend_requests_total",
        "Total HTTP requests forwarded per backend",
        &["pool", "backend", "status"]
    )
    .expect("register keel_backend_requests_total")
});

pub static BACKEND_RESPONSE_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "keel_backend_response_duration_seconds",
        "Time from backend selected to response complete",
        &["pool", "backend"],
        LATENCY_BUCKETS.to_vec()
    )
    .expect("register keel_backend_response_duration_seconds")
});

pub static BACKEND_CONNECTION_ERRORS: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        "keel_backend_connection_errors_total",
        "Failed connections to backends",
        &["pool", "backend"]
    )
    .expect("register keel_backend_connection_errors_total")
});

pub static ACTIVE_CONNECTIONS: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        "keel_active_connections",
        "Current active upstream connections per backend",
        &["pool", "backend"]
    )
    .expect("register keel_active_connections")
});

pub static BACKEND_HEALTHY: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        "keel_backend_healthy",
        "Backend health state (1 = healthy, 0 = unhealthy)",
        &["pool", "backend"]
    )
    .expect("register keel_backend_healthy")
});

pub static BACKEND_DRAIN_STATE: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        "keel_backend_drain_state",
        "Backend drain state (0 = active, 1 = draining, 2 = removed)",
        &["pool", "backend"]
    )
    .expect("register keel_backend_drain_state")
});

// HELPER FUNCTIONS

pub fn record_request(pool: &str, vhost: &str, status: u16, duration_secs: f64) {
    let s = status.to_string();
    REQUESTS_TOTAL.with_label_values(&[pool, vhost, &s]).inc();
    REQUEST_DURATION.with_label_values(&[pool, vhost]).observe(duration_secs);
}

pub fn record_backend_request(pool: &str, backend: &str, status: u16, duration_secs: f64) {
    let s = status.to_string();
    BACKEND_REQUESTS_TOTAL.with_label_values(&[pool, backend, &s]).inc();
    BACKEND_RESPONSE_DURATION.with_label_values(&[pool, backend]).observe(duration_secs);
}

pub fn add_request_bytes_in(pool: &str, vhost: &str, bytes: usize) {
    REQUEST_BYTES_IN.with_label_values(&[pool, vhost]).inc_by(bytes as f64);
}

pub fn add_request_bytes_out(pool: &str, vhost: &str, bytes: usize) {
    REQUEST_BYTES_OUT.with_label_values(&[pool, vhost]).inc_by(bytes as f64);
}

pub fn record_lb_error(pool: &str, vhost: &str, reason: &str) {
    LB_ERRORS_TOTAL.with_label_values(&[pool, vhost, reason]).inc();
}

pub fn record_backend_connection_error(pool: &str, backend: &str) {
    BACKEND_CONNECTION_ERRORS.with_label_values(&[pool, backend]).inc();
}

#[allow(dead_code)]
pub fn set_backend_healthy(pool: &str, backend: &str, healthy: bool) {
    BACKEND_HEALTHY
        .with_label_values(&[pool, backend])
        .set(if healthy { 1.0 } else { 0.0 });
}

pub fn set_active_connections(pool: &str, backend: &str, count: f64) {
    ACTIVE_CONNECTIONS.with_label_values(&[pool, backend]).set(count);
}

pub fn set_drain_state(pool: &str, backend: &str, state: u8) {
    BACKEND_DRAIN_STATE.with_label_values(&[pool, backend]).set(state as f64);
}

// METRICS HTTP SERVICE

/// Lightweight HTTP service that serves Prometheus metrics at GET /metrics.
pub struct MetricsService {
    address: String,
}

impl MetricsService {
    pub fn new(address: &str) -> Self {
        MetricsService { address: address.to_owned() }
    }
}

#[async_trait]
impl BackgroundService for MetricsService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let listener = match TcpListener::bind(&self.address).await {
            Ok(l) => {
                info!(address = self.address, "metrics: listening");
                l
            }
            Err(e) => {
                error!(address = self.address, error = %e, "metrics: failed to bind");
                return;
            }
        };

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                result = listener.accept() => {
                    match result {
                        Ok((mut stream, _)) => {
                            tokio::spawn(async move {
                                let encoder = TextEncoder::new();
                                let families = gather();
                                let body = encoder
                                    .encode_to_string(&families)
                                    .unwrap_or_default();
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
                                    encoder.format_type(),
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            });
                        }
                        Err(e) => error!(error = %e, "metrics: accept error"),
                    }
                }
            }
        }
    }
}
