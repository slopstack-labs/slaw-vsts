//! Sample-accurate-ish automation governance.
//!
//! VST3 hosts deliver automation as a queue of `(sample_offset, normalized_value)`
//! points per parameter per block, and the values ramp linearly between them
//! (before the first point, from the parameter's previous value). Plugins, on the
//! other hand, see one plain value per parameter per [`crate::ProcessBlock`].
//!
//! The substrate reconciles the two stakeholders without touching the plugin-facing
//! API: when a block carries automation, it is split into sub-blocks of at most
//! [`AUTOMATION_SUB_BLOCK_FRAMES`] frames (and exactly at automation points where that
//! costs little), and each sub-block is handed the interpolated values at its first
//! frame, its own slice of note events, and a transport advanced to where it starts.
//! A block without automation stays one plugin call.
//!
//! Everything here is plain Rust over preallocated memory: nothing allocates once
//! [`AutomationLanes::new`] has returned.

use crate::{ParameterSpecification, TransportSnapshot};

/// The longest stretch a plugin renders on one set of parameter values while automation moves.
/// 32 frames is 0.7 ms at 44.1 kHz: fine enough that a fast ramp no longer zippers, coarse
/// enough that a 2048-frame block costs 64 plugin calls rather than 2048.
pub const AUTOMATION_SUB_BLOCK_FRAMES: usize = 32;

/// The shortest sub-block worth an extra plugin call to land exactly on an automation point.
/// Points closer than this to the previous split (or to the end of the block) are honoured
/// by interpolation alone.
pub const MINIMUM_POINT_ALIGNED_SUB_BLOCK_FRAMES: usize = 8;

/// Automation points retained per parameter per block, preallocated up front. A host that
/// sends more is thinned evenly (first and last points always kept), which costs resolution,
/// never correctness of the final value and never an allocation.
pub const AUTOMATION_POINTS_PER_PARAMETER: usize = 64;

/// Snaps a position that lands on a bar line within floating-point noise into the new bar,
/// so a sub-block starting exactly on the downbeat is told it is on the downbeat.
const BAR_LINE_TOLERANCE_QUARTER_NOTES: f64 = 1.0e-9;

/// The bar length the substrate assumes when the host gives a bar position but no time
/// signature. Matches what tempo-aware plugins assume in the same situation.
const FALLBACK_BAR_QUARTER_NOTES: f64 = 4.0;

/// One retained point of a host's `IParamValueQueue`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AutomationPoint {
    pub(crate) sample_offset: usize,
    pub(crate) normalized_value: f64,
}

/// The points one parameter received this block, plus the value it ramps from.
pub(crate) struct AutomationLane {
    previous_normalized_value: f64,
    points: Vec<AutomationPoint>,
    /// Index of the first point strictly after the last frame queried. Queries move forward
    /// through the block, so this cursor keeps every lookup amortised O(1).
    upcoming_point_index: usize,
}

impl AutomationLane {
    fn with_preallocated_capacity() -> Self {
        Self {
            previous_normalized_value: 0.0,
            points: Vec::with_capacity(AUTOMATION_POINTS_PER_PARAMETER),
            upcoming_point_index: 0,
        }
    }

    fn is_moving(&self) -> bool {
        !self.points.is_empty()
    }

    fn begin(&mut self, previous_normalized_value: f64) {
        self.previous_normalized_value = previous_normalized_value;
        self.points.clear();
        self.upcoming_point_index = 0;
    }

    fn record_point(&mut self, sample_offset: usize, normalized_value: f64) {
        if self.points.len() == self.points.capacity() {
            // Never grow on the audio thread; the selection of retained indices keeps this rare.
            return;
        }
        // Hosts promise ascending offsets; enforce it so interpolation can rely on it.
        let monotonic_sample_offset = self
            .points
            .last()
            .map_or(sample_offset, |previous_point| sample_offset.max(previous_point.sample_offset));
        self.points.push(AutomationPoint {
            sample_offset: monotonic_sample_offset,
            normalized_value,
        });
    }

