// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Session-local telemetry and monotonic freshness, separate from sweep data.

use std::time::{Duration, Instant};

use kcsdi_core::data::Voltage;

pub const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
pub const STALE_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub temperature: f64,
    pub voltage: Voltage,
    /// Query start, so freshness includes both query and delivery latency.
    pub observed_at: Instant,
}

#[derive(Debug, Default)]
pub struct HealthState {
    pub snapshot: Option<HealthSnapshot>,
    pub pending: bool,
    pub error: Option<String>,
    pub last_attempt: Option<Instant>,
}

impl HealthState {
    pub fn due(&self, now: Instant) -> bool {
        !self.pending
            && self
                .last_attempt
                .is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL)
    }

    pub fn begin(&mut self) -> bool {
        if self.pending {
            return false;
        }
        self.pending = true;
        true
    }

    pub fn succeed(&mut self, snapshot: HealthSnapshot, now: Instant) {
        self.snapshot = Some(snapshot);
        self.pending = false;
        self.error = None;
        self.last_attempt = Some(now);
    }

    pub fn fail(&mut self, message: String, now: Instant) {
        self.pending = false;
        self.error = Some(message);
        self.last_attempt = Some(now);
    }

    pub fn age(&self, now: Instant) -> Option<Duration> {
        self.snapshot
            .as_ref()
            .map(|snapshot| now.saturating_duration_since(snapshot.observed_at))
    }

    pub fn is_stale(&self, now: Instant) -> bool {
        self.snapshot.is_some()
            && (self.error.is_some() || self.age(now).is_some_and(|age| age >= STALE_AFTER))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn snapshot(temperature: f64, observed_at: Instant) -> HealthSnapshot {
        HealthSnapshot {
            temperature,
            voltage: Voltage {
                external: 12.0,
                battery: 8.0,
            },
            observed_at,
        }
    }

    #[test]
    fn one_pending_refresh_and_completion_based_cooldown() {
        let now = Instant::now();
        let mut health = HealthState::default();
        assert!(health.due(now));
        assert!(health.begin());
        assert!(!health.begin());
        assert!(!health.due(now + Duration::from_secs(120)));
        let completed = now + Duration::from_secs(120);
        health.succeed(snapshot(42.0, completed), completed);
        assert!(!health.due(completed));
        assert!(!health.due(completed + REFRESH_INTERVAL - Duration::from_nanos(1)));
        assert!(health.due(completed + REFRESH_INTERVAL));
    }

    #[test]
    fn pending_does_not_hide_age_or_make_a_reading_fresh() {
        let now = Instant::now();
        let mut health = HealthState::default();
        health.succeed(snapshot(42.0, now), now + Duration::from_secs(20));
        assert_eq!(
            health.age(now + Duration::from_secs(20)),
            Some(Duration::from_secs(20))
        );
        assert!(!health.is_stale(now + STALE_AFTER - Duration::from_nanos(1)));
        assert!(health.begin());
        assert!(health.is_stale(now + STALE_AFTER));
        assert!(health.is_stale(now + Duration::from_secs(120)));
    }

    #[test]
    fn failure_keeps_the_last_pair_stale_until_success() {
        let now = Instant::now();
        let mut health = HealthState::default();
        health.succeed(snapshot(42.0, now), now);
        health.begin();
        health.fail("Status query failed".into(), now);
        assert_eq!(health.snapshot.as_ref().unwrap().temperature, 42.0);
        assert!(health.is_stale(now));
        assert!(!health.due(now));
        assert!(health.begin());
        assert!(health.is_stale(now));
        health.succeed(snapshot(43.0, now), now);
        assert!(!health.is_stale(now));
        assert!(health.error.is_none());
        assert!(!health.pending);
    }

    #[test]
    fn missing_reading_is_unavailable_not_zero_or_stale() {
        let now = Instant::now();
        let mut health = HealthState::default();
        health.fail("No status response".into(), now);
        assert!(health.snapshot.is_none());
        assert!(health.age(now).is_none());
        assert!(!health.is_stale(now));
        assert!(!health.due(now));
        health = HealthState::default();
        assert!(health.due(now));
        assert!(health.error.is_none());
    }

    #[test]
    fn out_of_order_test_clock_saturates_without_panic() {
        let now = Instant::now();
        let mut health = HealthState::default();
        health.succeed(
            snapshot(42.0, now + Duration::from_secs(1)),
            now + Duration::from_secs(1),
        );
        assert_eq!(health.age(now), Some(Duration::ZERO));
        assert!(!health.due(now));
        assert!(!health.is_stale(now));
    }
}
