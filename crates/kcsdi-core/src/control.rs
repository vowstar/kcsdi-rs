// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Cooperative cancellation without sharing transport ownership.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{Error, Result};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One operation's cancellation flag. Create a fresh token for another operation.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn pause(&self, duration: Duration) -> Result<()> {
        let started = Instant::now();
        loop {
            self.check()?;
            let Some(remaining) = duration.checked_sub(started.elapsed()) else {
                return Ok(());
            };
            std::thread::sleep(remaining.min(POLL_INTERVAL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_but_new_operations_are_independent() {
        let token = CancellationToken::default();
        let worker = token.clone();
        token.cancel();
        assert!(worker.is_cancelled());
        assert!(matches!(
            worker.pause(Duration::from_secs(10)),
            Err(Error::Cancelled)
        ));
        assert!(!CancellationToken::default().is_cancelled());
    }
}
