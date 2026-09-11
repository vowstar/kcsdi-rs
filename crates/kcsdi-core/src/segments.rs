// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 kcsdi-rs contributors

//! Host-side adjacent finite sweep plans. Display scales do not affect sampling.

use serde::{Deserialize, Serialize};

use crate::control::CancellationToken;
use crate::data::SweepData;
use crate::device::{Device, PointSettings, S11Params, S21Params, SpecParams, SweepProgress};
use crate::model::Capabilities;
use crate::transport::Transport;
use crate::{Error, Result};

mod join;
pub use join::{SampleOrigin, SegmentJoiner, SegmentMetadata};

/// Application resource limits, independent of per-command device limits.
pub const MAX_SEGMENTS: usize = 32;
pub const MAX_ACQUIRED_POINTS: u32 = 32_032;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start_hz: u64,
    pub stop_hz: u64,
    pub max_step_hz: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentProblem {
    #[error("define between 1 and {MAX_SEGMENTS} segments")]
    Count,
    #[error("stop frequency must exceed start frequency")]
    Order,
    #[error("maximum step must be positive")]
    Step,
    #[error("start frequency must equal the previous stop frequency")]
    Adjacency,
    #[error(
        "requires {required} points, maximum {maximum}. Increase the step or split the segment"
    )]
    Points { required: u128, maximum: u32 },
    #[error("total acquired points exceed {MAX_ACQUIRED_POINTS}")]
    Budget,
    #[error("{0}")]
    Receiver(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentError {
    /// Zero-based row, or no row for a whole-plan resource error.
    pub row: Option<usize>,
    pub problem: SegmentProblem,
}

impl std::fmt::Display for SegmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(row) = self.row {
            write!(f, "segment {}: ", row + 1)?;
        }
        self.problem.fmt(f)
    }
}

impl std::error::Error for SegmentError {}

impl From<SegmentError> for Error {
    fn from(error: SegmentError) -> Self {
        Self::InvalidParameter(error.to_string())
    }
}

impl Segment {
    pub fn points(&self, caps: &Capabilities) -> std::result::Result<u32, SegmentProblem> {
        let span = self
            .stop_hz
            .checked_sub(self.start_hz)
            .filter(|span| *span > 0)
            .ok_or(SegmentProblem::Order)?;
        if self.max_step_hz == 0 {
            return Err(SegmentProblem::Step);
        }
        let intervals = span
            .div_ceil(self.max_step_hz)
            .max(u64::from(caps.points_min.saturating_sub(1)));
        let required = u128::from(intervals) + 1;
        if required > u128::from(caps.points_max) {
            return Err(SegmentProblem::Points {
                required,
                maximum: caps.points_max,
            });
        }
        Ok(required as u32)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSegment {
    definition: Segment,
    points: u32,
}

impl PlannedSegment {
    pub fn definition(&self) -> Segment {
        self.definition
    }

    pub fn points(&self) -> u32 {
        self.points
    }

    pub fn step_hz(&self) -> f64 {
        (self.definition.stop_hz - self.definition.start_hz) as f64 / f64::from(self.points - 1)
    }

    pub fn sweep(&self, settings: &PointSettings) -> FiniteSweep {
        let Segment {
            start_hz, stop_hz, ..
        } = self.definition;
        let points = self.points;
        match *settings {
            PointSettings::S11 { cal, format, rbw } => FiniteSweep::S11(S11Params {
                cal,
                format,
                rbw,
                start_hz,
                stop_hz,
                points,
            }),
            PointSettings::S21 {
                cal,
                format,
                rbw,
                lo,
            } => FiniteSweep::S21(S21Params {
                cal,
                format,
                rbw,
                lo,
                start_hz,
                stop_hz,
                points,
            }),
            PointSettings::Spec {
                cal,
                rbw,
                lo,
                ref_level_dbm,
            } => FiniteSweep::Spec(SpecParams {
                cal,
                rbw,
                lo,
                ref_level_dbm,
                start_hz,
                stop_hz,
                points,
            }),
        }
    }
}

/// A mode-independent adapter to the existing finite APIs, not a new command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiniteSweep {
    S11(S11Params),
    S21(S21Params),
    Spec(SpecParams),
}

impl FiniteSweep {
    pub fn validate(&self, caps: &Capabilities) -> Result<()> {
        match self {
            Self::S11(params) => params.validate(caps),
            Self::S21(params) => params.validate(caps),
            Self::Spec(params) => params.validate(caps),
        }
    }

    pub fn acquire<T: Transport>(
        &self,
        device: &mut Device<T>,
        cancel: &CancellationToken,
        progress: impl FnMut(SweepProgress<'_>),
    ) -> Result<SweepData> {
        match self {
            Self::S11(params) => device.sweep_s11_controlled(params, cancel, progress),
            Self::S21(params) => device.sweep_s21_controlled(params, cancel, progress),
            Self::Spec(params) => device.sweep_spec_controlled(params, cancel, progress),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentPlan {
    segments: Vec<PlannedSegment>,
    acquired_points: u32,
    settings: PointSettings,
}

impl SegmentPlan {
    pub fn new(
        definitions: &[Segment],
        settings: &PointSettings,
        caps: &Capabilities,
    ) -> std::result::Result<Self, SegmentError> {
        if definitions.is_empty() || definitions.len() > MAX_SEGMENTS {
            return Err(SegmentError {
                row: None,
                problem: SegmentProblem::Count,
            });
        }
        let mut segments = Vec::with_capacity(definitions.len());
        let mut acquired_points: u32 = 0;
        for (row, definition) in definitions.iter().enumerate() {
            let row_error = |problem| SegmentError {
                row: Some(row),
                problem,
            };
            if row > 0 && definition.start_hz != definitions[row - 1].stop_hz {
                return Err(row_error(SegmentProblem::Adjacency));
            }
            let points = definition.points(caps).map_err(row_error)?;
            let segment = PlannedSegment {
                definition: *definition,
                points,
            };
            segment
                .sweep(settings)
                .validate(caps)
                .map_err(|error| row_error(SegmentProblem::Receiver(error.to_string())))?;
            acquired_points = acquired_points
                .checked_add(points)
                .ok_or_else(|| row_error(SegmentProblem::Budget))?;
            if acquired_points > MAX_ACQUIRED_POINTS {
                return Err(row_error(SegmentProblem::Budget));
            }
            segments.push(segment);
        }
        Ok(Self {
            segments,
            acquired_points,
            settings: settings.clone(),
        })
    }

    pub fn segments(&self) -> &[PlannedSegment] {
        &self.segments
    }

    pub fn acquired_points(&self) -> u32 {
        self.acquired_points
    }

    pub fn settings(&self) -> &PointSettings {
        &self.settings
    }
}

#[cfg(test)]
mod tests;
