// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Latest-only delivery for incomplete measurement previews.
//!
//! Completed measurements and control events use their reliable channel.
//! This mailbox holds at most one cumulative prefix for the current worker.
//! Callers reject obsolete session, request and cycle identities before display.

use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use kcsdi_core::data::SweepData;

/// An incomplete sweep, even when all expected rows have arrived before its end.
#[derive(Debug)]
pub struct PreviewEnvelope {
    pub session_id: u64,
    pub request_id: u64,
    pub cycle_id: u64,
    pub data: SweepData,
    pub expected_points: u32,
}

/// Shared latest-preview slot. Lock contention never delays the producer.
#[derive(Debug, Clone, Default)]
pub struct PreviewMailbox {
    latest: Arc<Mutex<Option<PreviewEnvelope>>>,
}

impl PreviewMailbox {
    /// Replace an unread preview, or discard this update if the slot is busy.
    pub fn publish(&self, preview: PreviewEnvelope) {
        let Some(mut slot) = self.try_slot() else {
            return;
        };
        let previous = slot.replace(preview);
        drop(slot);
        // Release the old measurement outside the shared critical section.
        drop(previous);
    }

    /// Consume the latest preview. A busy or empty slot returns None.
    pub fn take(&self) -> Option<PreviewEnvelope> {
        self.try_slot()?.take()
    }

    /// Remove a pending preview if the slot is available.
    /// Identity checks still reject stale updates when clearing races a producer.
    pub fn clear(&self) {
        drop(self.take());
    }

    fn try_slot(&self) -> Option<MutexGuard<'_, Option<PreviewEnvelope>>> {
        match self.latest.try_lock() {
            Ok(slot) => Some(slot),
            Err(TryLockError::WouldBlock) => None,
            // Replacing one owned Option has no partially updated invariants.
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcsdi_core::data::SweepPoint;
    use kcsdi_core::protocol::StreamMode;
    use std::sync::mpsc;
    use std::time::Duration;

    fn preview(cycle_id: u64, rows: usize) -> PreviewEnvelope {
        PreviewEnvelope {
            session_id: 1,
            request_id: 2,
            cycle_id,
            data: SweepData {
                mode: StreamMode::S11,
                format: "z".into(),
                points: (0..rows)
                    .map(|index| SweepPoint {
                        freq_hz: 5_000.0 + index as f64 * 1_000.0,
                        values: vec![50.0, 50.0, 0.0],
                    })
                    .collect(),
            },
            expected_points: 3,
        }
    }

    #[test]
    fn a_new_prefix_replaces_the_unread_prefix() {
        let mailbox = PreviewMailbox::default();
        mailbox.publish(preview(1, 1));
        mailbox.publish(preview(1, 2));
        let received = mailbox.take().unwrap();
        assert_eq!(
            (received.session_id, received.request_id, received.cycle_id),
            (1, 2, 1)
        );
        assert_eq!(received.expected_points, 3);
        assert_eq!(received.data.points.len(), 2);
        assert!(mailbox.take().is_none());
    }

    #[test]
    fn many_publications_retain_only_the_latest_snapshot() {
        let mailbox = PreviewMailbox::default();
        for cycle_id in 1..=10_000 {
            mailbox.publish(preview(cycle_id, 3));
        }
        assert_eq!(mailbox.take().unwrap().cycle_id, 10_000);
        assert!(mailbox.take().is_none());
    }

    #[test]
    fn clones_share_one_slot_and_clear_removes_it() {
        let mailbox = PreviewMailbox::default();
        let producer = mailbox.clone();
        producer.publish(preview(1, 1));
        assert_eq!(mailbox.take().unwrap().cycle_id, 1);
        assert!(producer.take().is_none());
        producer.publish(preview(2, 2));
        mailbox.clear();
        assert!(producer.take().is_none());
    }

    #[test]
    fn contention_drops_preview_updates_without_blocking() {
        let mailbox = PreviewMailbox::default();
        mailbox.publish(preview(1, 1));
        let producer = mailbox.clone();
        let guard = mailbox.latest.lock().unwrap();
        let (finished, result) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            producer.publish(preview(2, 2));
            let unavailable = producer.take().is_none();
            producer.clear();
            finished.send(unavailable).unwrap();
        });
        let completed = result.recv_timeout(Duration::from_secs(5));
        drop(guard);
        thread.join().unwrap();
        assert!(completed.unwrap());
        assert_eq!(mailbox.take().unwrap().cycle_id, 1);
    }

    #[test]
    fn poisoned_slots_are_recovered_without_panicking() {
        let mailbox = PreviewMailbox::default();
        mailbox.publish(preview(1, 1));
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = mailbox.latest.lock().unwrap();
            panic!("intentional preview fixture poisoning");
        });
        assert!(poisoned.is_err());
        assert!(mailbox.latest.is_poisoned());
        assert_eq!(mailbox.take().unwrap().cycle_id, 1);
        mailbox.publish(preview(2, 2));
        assert_eq!(mailbox.take().unwrap().cycle_id, 2);
        mailbox.publish(preview(3, 3));
        mailbox.clear();
        assert!(mailbox.take().is_none());
    }
}
