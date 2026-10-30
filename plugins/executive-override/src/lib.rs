//! # Executive Override
//!
//! When the kick speaks, everyone else stops talking. A ducker with three ways
//! to decide who has the floor:
//!
//! - **Sidechain**: an envelope follower on the stereo aux bus ("Sidechain"),
//!   optionally high-passed so only the kick's authority counts.
//! - **Tempo pump**: a ducking curve locked to the host's tempo and bar
//!   position, for hosts (and org charts) without sidechain routing.
//! - **MIDI trigger**: every note-on is an executive decision.
//!
//! All three share one shape: duck to `Depth` over `Attack`, stay down for
//! `Hold`, then recover over `Release` along `Curve`.

use synergy_substrate::dsp::{decibels_to_gain, gain_to_decibels, smoothing_coefficient, ZeroDelayStateVariableFilter};
use synergy_substrate::{
    export_synergy_plugin, NoteEventKind, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin, TransportSnapshot,
};

pub const MODE: usize = 0;
pub const THRESHOLD: usize = 1;
pub const DEPTH: usize = 2;
pub const ATTACK: usize = 3;
pub const RELEASE: usize = 4;
pub const HOLD: usize = 5;
pub const CURVE: usize = 6;
pub const RATE: usize = 7;
pub const SIDECHAIN_HIGHPASS: usize = 8;
pub const MIX: usize = 9;
pub const GAIN_REDUCTION_METER: usize = 10;

pub const MODE_SIDECHAIN: f32 = 0.0;
pub const MODE_TEMPO_PUMP: f32 = 1.0;
pub const MODE_MIDI_TRIGGER: f32 = 2.0;

const MODES: &[&str] = &["Sidechain", "Tempo pump", "MIDI trigger"];
const RATES: &[&str] = &["1/4", "1/8", "1/2", "1 bar"];

static PARAMETERS: [ParameterSpecification; 11] = [
    ParameterSpecification::list(0, "Mode", MODES, 1),
    ParameterSpecification::linear(1, "Threshold", "dB", -60.0, 0.0, -30.0),
    ParameterSpecification::linear(2, "Depth", "dB", 0.0, 48.0, 12.0),
    ParameterSpecification::logarithmic(3, "Attack", "ms", 0.1, 50.0, 1.0),
    ParameterSpecification::logarithmic(4, "Release", "ms", 10.0, 1_000.0, 160.0),
    ParameterSpecification::linear(5, "Hold", "ms", 0.0, 250.0, 10.0),
    ParameterSpecification::linear(6, "Curve", "%", 0.0, 100.0, 50.0),
    ParameterSpecification::list(7, "Rate", RATES, 0),
    ParameterSpecification::logarithmic(8, "Sidechain HPF", "Hz", 20.0, 500.0, 20.0),
    ParameterSpecification::linear(9, "Mix", "%", 0.0, 100.0, 100.0),
    ParameterSpecification::linear(10, "Gain Reduction", "dB", 0.0, 48.0, 0.0).as_meter(),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Executive Override",
    class_identifier: [0x4EBB12CC, 0xD7BC4424, 0xBF1ABAE2, 0x78F59112],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Dynamics",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: true,
    note_input: true,
    tail_seconds: 0.0,
};

/// Tempo used when the host does not say (or is stopped and says nothing).
const FALLBACK_TEMPO_BPM: f64 = 120.0;
/// Sidechain level above threshold (in dB) at which the duck is fully engaged.
const SIDECHAIN_ENGAGE_RANGE_DECIBELS: f32 = 6.0;
const KEY_FOLLOWER_RELEASE_MILLISECONDS: f32 = 30.0;

/// The duck shape at `seconds_since_trigger`: `depth` dB through the hold, then a
/// release to 0 dB whose steepness `curve` (0..1) sets: 0 linear, 1 snappy.
pub fn duck_shape_decibels(seconds_since_trigger: f64, depth_decibels: f32, hold_seconds: f64, release_seconds: f64, curve: f32) -> f32 {
    if seconds_since_trigger < hold_seconds {
        return depth_decibels;
    }
    let release_progress = ((seconds_since_trigger - hold_seconds) / release_seconds.max(1.0e-4)).clamp(0.0, 1.0) as f32;
    let exponent = 1.0 + 4.0 * curve.clamp(0.0, 1.0);
    depth_decibels * (1.0 - release_progress).powf(exponent)
}

/// Quarter notes per duck for a `Rate` list index, given the bar length.
pub fn rate_quarter_notes(rate_index: f32, bar_quarter_notes: f64) -> f64 {
    match rate_index.round() as i32 {
        1 => 0.5,
        2 => 2.0,
        3 => bar_quarter_notes,
        _ => 1.0,
    }
}

