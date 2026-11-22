//! Deterministic reconnect backoff and bounded local owner recovery budget.
//!
//! Mirrors `apps/shared/src/node/gateway-client.ts`:
//! - Exponential backoff: 1s -> 2s -> 4s -> 8s -> 16s -> 30s cap
//! - Owner recovery budget: at most 3 `--recover` attempts per 60s sliding window
//!   so a crash-looping gateway is never respawned without pause.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const RECONNECT_BASE: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);
pub const OWNER_RECOVERY_LIMIT: usize = 3;
pub const OWNER_RECOVERY_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    attempt: u32,
    recovery_timestamps: VecDeque<Instant>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            attempt: 0,
            recovery_timestamps: VecDeque::new(),
        }
    }
}

impl ReconnectPolicy {
    /// Reset backoff counter after a healthy connection is established.
    pub fn reset_backoff(&mut self) {
        self.attempt = 0;
    }

    /// Return the next reconnect delay (1s, 2s, 4s, 8s, 16s, 30s...).
    pub fn next_delay(&mut self) -> Duration {
        let shift = self.attempt.min(5);
        self.attempt = self.attempt.saturating_add(1);
        let secs = RECONNECT_BASE.as_secs().saturating_mul(1u64 << shift);
        Duration::from_secs(secs.min(RECONNECT_MAX.as_secs()))
    }

    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// Whether another `--recover` owner re-ensure is permitted within the 60s budget.
    pub fn allow_owner_recovery(&mut self, now: Instant) -> bool {
        while let Some(&front) = self.recovery_timestamps.front() {
            if now.duration_since(front) > OWNER_RECOVERY_WINDOW {
                self.recovery_timestamps.pop_front();
            } else {
                break;
            }
        }
        if self.recovery_timestamps.len() >= OWNER_RECOVERY_LIMIT {
            return false;
        }
        self.recovery_timestamps.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_caps_at_30s() {
        let mut p = ReconnectPolicy::default();
        assert_eq!(p.next_delay(), Duration::from_secs(1));
        assert_eq!(p.next_delay(), Duration::from_secs(2));
        assert_eq!(p.next_delay(), Duration::from_secs(4));
        assert_eq!(p.next_delay(), Duration::from_secs(8));
        assert_eq!(p.next_delay(), Duration::from_secs(16));
        assert_eq!(p.next_delay(), Duration::from_secs(30));
        assert_eq!(p.next_delay(), Duration::from_secs(30));
        p.reset_backoff();
        assert_eq!(p.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn owner_recovery_enforces_sliding_window_budget() {
        let mut p = ReconnectPolicy::default();
        let t0 = Instant::now();
        assert!(p.allow_owner_recovery(t0));
        assert!(p.allow_owner_recovery(t0 + Duration::from_secs(5)));
        assert!(p.allow_owner_recovery(t0 + Duration::from_secs(10)));
        // 4th attempt inside 60s window is refused
        assert!(!p.allow_owner_recovery(t0 + Duration::from_secs(20)));
        // Once the first timestamp expires (>60s), recovery is permitted again
        assert!(p.allow_owner_recovery(t0 + Duration::from_secs(61)));
    }
}