    /// The normalized value at `frame_index`, interpolated the way VST3 defines it: linear
    /// between points, ramping from the previous value before the first point, holding the
    /// last point after it. Queries must not go backwards within a block.
    pub(crate) fn normalized_value_at(&mut self, frame_index: usize) -> f64 {
        while self.upcoming_point_index < self.points.len() && self.points[self.upcoming_point_index].sample_offset <= frame_index {
            self.upcoming_point_index += 1;
        }
        let (ramp_start_offset, ramp_start_value) = match self.upcoming_point_index.checked_sub(1) {
            Some(reached_point_index) => {
                let reached_point = self.points[reached_point_index];
                (reached_point.sample_offset, reached_point.normalized_value)
            }
            None => (0, self.previous_normalized_value),
        };
        match self.points.get(self.upcoming_point_index) {
            None => ramp_start_value,
            Some(ramp_end_point) => {
                // ramp_end_point.sample_offset > frame_index >= ramp_start_offset, so the span is never zero.
                let ramp_span_frames = (ramp_end_point.sample_offset - ramp_start_offset) as f64;
                let ramp_progress = (frame_index - ramp_start_offset) as f64 / ramp_span_frames;
                ramp_start_value + (ramp_end_point.normalized_value - ramp_start_value) * ramp_progress
            }
        }
    }

    fn first_point_offset_at_or_after(&self, frame_index: usize) -> Option<usize> {
        // Points before the cursor sit at or before an earlier query, which is before `frame_index`.
        self.points[self.upcoming_point_index.min(self.points.len())..]
            .iter()
            .map(|automation_point| automation_point.sample_offset)
            .find(|sample_offset| *sample_offset >= frame_index)
    }
}

/// Which points of a queue of `point_count` to retain, without allocating: all of them when
/// they fit, otherwise [`AUTOMATION_POINTS_PER_PARAMETER`] spread evenly from first to last.
pub(crate) fn retained_point_indices(point_count: usize) -> impl Iterator<Item = usize> {
    let retained_count = point_count.min(AUTOMATION_POINTS_PER_PARAMETER);
    (0..retained_count).map(move |retained_index| {
        if point_count <= AUTOMATION_POINTS_PER_PARAMETER {
            retained_index
        } else {
            retained_index * (point_count - 1) / (AUTOMATION_POINTS_PER_PARAMETER - 1)
        }
    })
}

/// Per-parameter automation for the current block, allocated once per `setupProcessing`.
pub(crate) struct AutomationLanes {
    lanes: Vec<AutomationLane>,
    moving_lane_count: usize,
}

impl AutomationLanes {
    pub(crate) fn new(parameter_count: usize) -> Self {
        Self {
            lanes: (0..parameter_count).map(|_| AutomationLane::with_preallocated_capacity()).collect(),
            moving_lane_count: 0,
        }
    }

    /// Forgets last block's points. Call before ingesting a new block's queues.
    pub(crate) fn clear(&mut self) {
        for automation_lane in &mut self.lanes {
            automation_lane.begin(0.0);
        }
        self.moving_lane_count = 0;
    }

    /// Starts (or restarts) a parameter's lane for this block, ramping from `previous_normalized_value`.
    pub(crate) fn begin_lane(&mut self, parameter_index: usize, previous_normalized_value: f64) {
        if let Some(automation_lane) = self.lanes.get_mut(parameter_index) {
            if automation_lane.is_moving() {
                self.moving_lane_count -= 1;
            }
            automation_lane.begin(previous_normalized_value);
        }
    }

    pub(crate) fn record_point(&mut self, parameter_index: usize, sample_offset: usize, normalized_value: f64) {
        if let Some(automation_lane) = self.lanes.get_mut(parameter_index) {
            let was_moving = automation_lane.is_moving();
            automation_lane.record_point(sample_offset, normalized_value);
            if !was_moving && automation_lane.is_moving() {
                self.moving_lane_count += 1;
            }
        }
    }

    /// Whether any parameter carries automation this block (and the block must be split).
    pub(crate) fn any_lane_moving(&self) -> bool {
        self.moving_lane_count > 0
    }