pub struct ExecutiveOverride {
    sample_rate: f32,
    /// Smoothed reduction actually applied, in dB (positive = ducked).
    applied_reduction_decibels: f32,
    /// Seconds since the last trigger (sidechain over threshold, or a note-on).
    seconds_since_trigger: f64,
    key_highpass_left: ZeroDelayStateVariableFilter,
    key_highpass_right: ZeroDelayStateVariableFilter,
    key_envelope: f32,
    /// Free-running position in quarter notes, for tempo pump without a host transport.
    internal_quarter_notes: f64,
}

impl ExecutiveOverride {
    fn ducked_gain(&mut self, target_reduction_decibels: f32, attack_coefficient: f32) -> f32 {
        if target_reduction_decibels > self.applied_reduction_decibels {
            // Going down: glide over the attack so the duck never clicks.
            self.applied_reduction_decibels = target_reduction_decibels + attack_coefficient * (self.applied_reduction_decibels - target_reduction_decibels);
        } else {
            // Coming back up: follow the release curve exactly.
            self.applied_reduction_decibels = target_reduction_decibels;
        }
        decibels_to_gain(-self.applied_reduction_decibels)
    }

    /// Where the tempo pump is at the block's first sample, and how fast it moves.
    fn tempo_position(&self, transport: &TransportSnapshot) -> (f64, f64, f64) {
        let tempo_bpm = transport.tempo_bpm.unwrap_or(FALLBACK_TEMPO_BPM);
        let bar_quarter_notes = transport
            .time_signature
            .map(|(numerator, denominator)| 4.0 * f64::from(numerator) / f64::from(denominator))
            .unwrap_or(4.0);
        let start_quarter_notes = match (transport.playing, transport.project_quarter_notes) {
            (true, Some(project_quarter_notes)) => project_quarter_notes - transport.bar_start_quarter_notes.unwrap_or(0.0),
            _ => self.internal_quarter_notes,
        };
        (start_quarter_notes, tempo_bpm, bar_quarter_notes)
    }
}

impl SynergyPlugin for ExecutiveOverride {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            applied_reduction_decibels: 0.0,
            seconds_since_trigger: f64::INFINITY,
            key_highpass_left: ZeroDelayStateVariableFilter::new(),
            key_highpass_right: ZeroDelayStateVariableFilter::new(),
            key_envelope: 0.0,
            internal_quarter_notes: 0.0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.sample_rate);
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        let parameters = block.parameters;
        let mode = parameters[MODE].round();
        let threshold_decibels = parameters[THRESHOLD];
        let depth_decibels = parameters[DEPTH];
        let attack_coefficient = smoothing_coefficient(parameters[ATTACK], self.sample_rate);
        let release_seconds = f64::from(parameters[RELEASE]) * 0.001;
        let hold_seconds = f64::from(parameters[HOLD]) * 0.001;
        let curve = parameters[CURVE] / 100.0;
        let highpass_hz = parameters[SIDECHAIN_HIGHPASS];
        let wet_mix = (parameters[MIX] / 100.0).clamp(0.0, 1.0);
        let key_release_coefficient = smoothing_coefficient(KEY_FOLLOWER_RELEASE_MILLISECONDS, self.sample_rate);
        let seconds_per_frame = 1.0 / f64::from(self.sample_rate);
        let frame_count = block.frames();

        let (start_quarter_notes, tempo_bpm, bar_quarter_notes) = self.tempo_position(&block.transport);
        let quarter_notes_per_frame = tempo_bpm / 60.0 * seconds_per_frame;
        let pump_period_quarter_notes = rate_quarter_notes(parameters[RATE], bar_quarter_notes);
        let seconds_per_quarter_note = 60.0 / tempo_bpm;

        let mut next_event_index = 0;
        let mut deepest_reduction_decibels = 0.0f32;
        for frame_index in 0..frame_count {
            let seconds_since_trigger = if mode == MODE_TEMPO_PUMP {
                // Position within the current pump period, measured from the bar line.
                let position_quarter_notes = start_quarter_notes + frame_index as f64 * quarter_notes_per_frame;
                position_quarter_notes.rem_euclid(pump_period_quarter_notes) * seconds_per_quarter_note
            } else if mode == MODE_MIDI_TRIGGER {
                while next_event_index < block.events.len() && block.events[next_event_index].sample_offset <= frame_index {
                    if matches!(block.events[next_event_index].kind, NoteEventKind::On { .. }) {
                        self.seconds_since_trigger = 0.0;
                    }
                    next_event_index += 1;
                }
                let elapsed = self.seconds_since_trigger;
                self.seconds_since_trigger += seconds_per_frame;
                elapsed
            } else {
                // Sidechain: the key is above threshold → the clock restarts every sample it stays there.
                let key_level = match block.sidechain {
                    Some((key_left, key_right)) => {
                        let filtered_left = self
                            .key_highpass_left
                            .process(key_left[frame_index], highpass_hz, 0.0, self.sample_rate)
                            .highpass;
                        let filtered_right = self
                            .key_highpass_right
                            .process(key_right[frame_index], highpass_hz, 0.0, self.sample_rate)
                            .highpass;
                        filtered_left.abs().max(filtered_right.abs())
                    }
                    None => 0.0,
                };
                self.key_envelope = if key_level > self.key_envelope {
                    key_level
                } else {
                    key_level + key_release_coefficient * (self.key_envelope - key_level)
                };
                let overshoot_decibels = gain_to_decibels(self.key_envelope) - threshold_decibels;
                if overshoot_decibels > 0.0 {
                    // Partial authority between threshold and threshold + 6 dB, full authority above.
                    let engagement = (overshoot_decibels / SIDECHAIN_ENGAGE_RANGE_DECIBELS).min(1.0);
                    if engagement >= 1.0 {
                        self.seconds_since_trigger = 0.0;
                    } else {
                        // Map partial engagement onto the release curve: louder keys sit earlier in it.
                        let partial_seconds = hold_seconds + (1.0 - f64::from(engagement)) * release_seconds;
                        self.seconds_since_trigger = self.seconds_since_trigger.min(partial_seconds);
                    }
                }
                let elapsed = self.seconds_since_trigger;
                self.seconds_since_trigger += seconds_per_frame;
                elapsed
            };

            let target_reduction_decibels = duck_shape_decibels(seconds_since_trigger, depth_decibels, hold_seconds, release_seconds, curve);
            let ducked_gain = self.ducked_gain(target_reduction_decibels, attack_coefficient);
            deepest_reduction_decibels = deepest_reduction_decibels.max(self.applied_reduction_decibels);
            let applied_gain = 1.0 - wet_mix + wet_mix * ducked_gain;
            block.output_left[frame_index] = block.input_left[frame_index] * applied_gain;
            block.output_right[frame_index] = block.input_right[frame_index] * applied_gain;
        }
        self.internal_quarter_notes = (start_quarter_notes + frame_count as f64 * quarter_notes_per_frame).rem_euclid(bar_quarter_notes * 64.0);
        block.meters[GAIN_REDUCTION_METER] = Some(deepest_reduction_decibels);
    }
}

