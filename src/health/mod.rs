use crate::config::{HealthCheck, HealthCheckKind};
use async_trait::async_trait;
use pingora::lb::health_check::{
    HealthCheck as PingoraHealthCheck, HealthObserve, HttpHealthCheck, TcpHealthCheck,
};
use pingora::lb::Backend;
use std::time::Duration;

/// Logs health transitions and keeps the `keel_backend_healthy` metric current.
struct HealthObserver {
    pool: String,
}

#[async_trait]
impl HealthObserve for HealthObserver {
    async fn observe(&self, target: &Backend, healthy: bool) {
        let addr = target.addr.to_string();
        crate::metrics::set_backend_healthy(&self.pool, &addr, healthy);
        if healthy {
            tracing::info!(pool = self.pool, backend = addr, "health: backend healthy");
        } else {
            tracing::warn!(pool = self.pool, backend = addr, "health: backend unhealthy");
        }
    }
}

/// Build a Pingora health check from the pool config.
pub fn build(cfg: &HealthCheck, host: &str, pool: &str) -> Box<dyn PingoraHealthCheck + Send + Sync> {
    let timeout = parse_duration(&cfg.timeout);
    let observer = Box::new(HealthObserver { pool: pool.to_owned() });

    match cfg.kind {
        HealthCheckKind::Tcp => {
            let mut hc = TcpHealthCheck::default();
            hc.consecutive_success = cfg.healthy_threshold as usize;
            hc.consecutive_failure = cfg.unhealthy_threshold as usize;
            hc.peer_template.options.connection_timeout = Some(timeout);
            hc.health_changed_callback = Some(observer);
            Box::new(hc)
        }
        HealthCheckKind::Http => {
            let mut hc = HttpHealthCheck::new(host, false);
            hc.req.set_uri(cfg.path.parse().unwrap_or_else(|_| "/health".parse().unwrap()));
            hc.consecutive_success = cfg.healthy_threshold as usize;
            hc.consecutive_failure = cfg.unhealthy_threshold as usize;
            hc.peer_template.options.connection_timeout = Some(timeout);
            hc.peer_template.options.read_timeout = Some(timeout);
            hc.health_changed_callback = Some(observer);
            Box::new(hc)
        }
    }
}

/// Parse a human duration string like "10s", "500ms", "2m" into `Duration`.
pub fn parse_duration(s: &str) -> Duration {
    if let Some(ms) = s.strip_suffix("ms") {
        Duration::from_millis(ms.trim().parse().unwrap_or(1000))
    } else if let Some(m) = s.strip_suffix('m') {
        Duration::from_secs(m.trim().parse::<u64>().unwrap_or(1) * 60)
    } else if let Some(sec) = s.strip_suffix('s') {
        Duration::from_secs(sec.trim().parse().unwrap_or(10))
    } else {
        Duration::from_secs(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_durations() {
        assert_eq!(parse_duration("10s"), Duration::from_secs(10));
        assert_eq!(parse_duration("500ms"), Duration::from_millis(500));
        assert_eq!(parse_duration("2m"), Duration::from_secs(120));
        assert_eq!(parse_duration("unknown"), Duration::from_secs(10));
    }
}
