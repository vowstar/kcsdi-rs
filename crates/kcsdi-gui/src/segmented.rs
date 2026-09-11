// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Sequential finite segments share one connection and one completion boundary.

use std::time::Instant;

use kcsdi_core::control::CancellationToken;
use kcsdi_core::data::SweepData;
use kcsdi_core::device::{Device, SweepProgress};
use kcsdi_core::segments::{SegmentJoiner, SegmentMetadata, SegmentPlan};
use kcsdi_core::transport::Transport;
use kcsdi_core::{Error, Result};

use crate::preview::SegmentProgress;

pub fn acquire<T: Transport>(
    device: &mut Device<T>,
    plan: &SegmentPlan,
    cancel: &CancellationToken,
    mut progress: impl FnMut(SweepProgress<'_>, SegmentProgress),
) -> Result<(SweepData, SegmentMetadata)> {
    let mut joiner = SegmentJoiner::new(plan.clone());
    for (index, segment) in plan.segments().iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut last_preview = None;
        let mut preview_error = None;
        let frame = segment
            .sweep(plan.settings())
            .acquire(device, cancel, |prefix| {
                if prefix.points.is_empty() {
                    preview_error = None;
                }
                if cancel.is_cancelled() || preview_error.is_some() {
                    return;
                }
                let force =
                    prefix.points.is_empty() || prefix.points.len() == segment.points() as usize;
                if !force
                    && last_preview.is_some_and(|last: Instant| last.elapsed().as_millis() < 50)
                {
                    return;
                }
                last_preview = Some(Instant::now());
                match joiner.preview(prefix.points) {
                    Ok(points) => progress(
                        SweepProgress {
                            mode: prefix.mode,
                            format: prefix.format,
                            points: &points,
                            expected_points: plan.acquired_points(),
                        },
                        SegmentProgress {
                            index,
                            count: plan.segments().len(),
                            acquired: joiner.acquired_points() + prefix.points.len() as u32,
                            expected: plan.acquired_points(),
                            segment_points: prefix.points.len(),
                        },
                    ),
                    Err(error) => preview_error = Some(error),
                }
            })?;
        // A bad preview may be replaced by a fresh frame header. The complete
        // collector result is authoritative and revalidated by append.
        joiner.append(frame)?;
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    joiner.finish()
}

#[cfg(test)]
pub(crate) mod tests;