    /// Overwrites the plain value of every automated parameter with its value at `frame_index`.
    /// Parameters without automation keep whatever `plain_parameters` already holds.
    pub(crate) fn write_plain_values_at(&mut self, frame_index: usize, parameters: &[ParameterSpecification], plain_parameters: &mut [f32]) {
        for ((automation_lane, parameter), plain_parameter) in self.lanes.iter_mut().zip(parameters).zip(plain_parameters.iter_mut()) {
            if automation_lane.is_moving() {
                *plain_parameter = parameter.plain_from_normalized(automation_lane.normalized_value_at(frame_index)) as f32;
            }
        }
    }

    /// Where the sub-block starting at `sub_block_start` ends: [`AUTOMATION_SUB_BLOCK_FRAMES`]
    /// later, or earlier on an automation point when neither side of that split would be
    /// shorter than [`MINIMUM_POINT_ALIGNED_SUB_BLOCK_FRAMES`].
    pub(crate) fn sub_block_end(&self, sub_block_start: usize, frame_count: usize) -> usize {
        let mut sub_block_end = (sub_block_start + AUTOMATION_SUB_BLOCK_FRAMES).min(frame_count);
        let earliest_aligned_split = sub_block_start + MINIMUM_POINT_ALIGNED_SUB_BLOCK_FRAMES;
        let latest_aligned_split = frame_count.saturating_sub(MINIMUM_POINT_ALIGNED_SUB_BLOCK_FRAMES);
        for automation_lane in self.lanes.iter().filter(|automation_lane| automation_lane.is_moving()) {
            if let Some(point_offset) = automation_lane.first_point_offset_at_or_after(earliest_aligned_split) {
                if point_offset < sub_block_end && point_offset <= latest_aligned_split {
                    sub_block_end = point_offset;
                }
            }
        }
        sub_block_end
    }
}

/// The transport as it stands `elapsed_frames` into a block that started at `block_start`.
///
/// While playing, `project_quarter_notes` advances by frames × tempo / 60 / sample rate, and
/// `bar_start_quarter_notes` moves to the bar the new position falls in (bar length from the
/// time signature, 4/4 when the host gives none). A stopped transport stands still, as the host's
/// own would. A position the host gave without a tempo cannot be extrapolated, so later
/// sub-blocks are told the position is unknown rather than handed a stale one.
pub(crate) fn transport_after_frames(block_start: &TransportSnapshot, elapsed_frames: usize, sample_rate: f32) -> TransportSnapshot {
    if elapsed_frames == 0 || !block_start.playing {
        return *block_start;
    }
    let mut advanced_transport = *block_start;
    let (Some(tempo_bpm), Some(block_start_quarter_notes)) = (block_start.tempo_bpm, block_start.project_quarter_notes) else {
        advanced_transport.project_quarter_notes = None;
        return advanced_transport;
    };
    let advanced_quarter_notes = block_start_quarter_notes + elapsed_frames as f64 * tempo_bpm / 60.0 / f64::from(sample_rate);
    advanced_transport.project_quarter_notes = Some(advanced_quarter_notes);
    if let Some(block_start_bar_quarter_notes) = block_start.bar_start_quarter_notes {
        let bar_length_quarter_notes = block_start
            .time_signature
            .map(|(numerator, denominator)| 4.0 * f64::from(numerator) / f64::from(denominator))
            .filter(|bar_length| bar_length.is_finite() && *bar_length > 0.0)
            .unwrap_or(FALLBACK_BAR_QUARTER_NOTES);
        let bars_crossed = ((advanced_quarter_notes - block_start_bar_quarter_notes) / bar_length_quarter_notes + BAR_LINE_TOLERANCE_QUARTER_NOTES).floor();
        if bars_crossed >= 1.0 {
            advanced_transport.bar_start_quarter_notes = Some(block_start_bar_quarter_notes + bars_crossed * bar_length_quarter_notes);
        }
    }
    advanced_transport
}

