use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct RefusalLimiter {
    limit: u32,
    window: Duration,
    started: Option<Instant>,
    count: u32,
}

impl RefusalLimiter {
    pub(crate) fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            started: None,
            count: 0,
        }
    }

    /// Records a refusal at `now` and says whether it may be answered and logged.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        match self.started {
            Some(started) if now.saturating_duration_since(started) < self.window => {}
            _ => {
                self.started = Some(now);
                self.count = 0;
            }
        }
        self.count = self.count.saturating_add(1);
        self.count <= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    #[test]
    fn refusals_past_the_limit_go_unanswered_until_the_window_ends() {
        let start = Instant::now();
        let mut limiter = RefusalLimiter::new(3, WINDOW);
        let answered: Vec<bool> = (0..5)
            .map(|second| limiter.allow(start + Duration::from_secs(second)))
            .collect();
        assert_eq!(answered, [true, true, true, false, false]);
        assert!(!limiter.allow(start + Duration::from_millis(59_999)));
        assert!(limiter.allow(start + WINDOW));
        assert!(limiter.allow(start + WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn a_quiet_service_answers_every_refusal() {
        let start = Instant::now();
        let mut limiter = RefusalLimiter::new(1, WINDOW);
        for minute in 0..5 {
            assert!(limiter.allow(start + WINDOW * minute));
        }
    }
}