export_synergy_plugin!(ExecutiveOverride);

#[cfg(test)]
mod tests {
    use super::*;
    use synergy_substrate::NoteEvent;

    const SAMPLE_RATE: f32 = 48_000.0;

    fn parameters_for(mode: f32) -> Vec<f32> {
        let mut parameters: Vec<f32> = PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect();
        parameters[MODE] = mode;
        parameters
    }

    struct Rendered {
        output_left: Vec<f32>,
        deepest_reduction: f32,
    }

    fn render(
        plugin: &mut ExecutiveOverride,
        parameters: &[f32],
        sidechain: Option<&[f32]>,
        events: &[NoteEvent],
        transport: TransportSnapshot,
        frame_count: usize,
    ) -> Rendered {
        let constant_input = vec![0.5f32; frame_count];
        let mut output_left = vec![0.0; frame_count];
        let mut output_right = vec![0.0; frame_count];
        let mut meters = vec![None; PARAMETERS.len()];
        plugin.process(ProcessBlock {
            sample_rate: SAMPLE_RATE,
            input_left: &constant_input,
            input_right: &constant_input,
            sidechain: sidechain.map(|key| (key, key)),
            output_left: &mut output_left,
            output_right: &mut output_right,
            parameters,
            meters: &mut meters,
            events,
            transport,
        });
        Rendered {
            output_left,
            deepest_reduction: meters[GAIN_REDUCTION_METER].unwrap_or(-1.0),
        }
    }

    fn decibels_at(rendered: &Rendered, frame_index: usize) -> f32 {
        gain_to_decibels(rendered.output_left[frame_index] / 0.5)
    }