/// Folds one sub-block's meters into the block's: the reading with the largest magnitude
/// wins, sign kept. Every meter in the portfolio is a "worst case of the block" quantity
/// (deepest gain reduction, peak voices employed, peak level), so a split block reports
/// what the same block would have reported unsplit.
pub(crate) fn merge_sub_block_meters(block_meters: &mut [Option<f32>], sub_block_meters: &[Option<f32>]) {
    for (block_meter, sub_block_meter) in block_meters.iter_mut().zip(sub_block_meters) {
        let Some(sub_block_reading) = *sub_block_meter else {
            continue;
        };
        // A NaN reading never wins a comparison, so it only lands when nothing else was reported.
        let sub_block_reading_wins = block_meter.is_none_or(|block_reading| sub_block_reading.abs() > block_reading.abs());
        if sub_block_reading_wins {
            *block_meter = Some(sub_block_reading);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static RAMP_PARAMETERS: [ParameterSpecification; 2] = [
        ParameterSpecification::linear(0, "Ramp", "x", 0.0, 1.0, 0.0),
        ParameterSpecification::linear(1, "Steady", "x", 0.0, 1.0, 0.5),
    ];

    fn lanes_with(points: &[(usize, f64)], previous_normalized_value: f64) -> AutomationLanes {
        let mut automation_lanes = AutomationLanes::new(2);
        automation_lanes.clear();
        automation_lanes.begin_lane(0, previous_normalized_value);
        for (sample_offset, normalized_value) in points {
            automation_lanes.record_point(0, *sample_offset, *normalized_value);
        }
        automation_lanes
    }

    #[test]
    fn values_ramp_linearly_between_points_and_from_the_previous_value() {
        let mut automation_lanes = lanes_with(&[(100, 1.0), (200, 0.0), (200, 0.75)], 0.5);
        let ramp_lane = &mut automation_lanes.lanes[0];
        assert_eq!(ramp_lane.normalized_value_at(0), 0.5, "ramps from the previous value");
        assert!((ramp_lane.normalized_value_at(50) - 0.75).abs() < 1e-12);
        assert_eq!(ramp_lane.normalized_value_at(100), 1.0);
        assert!((ramp_lane.normalized_value_at(150) - 0.5).abs() < 1e-12);
        assert_eq!(ramp_lane.normalized_value_at(200), 0.75, "a second point at the same offset is a jump");
        assert_eq!(ramp_lane.normalized_value_at(10_000), 0.75, "holds the last point");
    }

    #[test]
    fn a_point_at_offset_zero_applies_immediately() {
        let mut automation_lanes = lanes_with(&[(0, 0.25), (64, 0.75)], 0.9);
        assert_eq!(automation_lanes.lanes[0].normalized_value_at(0), 0.25);
        assert!((automation_lanes.lanes[0].normalized_value_at(32) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn only_automated_parameters_are_overwritten() {
        let mut automation_lanes = lanes_with(&[(0, 0.0), (100, 1.0)], 0.0);
        let mut plain_parameters = [-1.0f32, -2.0];
        automation_lanes.write_plain_values_at(25, &RAMP_PARAMETERS, &mut plain_parameters);
        assert_eq!(plain_parameters, [0.25, -2.0]);
    }

    #[test]
    fn sub_blocks_are_capped_and_align_with_points_when_cheap() {
        let automation_lanes = lanes_with(&[(0, 0.0), (40, 1.0), (43, 0.0), (509, 1.0)], 0.0);
        assert!(automation_lanes.any_lane_moving());
        assert_eq!(automation_lanes.sub_block_end(0, 512), 32, "capped at the sub-block size");
        assert_eq!(automation_lanes.sub_block_end(32, 512), 40, "split on the point at 40");
        assert_eq!(automation_lanes.sub_block_end(40, 512), 72, "43 is too close to 40 to deserve a call");
        assert_eq!(automation_lanes.sub_block_end(480, 512), 512, "509 is too close to the end");
        assert_eq!(automation_lanes.sub_block_end(500, 512), 512);
        assert!(!AutomationLanes::new(2).any_lane_moving());
    }

    #[test]
    fn surplus_points_are_thinned_evenly_keeping_first_and_last() {
        assert_eq!(retained_point_indices(3).collect::<Vec<_>>(), vec![0, 1, 2]);
        let thinned: Vec<usize> = retained_point_indices(1_000).collect();
        assert_eq!(thinned.len(), AUTOMATION_POINTS_PER_PARAMETER);
        assert_eq!(thinned[0], 0);
        assert_eq!(*thinned.last().unwrap(), 999);
        assert!(thinned.windows(2).all(|pair| pair[1] > pair[0]));

        // Recording past capacity is ignored rather than reallocated.
        let mut automation_lanes = AutomationLanes::new(1);
        automation_lanes.begin_lane(0, 0.0);
        for point_index in 0..AUTOMATION_POINTS_PER_PARAMETER * 2 {
            automation_lanes.record_point(0, point_index, 0.5);
        }
        assert_eq!(automation_lanes.lanes[0].points.len(), AUTOMATION_POINTS_PER_PARAMETER);
        assert_eq!(automation_lanes.lanes[0].points.capacity(), AUTOMATION_POINTS_PER_PARAMETER);
    }

    #[test]
    fn restarting_a_lane_keeps_the_moving_count_honest() {
        let mut automation_lanes = lanes_with(&[(0, 1.0)], 0.0);
        automation_lanes.begin_lane(0, 0.0);
        assert!(!automation_lanes.any_lane_moving());
        automation_lanes.record_point(0, 3, 1.0);
        automation_lanes.clear();
        assert!(!automation_lanes.any_lane_moving());
    }

    fn playing_transport(project_quarter_notes: f64, bar_start_quarter_notes: f64, time_signature: (i32, i32)) -> TransportSnapshot {
        TransportSnapshot {
            tempo_bpm: Some(120.0),
            project_quarter_notes: Some(project_quarter_notes),
            bar_start_quarter_notes: Some(bar_start_quarter_notes),
            time_signature: Some(time_signature),
            playing: true,
        }
    }

    #[test]
    fn transport_advances_and_crosses_bar_lines() {
        // 120 BPM at 48 kHz: 24 000 frames per quarter note.
        let block_start = playing_transport(2.5, 0.0, (3, 4));
        let within_bar = transport_after_frames(&block_start, 12_000, 48_000.0);
        assert_eq!(within_bar.project_quarter_notes, Some(3.0));
        assert_eq!(within_bar.bar_start_quarter_notes, Some(3.0), "3/4: the bar line is at 3 quarter notes");
        let mid_next_bar = transport_after_frames(&block_start, 36_000, 48_000.0);
        assert_eq!(mid_next_bar.project_quarter_notes, Some(4.0));
        assert_eq!(mid_next_bar.bar_start_quarter_notes, Some(3.0));
        let two_bars_later = transport_after_frames(&block_start, 24_000 * 7, 48_000.0);
        assert_eq!(two_bars_later.bar_start_quarter_notes, Some(9.0));
        assert_eq!(transport_after_frames(&block_start, 0, 48_000.0), block_start);

        // 6/8 bars are three quarter notes long too; 4/4 assumed without a time signature.
        let compound = transport_after_frames(&playing_transport(2.0, 0.0, (6, 8)), 24_000, 48_000.0);
        assert_eq!(compound.bar_start_quarter_notes, Some(3.0));
        let unsigned = TransportSnapshot {
            time_signature: None,
            ..playing_transport(3.5, 0.0, (4, 4))
        };
        assert_eq!(transport_after_frames(&unsigned, 12_000, 48_000.0).bar_start_quarter_notes, Some(4.0));
    }

    #[test]
    fn a_stopped_or_tempo_less_transport_is_not_extrapolated() {
        let stopped = TransportSnapshot {
            playing: false,
            ..playing_transport(1.0, 0.0, (4, 4))
        };
        assert_eq!(transport_after_frames(&stopped, 4_096, 48_000.0), stopped);
        let tempo_less = TransportSnapshot {
            tempo_bpm: None,
            ..playing_transport(1.0, 0.0, (4, 4))
        };
        let advanced = transport_after_frames(&tempo_less, 32, 48_000.0);
        assert_eq!(advanced.project_quarter_notes, None);
        assert!(advanced.playing);
    }

    #[test]
    fn meters_keep_the_largest_magnitude() {
        let mut block_meters = [None, Some(2.0), None, Some(-1.0)];
        merge_sub_block_meters(&mut block_meters, &[Some(3.0), Some(1.0), None, Some(0.5)]);
        assert_eq!(block_meters, [Some(3.0), Some(2.0), None, Some(-1.0)]);
        merge_sub_block_meters(&mut block_meters, &[Some(-4.0), Some(f32::NAN), Some(0.0), Some(-1.5)]);
        assert_eq!(block_meters, [Some(-4.0), Some(2.0), Some(0.0), Some(-1.5)]);
    }
}
