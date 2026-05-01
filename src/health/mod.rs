use crate::config::{HealthCheck, HealthCheckKind};
use pingora::lb::health_check::{HealthCheck as PingoraHealthCheck, HttpHealthCheck, TcpHealthCheck};
use std::time::Duration;

/// Build a Pingora health check from the pool config.
pub fn build(cfg: &HealthCheck, host: &str) -> Box<dyn PingoraHealthCheck + Send + Sync> {
    let timeout = parse_duration(&cfg.timeout);

    match cfg.kind {
        HealthCheckKind::Tcp => {
            let mut hc = TcpHealthCheck::default();
            hc.consecutive_success = cfg.healthy_threshold as usize;
            hc.consecutive_failure = cfg.unhealthy_threshold as usize;
            hc.peer_template.options.connection_timeout = Some(timeout);
            Box::new(hc)
        }
        HealthCheckKind::Http => {
            let mut hc = HttpHealthCheck::new(host, false);
            hc.req.set_uri(cfg.path.parse().unwrap_or_else(|_| "/health".parse().unwrap()));
            hc.consecutive_success = cfg.healthy_threshold as usize;
            hc.consecutive_failure = cfg.unhealthy_threshold as usize;
            hc.peer_template.options.connection_timeout = Some(timeout);
            hc.peer_template.options.read_timeout = Some(timeout);
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