    #[test]
    fn sidechain_ducking_follows_the_key() {
        // A 50 ms kick-like burst at 0.25 s in a one-second key signal.
        let mut key = vec![0.0f32; 48_000];
        for (frame_index, sample) in key.iter_mut().enumerate().skip(12_000).take(2_400) {
            *sample = (frame_index as f32 * std::f32::consts::TAU * 60.0 / SAMPLE_RATE).sin() * 0.9;
        }
        let parameters = parameters_for(MODE_SIDECHAIN);
        let rendered = render(
            &mut ExecutiveOverride::new(SAMPLE_RATE),
            &parameters,
            Some(&key),
            &[],
            TransportSnapshot::default(),
            48_000,
        );
        assert!(decibels_at(&rendered, 6_000).abs() < 0.01, "untouched before the kick");
        assert!(
            decibels_at(&rendered, 13_000) < -11.0,
            "ducked during the kick: {}",
            decibels_at(&rendered, 13_000)
        );
        assert!(decibels_at(&rendered, 40_000).abs() < 0.01, "recovered after release");
        assert!((rendered.deepest_reduction - 12.0).abs() < 0.2);
        assert!(rendered.output_left.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn an_absent_sidechain_never_ducks() {
        let parameters = parameters_for(MODE_SIDECHAIN);
        let rendered = render(
            &mut ExecutiveOverride::new(SAMPLE_RATE),
            &parameters,
            None,
            &[],
            TransportSnapshot::default(),
            24_000,
        );
        assert!(rendered.output_left.iter().all(|sample| (*sample - 0.5).abs() < 1e-6));
        assert_eq!(rendered.deepest_reduction, 0.0);
    }

    #[test]
    fn tempo_pump_ducks_on_every_quarter_note() {
        // 120 BPM: a quarter note every 24 000 frames; start one beat into the bar.
        let transport = TransportSnapshot {
            tempo_bpm: Some(120.0),
            project_quarter_notes: Some(1.0),
            bar_start_quarter_notes: Some(0.0),
            time_signature: Some((4, 4)),
            playing: true,
        };
        let parameters = parameters_for(MODE_TEMPO_PUMP);
        let rendered = render(&mut ExecutiveOverride::new(SAMPLE_RATE), &parameters, None, &[], transport, 96_000);
        for beat_index in 0..4 {
            let beat_start = beat_index * 24_000;
            // Deepest just after the beat (after the 1 ms attack), recovered before the next one.
            let just_after_beat = decibels_at(&rendered, beat_start + 200);
            let late_in_beat = decibels_at(&rendered, beat_start + 20_000);
            assert!(just_after_beat < -11.0, "beat {beat_index}: {just_after_beat} dB");
            assert!(late_in_beat > -0.01, "beat {beat_index} recovered to {late_in_beat} dB");
        }
    }

    #[test]
    fn eighth_note_rate_and_bar_offsets_line_up() {
        let mut parameters = parameters_for(MODE_TEMPO_PUMP);
        parameters[RATE] = 1.0;
        // Mid-bar start (1.25 quarter notes in): the next eighth-note duck is 0.25 quarter notes (6 000 frames) away.
        let transport = TransportSnapshot {
            tempo_bpm: Some(120.0),
            project_quarter_notes: Some(9.25),
            bar_start_quarter_notes: Some(8.0),
            time_signature: Some((4, 4)),
            playing: true,
        };
        let rendered = render(&mut ExecutiveOverride::new(SAMPLE_RATE), &parameters, None, &[], transport, 24_000);
        assert!(decibels_at(&rendered, 5_900) > -1.0);
        assert!(decibels_at(&rendered, 6_200) < -11.0);
        assert!(decibels_at(&rendered, 18_200) < -11.0);
    }

    #[test]
    fn midi_note_ons_are_executive_decisions() {
        let parameters = parameters_for(MODE_MIDI_TRIGGER);
        let events = [NoteEvent {
            sample_offset: 10_000,
            kind: NoteEventKind::On { pitch: 36, velocity: 1.0 },
        }];
        let rendered = render(
            &mut ExecutiveOverride::new(SAMPLE_RATE),
            &parameters,
            None,
            &events,
            TransportSnapshot::default(),
            48_000,
        );
        assert!(decibels_at(&rendered, 9_000).abs() < 0.01);
        assert!(decibels_at(&rendered, 10_300) < -11.0);
        assert!(decibels_at(&rendered, 30_000).abs() < 0.01);
    }

    #[test]
    fn the_curve_shapes_the_recovery() {
        let linear_midpoint = duck_shape_decibels(0.05 + 0.08, 12.0, 0.05, 0.16, 0.0);
        let snappy_midpoint = duck_shape_decibels(0.05 + 0.08, 12.0, 0.05, 0.16, 1.0);
        assert!((linear_midpoint - 6.0).abs() < 1e-4);
        assert!(snappy_midpoint < 1.0);
        assert_eq!(duck_shape_decibels(0.01, 12.0, 0.05, 0.16, 0.5), 12.0);
        assert_eq!(duck_shape_decibels(10.0, 12.0, 0.05, 0.16, 0.5), 0.0);
    }
}
