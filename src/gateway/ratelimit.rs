//! Token-bucket rate limiter keyed by (rule, client IP).
//!
//! A bucket holds up to `burst` tokens and refills at `rate` tokens per
//! second; a request takes one. Buckets are created on first use and swept
//! after they have been idle a while, so the map stays bounded by the number
//! of recently active clients.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limit {
    /// Sustained tokens per second.
    pub rate: f64,
    /// Bucket capacity: the largest burst allowed after idle time.
    pub burst: f64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<(String, IpAddr), Bucket>>,
}

impl RateLimiter {
    /// Take one token for `ip` under `rule`. `Err` carries how long until a
    /// token is available, for `Retry-After`.
    pub fn check(&self, rule: &str, ip: IpAddr, limit: Limit, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets
            .entry((rule.to_owned(), ip))
            .or_insert(Bucket { tokens: limit.burst, last: now });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * limit.rate).min(limit.burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / limit.rate;
            Err(Duration::from_secs_f64(wait))
        }
    }

    /// Drop buckets idle for longer than `idle`. Returns how many remain.
    pub fn sweep(&self, now: Instant, idle: Duration) -> usize {
        let mut buckets = self.buckets.lock().unwrap();
        buckets.retain(|_, b| now.saturating_duration_since(b.last) < idle);
        buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 1));

    #[test]
    fn burst_then_refill() {
        let l = RateLimiter::default();
        let limit = Limit { rate: 2.0, burst: 3.0 };
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(l.check("r", IP, limit, t0).is_ok());
        }
        let wait = l.check("r", IP, limit, t0).unwrap_err();
        assert!((wait.as_secs_f64() - 0.5).abs() < 1e-6, "one token at 2/s is 0.5s away");
        // After 0.5s exactly one token is back.
        assert!(l.check("r", IP, limit, t0 + Duration::from_millis(500)).is_ok());
        assert!(l.check("r", IP, limit, t0 + Duration::from_millis(500)).is_err());
        // Long idle refills to the burst, never beyond.
        for _ in 0..3 {
            assert!(l.check("r", IP, limit, t0 + Duration::from_secs(60)).is_ok());
        }
        assert!(l.check("r", IP, limit, t0 + Duration::from_secs(60)).is_err());
    }

    #[test]
    fn rules_and_clients_are_independent() {
        let l = RateLimiter::default();
        let limit = Limit { rate: 1.0, burst: 1.0 };
        let t0 = Instant::now();
        assert!(l.check("a", IP, limit, t0).is_ok());
        assert!(l.check("a", IP, limit, t0).is_err());
        assert!(l.check("b", IP, limit, t0).is_ok(), "other rule");
        let other = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 2));
        assert!(l.check("a", other, limit, t0).is_ok(), "other client");
    }

    #[test]
    fn sweep_drops_idle_buckets() {
        let l = RateLimiter::default();
        let limit = Limit { rate: 1.0, burst: 1.0 };
        let t0 = Instant::now();
        l.check("a", IP, limit, t0).ok();
        assert_eq!(l.sweep(t0 + Duration::from_secs(1), Duration::from_secs(10)), 1);
        assert_eq!(l.sweep(t0 + Duration::from_secs(11), Duration::from_secs(10)), 0);
    }
}
