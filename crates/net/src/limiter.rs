//! Throttles failed authentication attempts per source address and globally.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Failures from one source that are forgiven before backoff starts (typos happen).
const FREE_FAILURES: u32 = 3;
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// A source's failure history is forgotten after this much quiet time.
const FORGET_AFTER: Duration = Duration::from_secs(3600);
const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
/// Failures from all sources within [`GLOBAL_WINDOW`] that lock out everyone.
const GLOBAL_FAILURE_LIMIT: usize = 20;
/// Bound on tracked sources so spoofed or rotating addresses cannot grow memory forever.
const MAX_TRACKED_SOURCES: usize = 4096;

#[derive(Debug)]
struct SourceRecord {
    failures: u32,
    locked_until: Instant,
    last_failure: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct AttemptLimiter {
    sources: HashMap<IpAddr, SourceRecord>,
    recent_failures: VecDeque<Instant>,
}

impl AttemptLimiter {
    /// Whether a new attempt from `address` may proceed at `now`.
    pub(crate) fn allows(&mut self, address: IpAddr, now: Instant) -> bool {
        self.expire(now);
        if self.recent_failures.len() >= GLOBAL_FAILURE_LIMIT {
            return false;
        }
        self.sources
            .get(&source_key(address))
            .is_none_or(|record| now >= record.locked_until)
    }

    pub(crate) fn record_failure(&mut self, address: IpAddr, now: Instant) {
        self.expire(now);
        self.recent_failures.push_back(now);
        if self.sources.len() >= MAX_TRACKED_SOURCES {
            // Evict the quietest source; the global window still bounds overall guessing.
            if let Some(oldest) = self
                .sources
                .iter()
                .min_by_key(|(_, record)| record.last_failure)
                .map(|(key, _)| *key)
            {
                self.sources.remove(&oldest);
            }
        }
        let record = self
            .sources
            .entry(source_key(address))
            .or_insert(SourceRecord {
                failures: 0,
                locked_until: now,
                last_failure: now,
            });
        record.failures = record.failures.saturating_add(1);
        record.last_failure = now;
        record.locked_until = now + backoff(record.failures);
    }

    pub(crate) fn record_success(&mut self, address: IpAddr) {
        self.sources.remove(&source_key(address));
    }

    fn expire(&mut self, now: Instant) {
        while self
            .recent_failures
            .front()
            .is_some_and(|failure| now.duration_since(*failure) >= GLOBAL_WINDOW)
        {
            self.recent_failures.pop_front();
        }
        self.sources
            .retain(|_, record| now.duration_since(record.last_failure) < FORGET_AFTER);
    }
}

fn backoff(failures: u32) -> Duration {
    if failures <= FREE_FAILURES {
        return Duration::ZERO;
    }
    let exponent = (failures - FREE_FAILURES - 1).min(16);
    Duration::from_secs(1u64 << exponent).min(MAX_BACKOFF)
}

/// IPv6 hosts usually own a whole /64, so throttle the prefix rather than single addresses.
fn source_key(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut segments = v6.segments();
            segments[4..].fill(0);
            IpAddr::V6(segments.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTACKER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7));
    const OTHER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 9));

    #[test]
    fn backoff_grows_after_free_failures_and_is_capped() {
        assert_eq!(backoff(3), Duration::ZERO);
        assert_eq!(backoff(4), Duration::from_secs(1));
        assert_eq!(backoff(6), Duration::from_secs(4));
        assert_eq!(backoff(40), MAX_BACKOFF);
    }

    #[test]
    fn repeated_failures_lock_out_the_source_only() {
        let mut limiter = AttemptLimiter::default();
        let start = Instant::now();
        for _ in 0..=FREE_FAILURES {
            assert!(limiter.allows(ATTACKER, start));
            limiter.record_failure(ATTACKER, start);
        }
        assert!(!limiter.allows(ATTACKER, start));
        assert!(!limiter.allows(ATTACKER, start + Duration::from_millis(900)));
        assert!(limiter.allows(OTHER, start));
        assert!(limiter.allows(ATTACKER, start + Duration::from_secs(3)));
    }

    #[test]
    fn success_clears_the_source_history() {
        let mut limiter = AttemptLimiter::default();
        let now = Instant::now();
        for _ in 0..6 {
            limiter.record_failure(ATTACKER, now);
        }
        limiter.record_success(ATTACKER);
        assert!(limiter.allows(ATTACKER, now));
    }

    #[test]
    fn many_sources_trigger_the_global_limit() {
        let mut limiter = AttemptLimiter::default();
        let now = Instant::now();
        for host in 0..GLOBAL_FAILURE_LIMIT {
            let address = IpAddr::V4(std::net::Ipv4Addr::new(
                10,
                0,
                0,
                u8::try_from(host).unwrap(),
            ));
            limiter.record_failure(address, now);
        }
        assert!(!limiter.allows(OTHER, now));
        assert!(limiter.allows(OTHER, now + GLOBAL_WINDOW));
    }

    #[test]
    fn ipv6_sources_are_grouped_by_prefix() {
        let first: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let second: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert_eq!(source_key(first), source_key(second));
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(source_key(mapped), ATTACKER);
    }
}
