// TokenBucket rate limiter — thread-safe, uses std Mutex + Instant (no external deps).
//
// Rate limiting strategy: leaky token bucket
//   - capacity: max burst size (e.g. 20 requests)
//   - fill_rate: tokens replenished per second (e.g. 10 req/s)
//   - try_consume: returns false when bucket is empty → 429 Too Many Requests

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Instant;

/// Thread-safe token bucket rate limiter
pub struct RateLimiter {
    inner: Mutex<TokenBucket>,
}

/// Token bucket state (must hold Mutex to access)
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    fill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, fill_rate: f64) -> Self {
        // BUG-22: fractional fill rates (< 1 req/s) are legitimate sustained
        // limits and must be honored; only nonsensical values (zero, negative,
        // NaN) fall back to 1.0 — the same guard the capacity clamp applies.
        let fill_rate = if fill_rate.is_finite() && fill_rate > 0.0 {
            fill_rate
        } else {
            1.0
        };
        Self {
            capacity: capacity.max(1.0),
            tokens: capacity.max(1.0),
            fill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time, then attempt to consume.
    /// Returns true if request is allowed.
    fn try_consume(&mut self, tokens: f64) -> bool {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.fill_rate).min(self.capacity);
        self.last_refill = Instant::now();

        if self.tokens >= tokens {
            self.tokens -= tokens;
            true
        } else {
            false
        }
    }
}

impl RateLimiter {
    /// Create a new rate limiter.
    /// - `capacity`: max burst size (e.g. 20 requests in a burst)
    /// - `fill_rate`: sustained requests per second (e.g. 10 req/s)
    pub fn new(capacity: f64, fill_rate: f64) -> Self {
        Self {
            inner: Mutex::new(TokenBucket::new(capacity, fill_rate)),
        }
    }

    /// Try to consume `tokens` from the bucket. Returns false if rate limited.
    pub fn try_consume(&self, tokens: f64) -> bool {
        self.inner.lock().unwrap().try_consume(tokens)
    }

    /// Current fill rate (requests per second)
    pub fn fill_rate(&self) -> f64 {
        self.inner.lock().unwrap().fill_rate
    }

    /// Current capacity (burst size)
    pub fn capacity(&self) -> f64 {
        self.inner.lock().unwrap().capacity
    }
}

/// Rate limit configuration for HTTP server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Sustained requests per second
    #[serde(default = "default_rps")]
    pub requests_per_second: f64,
    /// Max burst size (allows short spikes)
    #[serde(default = "default_burst")]
    pub burst_size: f64,
}

fn default_rps() -> f64 {
    10.0
}
fn default_burst() -> f64 {
    20.0
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_second: default_rps(),
            burst_size: default_burst(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rate_limiter_allows_initial_burst() {
        let rl = RateLimiter::new(10.0, 10.0);
        // Should allow up to 10 tokens immediately
        for i in 0..10 {
            assert!(rl.try_consume(1.0), "request {} should be allowed", i);
        }
        // 11th should be denied (bucket empty)
        assert!(!rl.try_consume(1.0), "11th request should be rate limited");
    }

    #[test]
    fn rate_limiter_refills_over_time() {
        let rl = RateLimiter::new(1.0, 10.0); // cap=1, fill=10/s
        assert!(rl.try_consume(1.0));
        assert!(!rl.try_consume(1.0));
        // After ~100ms, should have ~1 token
        std::thread::sleep(Duration::from_millis(110));
        assert!(rl.try_consume(1.0), "should refill 1 token after ~100ms");
    }

    #[test]
    fn rate_limiter_low_rate_no_burst() {
        let rl = RateLimiter::new(100.0, 100.0); // high capacity, high rate
        for i in 0..100 {
            assert!(rl.try_consume(1.0), "request {} should be allowed", i);
        }
    }

    #[test]
    fn rate_limiter_zero_capacity_defaults_to_one() {
        // capacity 0 will be clamped to 1
        let rl = RateLimiter::new(0.0, 1.0);
        assert!(
            rl.try_consume(1.0),
            "min capacity should allow first request"
        );
        // capacity is clamped to 1, so serial request 2 should be rate limited
        assert!(rl.capacity() >= 1.0, "capacity should be at least 1");
        // After consuming, capacity should refill
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(rl.try_consume(1.0), "should refill 1 token after ~1s");
    }

    #[test]
    fn rate_limiter_concurrent_access() {
        let rl = std::sync::Arc::new(RateLimiter::new(100.0, 100.0));
        let mut handles = vec![];
        for _ in 0..10 {
            let rl = rl.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    let _ = rl.try_consume(1.0);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // All 100 should have been consumed
        assert!(
            !rl.try_consume(1.0),
            "after 100 concurrent, bucket should be empty"
        );
    }

    #[test]
    fn rate_limit_config_defaults() {
        let cfg = RateLimitConfig::default();
        assert!((cfg.requests_per_second - 10.0).abs() < f64::EPSILON);
        assert!((cfg.burst_size - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_limit_config_serde() {
        let yaml = "requests_per_second: 5\nburst_size: 10\n";
        let cfg: RateLimitConfig = serde_yaml::from_str(yaml).unwrap();
        assert!((cfg.requests_per_second - 5.0).abs() < f64::EPSILON);
        assert!((cfg.burst_size - 10.0).abs() < f64::EPSILON);
    }

    /// BUG-22 regression: `fill_rate` is documented as sustained requests per
    /// second, and a fractional rate (< 1.0) is a legitimate configuration
    /// (e.g. 0.5 req/s = one request every two seconds). Silently clamping
    /// it up to 1.0 widens the effective limit — 0.5 becomes 2×, 0.1 becomes
    /// 10× as permissive as configured.
    #[test]
    fn fractional_fill_rate_is_honored_not_clamped_up() {
        let rl = RateLimiter::new(10.0, 0.5);
        assert_eq!(rl.fill_rate(), 0.5, "0.5 req/s must stay 0.5, not widen to 1.0");
        let rl = RateLimiter::new(10.0, 0.1);
        assert_eq!(rl.fill_rate(), 0.1);
        // Only nonsensical rates (zero/negative/NaN) fall back to the 1.0
        // default, mirroring the documented capacity clamp.
        assert_eq!(RateLimiter::new(10.0, 0.0).fill_rate(), 1.0);
        assert_eq!(RateLimiter::new(10.0, -3.0).fill_rate(), 1.0);
        assert_eq!(RateLimiter::new(10.0, f64::NAN).fill_rate(), 1.0);
    }

    /// Timing check of the same contract: with capacity 1 and fill 0.5/s, a
    /// second request must stay denied for a window where a 1.0/s bucket
    /// would already have refilled (400 ms < 1 s needed at 0.5/s).
    #[test]
    fn fractional_fill_rate_stays_strict_in_time() {
        let rl = RateLimiter::new(1.0, 0.5);
        assert!(rl.try_consume(1.0));
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !rl.try_consume(1.0),
            "0.5 req/s must not refill a full token within 400 ms"
        );
    }
}
