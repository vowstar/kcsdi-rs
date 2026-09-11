// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Immutable workspace acquisition requests and completed measurements.

use std::sync::Arc;
use std::time::SystemTime;

use kcsdi_core::data::SweepData;
use kcsdi_core::device::{PointParams, PointSettings, S11Params, S21Params, SpecParams};
use kcsdi_core::protocol::StreamMode;
use kcsdi_core::segments::{SegmentMetadata, SegmentPlan};
use kcsdi_core::{Error, Result};
use serde::{Deserialize, Serialize};

pub const MAX_TRACES: usize = 10;

/// Stable identity, independent of a trace's current list position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TraceId(pub u64);

/// Actual wire settings, without display scale, style or projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquisitionSettings {
    S11(S11Params),
    S21(S21Params),
    Spec(SpecParams),
    Segments(SegmentPlan),
    List {
        settings: PointSettings,
        frequencies_hz: Vec<u64>,
    },
}

impl From<SegmentPlan> for AcquisitionSettings {
    fn from(plan: SegmentPlan) -> Self {
        Self::Segments(plan)
    }
}

impl AcquisitionSettings {
    pub fn receiver(&self) -> PointSettings {
        match self {
            Self::S11(p) => PointSettings::S11 {
                cal: p.cal,
                format: p.format,
                rbw: p.rbw,
            },
            Self::S21(p) => PointSettings::S21 {
                cal: p.cal,
                format: p.format,
                rbw: p.rbw,
                lo: p.lo,
            },
            Self::Spec(p) => PointSettings::Spec {
                cal: p.cal,
                rbw: p.rbw,
                lo: p.lo,
                ref_level_dbm: p.ref_level_dbm,
            },
            Self::List { settings, .. } => settings.clone(),
            Self::Segments(plan) => plan.settings().clone(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let caps = crate::state::DEVICE_MODEL.capabilities();
        let rbw = match self {
            Self::S11(params) => {
                params.validate(&caps)?;
                params.rbw
            }
            Self::S21(params) => {
                params.validate(&caps)?;
                params.rbw
            }
            Self::Spec(params) => {
                params.validate(&caps)?;
                Some(params.rbw)
            }
            Self::Segments(plan) => {
                for segment in plan.segments() {
                    segment.sweep(plan.settings()).validate(&caps)?;
                }
                match plan.settings() {
                    PointSettings::S11 { rbw, .. } | PointSettings::S21 { rbw, .. } => *rbw,
                    PointSettings::Spec { rbw, .. } => Some(*rbw),
                }
            }
            Self::List {
                settings,
                frequencies_hz,
            } => {
                if !(crate::frequency_list::MIN_POINTS..=crate::frequency_list::MAX_POINTS)
                    .contains(&frequencies_hz.len())
                    || frequencies_hz.windows(2).any(|pair| pair[0] > pair[1])
                {
                    return Err(Error::InvalidParameter(
                        "frequency lists require 3 to 1001 ascending entries".into(),
                    ));
                }
                for &frequency_hz in frequencies_hz {
                    PointParams {
                        settings: settings.clone(),
                        frequency_hz,
                    }
                    .validate(&caps)?;
                }
                match settings {
                    PointSettings::S11 { rbw, .. } | PointSettings::S21 { rbw, .. } => *rbw,
                    PointSettings::Spec { rbw, .. } => Some(*rbw),
                }
            }
        };
        if rbw.is_none() {
            return Err(Error::InvalidParameter(
                "workspace sweeps require an explicit bandwidth".into(),
            ));
        }
        Ok(())
    }

    pub fn mode(&self) -> StreamMode {
        match self {
            Self::S11(_) => StreamMode::S11,
            Self::S21(_) => StreamMode::S21,
            Self::Spec(_) => StreamMode::Spec,
            Self::Segments(plan) => plan.settings().mode(),
            Self::List { settings, .. } => settings.mode(),
        }
    }

    pub fn format(&self) -> &'static str {
        match self {
            Self::S11(params) => params.format.as_str(),
            Self::S21(params) => params.format.as_str(),
            Self::Spec(_) => "",
            Self::Segments(plan) => plan.settings().format(),
            Self::List { settings, .. } => settings.format(),
        }
    }

    pub fn points(&self) -> u32 {
        match self {
            Self::S11(params) => params.points,
            Self::S21(params) => params.points,
            Self::Spec(params) => params.points,
            Self::Segments(plan) => plan.acquired_points(),
            Self::List { frequencies_hz, .. } => frequencies_hz.len() as u32,
        }
    }

    pub fn accepts(&self, data: &SweepData) -> bool {
        data.mode == self.mode()
            && data.format == self.format()
            && match self {
                Self::Segments(plan) => {
                    let maximum = plan.acquired_points() as usize;
                    (maximum - plan.segments().len() + 1..=maximum).contains(&data.points.len())
                }
                _ => data.points.len() == self.points() as usize,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionGroup {
    pub settings: AcquisitionSettings,
    pub members: Vec<TraceId>,
}

/// One finite pass over visible definitions, repeated by a single worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepPlan {
    pub groups: Vec<AcquisitionGroup>,
    pub run: crate::run_settings::RunSettings,
}

impl SweepPlan {
    pub fn from_requests(
        requests: impl IntoIterator<Item = (TraceId, AcquisitionSettings)>,
    ) -> Result<Self> {
        let mut groups: Vec<AcquisitionGroup> = Vec::new();
        let mut members = Vec::new();
        for (id, settings) in requests {
            if id.0 == 0 || members.contains(&id) {
                return Err(Error::InvalidParameter(
                    "invalid or repeated trace ID".into(),
                ));
            }
            if members.len() == MAX_TRACES {
                return Err(Error::InvalidParameter(
                    "a workspace supports at most 10 traces".into(),
                ));
            }
            settings.validate()?;
            members.push(id);
            if let Some(group) = groups.iter_mut().find(|group| group.settings == settings) {
                group.members.push(id);
            } else {
                groups.push(AcquisitionGroup {
                    settings,
                    members: vec![id],
                });
            }
        }
        let plan = Self {
            groups,
            run: Default::default(),
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Recheck caller-built plans before the worker changes any instrument state.
    pub fn validate(&self) -> Result<()> {
        self.run.validate().map_err(Error::InvalidParameter)?;
        if self.groups.is_empty() || self.groups.len() > MAX_TRACES {
            return Err(Error::InvalidParameter(
                "select between 1 and 10 visible traces".into(),
            ));
        }
        let mut members = Vec::new();
        for (index, group) in self.groups.iter().enumerate() {
            group.settings.validate()?;
            if group.members.is_empty()
                || self.groups[..index]
                    .iter()
                    .any(|previous| previous.settings == group.settings)
            {
                return Err(Error::InvalidParameter(
                    "empty or repeated acquisition group".into(),
                ));
            }
            for &id in &group.members {
                if id.0 == 0 || members.contains(&id) || members.len() == MAX_TRACES {
                    return Err(Error::InvalidParameter(
                        "invalid workspace trace membership".into(),
                    ));
                }
                members.push(id);
            }
        }
        Ok(())
    }
}

/// One complete measured frame and the immutable conditions that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedSweep {
    pub data: SweepData,
    pub segments: Option<SegmentMetadata>,
    pub settings: AcquisitionSettings,
    pub session_id: u64,
    pub completed_at: SystemTime,
}

impl CompletedSweep {
    pub fn accepts_data(&self) -> bool {
        self.settings.accepts(&self.data)
            && match (&self.settings, &self.segments) {
                (AcquisitionSettings::Segments(plan), Some(metadata)) => {
                    metadata.validate(plan, &self.data).is_ok()
                }
                (AcquisitionSettings::Segments(_), None) | (_, Some(_)) => false,
                (_, None) => true,
            }
    }
}

#[derive(Debug)]
pub struct SweepDelivery {
    pub members: Vec<TraceId>,
    pub snapshot: Arc<CompletedSweep>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kcsdi_core::commands::{Cal, Format, Lo};
    use kcsdi_core::model::Rbw;

    pub fn s11() -> S11Params {
        S11Params {
            cal: Cal::CalOff,
            format: Format::Z,
            points: 3,
            start_hz: 1_000_000,
            stop_hz: 2_000_000,
            rbw: Some(Rbw::R10k),
        }
    }

    pub fn spec() -> SpecParams {
        SpecParams {
            cal: Cal::CalOff,
            lo: Lo::HighLo,
            points: 3,
            start_hz: 1_000_000,
            stop_hz: 2_000_000,
            rbw: Rbw::R10k,
            ref_level_dbm: -10,
        }
    }

    pub fn s21() -> S21Params {
        S21Params {
            cal: Cal::CalOff,
            format: Format::Delay,
            lo: Lo::HighLo,
            points: 3,
            start_hz: 1_000_000,
            stop_hz: 2_000_000,
            rbw: Some(Rbw::R10k),
        }
    }

    #[test]
    fn identical_wire_requests_share_one_ordered_group() {
        let reflection = AcquisitionSettings::S11(s11());
        let spectrum = AcquisitionSettings::Spec(spec());
        let plan = SweepPlan::from_requests([
            (TraceId(7), reflection.clone()),
            (TraceId(2), spectrum.clone()),
            (TraceId(9), reflection.clone()),
            (TraceId(4), spectrum.clone()),
        ])
        .unwrap();
        assert_eq!(
            plan.groups,
            [
                AcquisitionGroup {
                    settings: reflection,
                    members: vec![TraceId(7), TraceId(9)]
                },
                AcquisitionGroup {
                    settings: spectrum,
                    members: vec![TraceId(2), TraceId(4)]
                },
            ]
        );
    }

    #[test]
    fn each_acquisition_condition_separates_groups() {
        let base = s11();
        let different = [
            S11Params {
                cal: Cal::CalSys,
                ..base.clone()
            },
            S11Params {
                format: Format::Ma,
                ..base.clone()
            },
            S11Params {
                rbw: Some(Rbw::R1k),
                ..base.clone()
            },
            S11Params {
                start_hz: 500_000,
                ..base.clone()
            },
            S11Params {
                stop_hz: 3_000_000,
                ..base.clone()
            },
            S11Params {
                points: 201,
                ..base.clone()
            },
        ];
        for changed in different {
            let plan = SweepPlan::from_requests([
                (TraceId(1), AcquisitionSettings::S11(base.clone())),
                (TraceId(2), AcquisitionSettings::S11(changed)),
            ])
            .unwrap();
            assert_eq!(plan.groups.len(), 2);
        }
        let base = spec();
        for changed in [
            SpecParams {
                cal: Cal::CalOn,
                ..base.clone()
            },
            SpecParams {
                lo: Lo::LowLo,
                ..base.clone()
            },
            SpecParams {
                rbw: Rbw::R1k,
                ..base.clone()
            },
            SpecParams {
                ref_level_dbm: -20,
                ..base.clone()
            },
        ] {
            let plan = SweepPlan::from_requests([
                (TraceId(1), AcquisitionSettings::Spec(base.clone())),
                (TraceId(2), AcquisitionSettings::Spec(changed)),
            ])
            .unwrap();
            assert_eq!(plan.groups.len(), 2);
        }
    }

    #[test]
    fn invalid_empty_duplicate_or_oversized_plans_cannot_run() {
        assert!(SweepPlan::from_requests([]).is_err());
        let settings = AcquisitionSettings::S11(s11());
        for ids in [vec![0], vec![1, 1], (1..=11).collect()] {
            assert!(
                SweepPlan::from_requests(ids.into_iter().map(|id| (TraceId(id), settings.clone())))
                    .is_err()
            );
        }
        let plan =
            SweepPlan::from_requests((1..=10).map(|id| (TraceId(id), settings.clone()))).unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].members.len(), 10);
        let mut invalid = plan.clone();
        invalid.groups.push(invalid.groups[0].clone());
        assert!(invalid.validate().is_err());
        invalid = plan;
        invalid.groups[0].members.clear();
        assert!(invalid.validate().is_err());
        let mut unspecified = s11();
        unspecified.rbw = None;
        assert!(AcquisitionSettings::S11(unspecified).validate().is_err());
    }
}
