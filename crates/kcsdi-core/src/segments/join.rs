// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

use crate::data::{SweepData, SweepPoint};
use crate::{Error, Result, table};

use super::{PlannedSegment, SegmentPlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleOrigin {
    pub segment: usize,
    pub point: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentMetadata {
    pub origins: Vec<SampleOrigin>,
    pub received: Vec<u32>,
}

impl SegmentMetadata {
    /// Recheck a frozen joined result before delivery or export.
    pub fn validate(&self, plan: &SegmentPlan, data: &SweepData) -> Result<()> {
        let invalid = || Error::Protocol("invalid segmented result or sample provenance".into());
        if self.received.len() != plan.segments().len()
            || self.origins.len() != data.points.len()
            || data.mode != plan.settings().mode()
            || data.format != plan.settings().format()
            || data
                .points
                .windows(2)
                .any(|pair| pair[0].freq_hz >= pair[1].freq_hz)
        {
            return Err(invalid());
        }
        let width = table::columns(data.mode, &data.format)
            .ok_or_else(invalid)?
            .len();
        let mut next = vec![None; self.received.len()];
        for (origin, point) in self.origins.iter().zip(&data.points) {
            let row = plan.segments().get(origin.segment).ok_or_else(invalid)?;
            if !point.freq_hz.is_finite()
                || point.freq_hz < 0.0
                || point.values.len() != width
                || origin.point >= row.points as usize
                || next[origin.segment]
                    .map_or(origin.point > usize::from(origin.segment > 0), |expected| {
                        origin.point != expected
                    })
                || (origin.point == 0
                    && !endpoint_matches(point.freq_hz, row.definition.start_hz, row))
                || (origin.point + 1 == row.points as usize
                    && !endpoint_matches(point.freq_hz, row.definition.stop_hz, row))
            {
                return Err(invalid());
            }
            next[origin.segment] = Some(origin.point + 1);
        }
        for (index, row) in plan.segments().iter().enumerate() {
            if self.received[index] != row.points || next[index] != Some(row.points as usize) {
                return Err(invalid());
            }
        }
        Ok(())
    }

    pub fn retained(&self, segment: usize) -> usize {
        self.origins
            .iter()
            .filter(|origin| origin.segment == segment)
            .count()
    }
}

/// A bounded builder. Only finish can expose an entire completed acquisition.
pub struct SegmentJoiner {
    plan: SegmentPlan,
    data: SweepData,
    metadata: SegmentMetadata,
}

impl SegmentJoiner {
    pub fn new(plan: SegmentPlan) -> Self {
        Self {
            data: SweepData {
                mode: plan.settings().mode(),
                format: plan.settings().format().into(),
                points: Vec::with_capacity(plan.acquired_points() as usize),
            },
            plan,
            metadata: SegmentMetadata::default(),
        }
    }

    pub fn acquired_points(&self) -> u32 {
        self.metadata.received.iter().sum()
    }

    /// Replaceable preview. Does not advance the accepted segment index.
    pub fn preview(&self, points: &[SweepPoint]) -> Result<Vec<SweepPoint>> {
        self.validate_rows(points, false)?;
        let mut joined = self.data.points.clone();
        let join = join_kind(&joined, points)?;
        append_rows(&mut joined, points, join);
        Ok(joined)
    }

    /// Validate everything before mutating the accepted prefix.
    pub fn append(&mut self, frame: SweepData) -> Result<()> {
        if frame.mode != self.data.mode || frame.format != self.data.format {
            return Err(self.error("unexpected measurement format"));
        }
        self.validate_rows(&frame.points, true)?;
        let join = join_kind(&self.data.points, &frame.points)?;
        let segment = self.metadata.received.len();
        let origins: Vec<_> = (0..frame.points.len())
            .map(|point| SampleOrigin { segment, point })
            .collect();
        match join {
            Join::Append => self.metadata.origins.extend(origins),
            Join::Deduplicate => self.metadata.origins.extend(origins.into_iter().skip(1)),
            Join::Cross => {
                let end = self.metadata.origins.len() - 1;
                self.metadata.origins.insert(end, origins[0]);
                self.metadata.origins.extend(origins.into_iter().skip(1));
            }
        }
        self.metadata.received.push(frame.points.len() as u32);
        append_rows(&mut self.data.points, &frame.points, join);
        Ok(())
    }

    pub fn finish(self) -> Result<(SweepData, SegmentMetadata)> {
        if self.metadata.received.len() != self.plan.segments().len() {
            return Err(self.error("incomplete segmented acquisition"));
        }
        Ok((self.data, self.metadata))
    }

    fn error(&self, message: &str) -> Error {
        Error::Protocol(format!(
            "segment {}: {message}",
            self.metadata.received.len() + 1
        ))
    }

    fn validate_rows(&self, points: &[SweepPoint], complete: bool) -> Result<()> {
        let Some(segment) = self.plan.segments().get(self.metadata.received.len()) else {
            return Err(self.error("unexpected extra segment"));
        };
        let count = segment.points as usize;
        if points.len() > count || (complete && points.len() != count) {
            return Err(self.error("unexpected sample count"));
        }
        let width = table::columns(self.data.mode, &self.data.format)
            .ok_or_else(|| self.error("unsupported measurement format"))?
            .len();
        if points.iter().any(|point| {
            !point.freq_hz.is_finite() || point.freq_hz < 0.0 || point.values.len() != width
        }) || points
            .windows(2)
            .any(|pair| pair[0].freq_hz >= pair[1].freq_hz)
        {
            return Err(self.error("expected ascending frequencies and complete measurement rows"));
        }
        if let Some(first) = points.first()
            && !endpoint_matches(first.freq_hz, segment.definition.start_hz, segment)
        {
            return Err(self.error("reported start frequency is outside the segment edge interval"));
        }
        if complete
            && !endpoint_matches(
                points[count - 1].freq_hz,
                segment.definition.stop_hz,
                segment,
            )
        {
            return Err(self.error("reported stop frequency is outside the segment edge interval"));
        }
        Ok(())
    }
}

// A host guard, not an instrument accuracy bound (section 12.4).
fn endpoint_matches(actual: f64, requested: u64, segment: &PlannedSegment) -> bool {
    (actual - requested as f64).abs() < segment.step_hz()
}

#[derive(Clone, Copy)]
enum Join {
    Append,
    Deduplicate,
    Cross,
}

fn join_kind(previous: &[SweepPoint], next: &[SweepPoint]) -> Result<Join> {
    let (Some(last), Some(first)) = (previous.last(), next.first()) else {
        return Ok(Join::Append);
    };
    if first.freq_hz > last.freq_hz {
        return Ok(Join::Append);
    }
    if first.freq_hz == last.freq_hz {
        return Ok(Join::Deduplicate);
    }
    if previous.len() > 1
        && previous[previous.len() - 2].freq_hz < first.freq_hz
        && next
            .get(1)
            .is_none_or(|second| last.freq_hz < second.freq_hz)
    {
        return Ok(Join::Cross);
    }
    Err(Error::Protocol(
        "segment frequencies interleave beyond their shared boundary".into(),
    ))
}

fn append_rows(target: &mut Vec<SweepPoint>, points: &[SweepPoint], join: Join) {
    match join {
        Join::Append => target.extend_from_slice(points),
        Join::Deduplicate => target.extend_from_slice(&points[1..]),
        Join::Cross => {
            target.insert(target.len() - 1, points[0].clone());
            target.extend_from_slice(&points[1..]);
        }
    }
}
