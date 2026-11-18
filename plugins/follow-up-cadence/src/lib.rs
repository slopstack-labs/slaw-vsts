//! # Follow-Up Cadence
//!
//! Circles back on every 1/8 note. A stereo delay for stakeholders who did not
//! hear it the first time:
//!
//! - **Sync**: a free-running time in milliseconds, or a note division read
//!   from the host's tempo (120 BPM when the host keeps its tempo to itself).
//! - **Feedback** is hard-capped at 90 %, so every follow-up is quieter than the
//!   last and the thread always goes cold eventually.
//! - **Ping-Pong** routes each repeat to the other side of the room: 0 % keeps
//!   straight stereo echoes, 100 % alternates left and right.
//! - **Low Cut** and **High Cut** sit inside the feedback loop, so each repeat is
//!   thinner and darker than the one before, like a forwarded email.
//! - **Ducking** follows the dry input like a sidechain: the repeats step back
//!   while the input is talking and swell back in between phrases.
//!
//! Delay-time changes (automation, a tempo change, a new division) never jump
//! the read head: the incumbent head hands the account over to a successor head
//! across a short raised-cosine crossfade, so there is no click and no pitch
//! warble. Reads are 4-point Hermite interpolated, so triplet divisions land
//! between samples without dulling the loop.

use synergy_substrate::dsp::{decibels_to_gain, flush_denormal, gain_to_decibels, smoothing_coefficient, BiquadCoefficients, BiquadState};
use synergy_substrate::{export_synergy_plugin, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin, TransportSnapshot};

pub const SYNC: usize = 0;
pub const TIME: usize = 1;
pub const DIVISION: usize = 2;
pub const FEEDBACK: usize = 3;
pub const PING_PONG: usize = 4;
pub const LOW_CUT: usize = 5;
pub const HIGH_CUT: usize = 6;
pub const DUCKING: usize = 7;
pub const MIX: usize = 8;

pub const SYNC_FREE: f32 = 0.0;
pub const SYNC_TEMPO: f32 = 1.0;

pub const DIVISION_SIXTEENTH: f32 = 0.0;
pub const DIVISION_EIGHTH_TRIPLET: f32 = 1.0;
pub const DIVISION_EIGHTH: f32 = 2.0;
pub const DIVISION_EIGHTH_DOTTED: f32 = 3.0;
pub const DIVISION_QUARTER_TRIPLET: f32 = 4.0;
pub const DIVISION_QUARTER: f32 = 5.0;
pub const DIVISION_QUARTER_DOTTED: f32 = 6.0;
pub const DIVISION_HALF: f32 = 7.0;
pub const DIVISION_BAR: f32 = 8.0;

const SYNC_MODES: &[&str] = &["Free", "Tempo"];
const DIVISIONS: &[&str] = &["1/16", "1/8 triplet", "1/8", "1/8 dotted", "1/4 triplet", "1/4", "1/4 dotted", "1/2", "1 bar"];

/// The feedback knob stops here: below 100 %, so every repeat is quieter than the one before.
const MAXIMUM_FEEDBACK_PERCENT: f64 = 90.0;
const MAXIMUM_FEEDBACK: f32 = (MAXIMUM_FEEDBACK_PERCENT / 100.0) as f32;

static PARAMETERS: [ParameterSpecification; 9] = [
    ParameterSpecification::list(0, "Sync", SYNC_MODES, 1),
    ParameterSpecification::logarithmic(1, "Time", "ms", 1.0, 2_000.0, 375.0),
    ParameterSpecification::list(2, "Division", DIVISIONS, 2),
    ParameterSpecification::linear(3, "Feedback", "%", 0.0, MAXIMUM_FEEDBACK_PERCENT, 40.0),
    ParameterSpecification::linear(4, "Ping-Pong", "%", 0.0, 100.0, 50.0),
    ParameterSpecification::logarithmic(5, "Low Cut", "Hz", 20.0, 2_000.0, 150.0),
    ParameterSpecification::logarithmic(6, "High Cut", "Hz", 1_000.0, 20_000.0, 6_500.0),
    ParameterSpecification::linear(7, "Ducking", "dB", 0.0, 24.0, 0.0),
    ParameterSpecification::linear(8, "Mix", "%", 0.0, 100.0, 30.0),
];

/// The longest follow-up on the calendar: one 4/4 bar at 30 BPM (8 s), which also covers
/// the 2 s free-time maximum. Longer requests (a 7/4 bar at 30 BPM) are clamped to it.
pub const MAXIMUM_DELAY_SECONDS: f64 = 8.0;

/// How long the worst case keeps ringing: the maximum delay at maximum feedback, until the
/// repeats are 60 dB down. `8 s · ln(10⁻³) / ln(0.9)` ≈ 524.5 s; a test keeps this honest.
const TAIL_SECONDS: f64 = 525.0;

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Follow-Up Cadence",
    class_identifier: [0xF6C9EF6F, 0x36114E3A, 0xBD5F4ABE, 0xFC8A8D97],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Delay",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: false,
    note_input: false,
    tail_seconds: TAIL_SECONDS,
};

/// Tempo used when the host does not say (or says something no metronome could play).
const FALLBACK_TEMPO_BPM: f64 = 120.0;
/// The Hermite read needs one sample behind and two ahead of the read position, all already written.
const MINIMUM_DELAY_FRAMES: f64 = 4.0;
/// Extra delay-line slots beyond the longest delay, so the interpolator never reads the write head.
const DELAY_LINE_HEADROOM_FRAMES: usize = 8;
/// How long the incumbent read head takes to hand over to its successor when the delay time changes.
const HANDOFF_MILLISECONDS: f32 = 50.0;
/// Delay changes smaller than this (in frames) are rounding noise, not a reorg.
const HANDOFF_TOLERANCE_FRAMES: f64 = 1.0e-3;
/// Feedback, ping-pong and mix glide over this, so automating them never zippers.
const PARAMETER_GLIDE_MILLISECONDS: f32 = 20.0;
/// The input-level follower tracks the waveform's peaks and lets go over this.
const LEVEL_FOLLOWER_RELEASE_MILLISECONDS: f32 = 30.0;
/// The repeats step back this fast when the input starts talking…
const DUCKING_ATTACK_MILLISECONDS: f32 = 10.0;
/// …and swell back in over this once it stops.
const DUCKING_RELEASE_MILLISECONDS: f32 = 250.0;
/// Input below this level (dBFS) leaves the repeats alone.
const DUCKING_FLOOR_DECIBELS: f32 = -48.0;
/// Input this many dB above the floor (−18 dBFS) ducks the repeats by the full Ducking amount.
const DUCKING_ENGAGE_RANGE_DECIBELS: f32 = 30.0;

/// Low Cut fully counter-clockwise (20 Hz) takes the highpass out of the loop entirely.
const LOW_CUT_OFF_AT_OR_BELOW_HZ: f32 = 20.5;
/// High Cut fully clockwise (20 kHz) takes the lowpass out of the loop entirely.
const HIGH_CUT_OFF_AT_OR_ABOVE_HZ: f32 = 19_900.0;

/// Quarter notes per repeat for a `Division` list index, given the bar length in quarter notes.
pub fn division_quarter_notes(division_index: f32, bar_quarter_notes: f64) -> f64 {
    match division_index.round() as i32 {
        0 => 0.25,
        1 => 1.0 / 3.0,
        3 => 0.75,
        4 => 2.0 / 3.0,
        5 => 1.0,
        6 => 1.5,
        7 => 2.0,
        8 => bar_quarter_notes,
        _ => 0.5,
    }
}

/// The host's tempo when it is a tempo, otherwise the fallback.
pub fn effective_tempo_bpm(transport: &TransportSnapshot) -> f64 {
    transport
        .tempo_bpm
        .filter(|tempo_bpm| tempo_bpm.is_finite() && *tempo_bpm > 0.0)
        .unwrap_or(FALLBACK_TEMPO_BPM)
}

/// Quarter notes per bar from the host's time signature (4/4 when it does not say).
pub fn bar_quarter_notes(transport: &TransportSnapshot) -> f64 {
    transport
        .time_signature
        .filter(|(numerator, denominator)| *numerator > 0 && *denominator > 0)
        .map(|(numerator, denominator)| 4.0 * f64::from(numerator) / f64::from(denominator))
        .unwrap_or(4.0)
}

/// The delay the knobs and the host's transport ask for, in seconds, before any clamping.
pub fn requested_delay_seconds(parameters: &[f32], transport: &TransportSnapshot) -> f64 {
    if parameters[SYNC].round() == SYNC_TEMPO {
        division_quarter_notes(parameters[DIVISION], bar_quarter_notes(transport)) * 60.0 / effective_tempo_bpm(transport)
    } else {
        f64::from(parameters[TIME]) * 0.001
    }
}

/// The loop's low cut: a Butterworth highpass, or no filter at all with the knob fully open (20 Hz).
pub fn loop_low_cut_coefficients(low_cut_hz: f32, sample_rate: f32) -> BiquadCoefficients {
    if low_cut_hz <= LOW_CUT_OFF_AT_OR_BELOW_HZ {
        BiquadCoefficients::IDENTITY
    } else {
        BiquadCoefficients::highpass(f64::from(low_cut_hz), std::f64::consts::FRAC_1_SQRT_2, f64::from(sample_rate))
    }
}

/// The loop's high cut: a Butterworth lowpass, or no filter at all with the knob fully open (20 kHz).
pub fn loop_high_cut_coefficients(high_cut_hz: f32, sample_rate: f32) -> BiquadCoefficients {
    if high_cut_hz >= HIGH_CUT_OFF_AT_OR_ABOVE_HZ {
        BiquadCoefficients::IDENTITY
    } else {
        BiquadCoefficients::lowpass(f64::from(high_cut_hz), std::f64::consts::FRAC_1_SQRT_2, f64::from(sample_rate))
    }
}

/// Reads `delay_line` `delay_frames` behind the write head with 4-point, 3rd-order Hermite
/// interpolation: exact on whole frames, and far flatter than linear interpolation between
/// them, which matters when a fractional read is repeated a hundred times around a loop.
fn read_hermite(delay_line: &[f32], write_position: usize, delay_frames: f64) -> f32 {
    let line_length = delay_line.len();
    let read_position = write_position as f64 + line_length as f64 - delay_frames;
    let whole_position = read_position.floor();
    let fraction = (read_position - whole_position) as f32;
    let current_index = whole_position as usize % line_length;
    let previous_sample = delay_line[(current_index + line_length - 1) % line_length];
    let current_sample = delay_line[current_index];
    let next_sample = delay_line[(current_index + 1) % line_length];
    let after_next_sample = delay_line[(current_index + 2) % line_length];
    let slope_coefficient = 0.5 * (next_sample - previous_sample);
    let curvature_coefficient = previous_sample - 2.5 * current_sample + 2.0 * next_sample - 0.5 * after_next_sample;
    let cubic_coefficient = 0.5 * (after_next_sample - previous_sample) + 1.5 * (current_sample - next_sample);
    ((cubic_coefficient * fraction + curvature_coefficient) * fraction + slope_coefficient) * fraction + current_sample
}

/// One step of a one-pole glide toward `target_value`, landing on it exactly once close enough
/// (so a glide to zero ends at zero instead of crawling through the denormals).
#[inline]
fn glide_toward(current_value: f32, target_value: f32, glide_coefficient: f32) -> f32 {
    let next_value = target_value + glide_coefficient * (current_value - target_value);
    if (next_value - target_value).abs() < 1.0e-6 {
        target_value
    } else {
        next_value
    }
}

pub struct FollowUpCadence {
    sample_rate: f32,
    left_delay_line: Vec<f32>,
    right_delay_line: Vec<f32>,
    /// The slot the current frame is written to; the sample `n` frames ago lives `n` slots behind it.
    write_position: usize,
    /// The delay, in frames, of the read head that currently owns the account.
    incumbent_delay_frames: f64,
    /// The delay the account is being handed to, while a handoff is in progress.
    successor_delay_frames: f64,
    /// 0..1 through the current handoff.
    handoff_progress: f32,
    handoff_in_progress: bool,
    left_low_cut: BiquadState,
    right_low_cut: BiquadState,
    left_high_cut: BiquadState,
    right_high_cut: BiquadState,
    /// Peak follower on the dry input, for ducking.
    input_level_envelope: f32,
    /// Ducking currently applied to the repeats, in dB (positive = quieter).
    applied_ducking_decibels: f32,
    smoothed_feedback: f32,
    smoothed_ping_pong: f32,
    smoothed_mix: f32,
    /// False until the first block: then the delay and the smoothed knobs snap to their targets
    /// instead of gliding in from zero.
    onboarded: bool,
}

impl FollowUpCadence {
    /// The longest delay the line can serve, in frames.
    fn maximum_delay_frames(&self) -> f64 {
        (self.left_delay_line.len() - DELAY_LINE_HEADROOM_FRAMES) as f64
    }
}

impl SynergyPlugin for FollowUpCadence {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        let safe_sample_rate = if sample_rate.is_finite() && sample_rate >= 1_000.0 {
            sample_rate
        } else {
            44_100.0
        };
        let line_length = (MAXIMUM_DELAY_SECONDS * f64::from(safe_sample_rate)).ceil() as usize + DELAY_LINE_HEADROOM_FRAMES;
        Self {
            sample_rate: safe_sample_rate,
            left_delay_line: vec![0.0; line_length],
            right_delay_line: vec![0.0; line_length],
            write_position: 0,
            incumbent_delay_frames: MINIMUM_DELAY_FRAMES,
            successor_delay_frames: MINIMUM_DELAY_FRAMES,
            handoff_progress: 0.0,
            handoff_in_progress: false,
            left_low_cut: BiquadState::default(),
            right_low_cut: BiquadState::default(),
            left_high_cut: BiquadState::default(),
            right_high_cut: BiquadState::default(),
            input_level_envelope: 0.0,
            applied_ducking_decibels: 0.0,
            smoothed_feedback: 0.0,
            smoothed_ping_pong: 0.0,
            smoothed_mix: 0.0,
            onboarded: false,
        }
    }

    fn reset(&mut self) {
        // No allocation: the lines keep their capacity and are cleared in place.
        self.left_delay_line.fill(0.0);
        self.right_delay_line.fill(0.0);
        self.write_position = 0;
        self.handoff_progress = 0.0;
        self.handoff_in_progress = false;
        self.left_low_cut = BiquadState::default();
        self.right_low_cut = BiquadState::default();
        self.left_high_cut = BiquadState::default();
        self.right_high_cut = BiquadState::default();
        self.input_level_envelope = 0.0;
        self.applied_ducking_decibels = 0.0;
        self.onboarded = false;
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        let parameters = block.parameters;
        let sample_rate = self.sample_rate;
        let requested_delay_frames = requested_delay_seconds(parameters, &block.transport) * f64::from(sample_rate);
        let target_delay_frames = if requested_delay_frames.is_finite() {
            requested_delay_frames.clamp(MINIMUM_DELAY_FRAMES, self.maximum_delay_frames())
        } else {
            self.incumbent_delay_frames
        };
        let target_feedback = (parameters[FEEDBACK] / 100.0).clamp(0.0, MAXIMUM_FEEDBACK);
        let target_ping_pong = (parameters[PING_PONG] / 100.0).clamp(0.0, 1.0);
        let target_mix = (parameters[MIX] / 100.0).clamp(0.0, 1.0);
        let ducking_decibels = parameters[DUCKING].clamp(0.0, 48.0);

        if !self.onboarded {
            self.incumbent_delay_frames = target_delay_frames;
            self.smoothed_feedback = target_feedback;
            self.smoothed_ping_pong = target_ping_pong;
            self.smoothed_mix = target_mix;
            self.onboarded = true;
        }

        let low_cut_coefficients = loop_low_cut_coefficients(parameters[LOW_CUT], sample_rate);
        let high_cut_coefficients = loop_high_cut_coefficients(parameters[HIGH_CUT], sample_rate);
        let glide_coefficient = smoothing_coefficient(PARAMETER_GLIDE_MILLISECONDS, sample_rate);
        let level_release_coefficient = smoothing_coefficient(LEVEL_FOLLOWER_RELEASE_MILLISECONDS, sample_rate);
        let ducking_attack_coefficient = smoothing_coefficient(DUCKING_ATTACK_MILLISECONDS, sample_rate);
        let ducking_release_coefficient = smoothing_coefficient(DUCKING_RELEASE_MILLISECONDS, sample_rate);
        let handoff_increment = 1.0 / (HANDOFF_MILLISECONDS * 0.001 * sample_rate);
        let line_length = self.left_delay_line.len();

        for frame_index in 0..block.frames() {
            // A non-finite input would circle the loop forever (and poison the dry path): it is not invited back.
            let left_input = block.input_left[frame_index];
            let right_input = block.input_right[frame_index];
            let left_input = if left_input.is_finite() { left_input } else { 0.0 };
            let right_input = if right_input.is_finite() { right_input } else { 0.0 };

            self.smoothed_feedback = glide_toward(self.smoothed_feedback, target_feedback, glide_coefficient);
            self.smoothed_ping_pong = glide_toward(self.smoothed_ping_pong, target_ping_pong, glide_coefficient);
            self.smoothed_mix = glide_toward(self.smoothed_mix, target_mix, glide_coefficient);

            // ── The read heads: one incumbent, and a successor while a handoff is in progress ──
            if !self.handoff_in_progress && (target_delay_frames - self.incumbent_delay_frames).abs() > HANDOFF_TOLERANCE_FRAMES {
                self.successor_delay_frames = target_delay_frames;
                self.handoff_progress = 0.0;
                self.handoff_in_progress = true;
            }
            let incumbent_left = read_hermite(&self.left_delay_line, self.write_position, self.incumbent_delay_frames);
            let incumbent_right = read_hermite(&self.right_delay_line, self.write_position, self.incumbent_delay_frames);
            let (left_tap, right_tap) = if self.handoff_in_progress {
                let successor_left = read_hermite(&self.left_delay_line, self.write_position, self.successor_delay_frames);
                let successor_right = read_hermite(&self.right_delay_line, self.write_position, self.successor_delay_frames);
                // Raised cosine: the weights always sum to one and both ends are flat.
                let successor_weight = 0.5 - 0.5 * (std::f32::consts::PI * self.handoff_progress).cos();
                self.handoff_progress += handoff_increment;
                if self.handoff_progress >= 1.0 {
                    self.incumbent_delay_frames = self.successor_delay_frames;
                    self.handoff_in_progress = false;
                }
                (
                    incumbent_left + successor_weight * (successor_left - incumbent_left),
                    incumbent_right + successor_weight * (successor_right - incumbent_right),
                )
            } else {
                (incumbent_left, incumbent_right)
            };

            // ── The loop: every repeat is low-cut and high-cut again, so it thins and darkens ──
            let conditioned_left =
                self.left_high_cut
                    .process(&high_cut_coefficients, self.left_low_cut.process(&low_cut_coefficients, f64::from(left_tap))) as f32;
            let conditioned_right =
                self.right_high_cut
                    .process(&high_cut_coefficients, self.right_low_cut.process(&low_cut_coefficients, f64::from(right_tap))) as f32;

            // Ping-pong: the input enters on the left only and each repeat crosses the room.
            // The routing matrix [[1-p, p], [p, 1-p]] has no gain above one, so the loop gain stays at Feedback.
            let ping_pong = self.smoothed_ping_pong;
            let straight_share = 1.0 - ping_pong;
            let mono_input = 0.5 * (left_input + right_input);
            let left_injection = straight_share * left_input + ping_pong * mono_input;
            let right_injection = straight_share * right_input;
            let left_returning = straight_share * conditioned_left + ping_pong * conditioned_right;
            let right_returning = straight_share * conditioned_right + ping_pong * conditioned_left;
            self.left_delay_line[self.write_position] = flush_denormal(left_injection + self.smoothed_feedback * left_returning);
            self.right_delay_line[self.write_position] = flush_denormal(right_injection + self.smoothed_feedback * right_returning);
            self.write_position += 1;
            if self.write_position == line_length {
                self.write_position = 0;
            }

            // ── Ducking: a sidechain follower on the dry input pushes the repeats back ──
            let input_peak = left_input.abs().max(right_input.abs());
            self.input_level_envelope = if input_peak > self.input_level_envelope {
                input_peak
            } else {
                flush_denormal(input_peak + level_release_coefficient * (self.input_level_envelope - input_peak))
            };
            let target_ducking_decibels = if ducking_decibels > 0.0 {
                let engagement = ((gain_to_decibels(self.input_level_envelope) - DUCKING_FLOOR_DECIBELS) / DUCKING_ENGAGE_RANGE_DECIBELS).clamp(0.0, 1.0);
                ducking_decibels * engagement
            } else {
                0.0
            };
            let ducking_coefficient = if target_ducking_decibels > self.applied_ducking_decibels {
                ducking_attack_coefficient
            } else {
                ducking_release_coefficient
            };
            self.applied_ducking_decibels = target_ducking_decibels + ducking_coefficient * (self.applied_ducking_decibels - target_ducking_decibels);
            if (self.applied_ducking_decibels - target_ducking_decibels).abs() < 1.0e-4 {
                self.applied_ducking_decibels = target_ducking_decibels;
            }
            let ducking_gain = if self.applied_ducking_decibels > 0.0 {
                decibels_to_gain(-self.applied_ducking_decibels)
            } else {
                1.0
            };

            let wet_left = flush_denormal(conditioned_left * ducking_gain);
            let wet_right = flush_denormal(conditioned_right * ducking_gain);
            let wet_mix = self.smoothed_mix;
            let dry_mix = 1.0 - wet_mix;
            block.output_left[frame_index] = left_input * dry_mix + wet_left * wet_mix;
            block.output_right[frame_index] = right_input * dry_mix + wet_right * wet_mix;
        }
    }
}

export_synergy_plugin!(FollowUpCadence);

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 44_100.0;
    const BLOCK_FRAMES: usize = 512;

    fn default_parameters() -> Vec<f32> {
        PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect()
    }

    /// Fully wet, no feedback, no ping-pong, filters wide open, no ducking: one clean echo.
    fn single_echo_parameters() -> Vec<f32> {
        let mut parameters = default_parameters();
        parameters[FEEDBACK] = 0.0;
        parameters[PING_PONG] = 0.0;
        parameters[LOW_CUT] = 20.0;
        parameters[HIGH_CUT] = 20_000.0;
        parameters[DUCKING] = 0.0;
        parameters[MIX] = 100.0;
        parameters
    }

    fn transport_at(tempo_bpm: f64) -> TransportSnapshot {
        TransportSnapshot {
            tempo_bpm: Some(tempo_bpm),
            project_quarter_notes: Some(0.0),
            bar_start_quarter_notes: Some(0.0),
            time_signature: Some((4, 4)),
            playing: true,
        }
    }

    fn impulse(frame_count: usize) -> Vec<f32> {
        let mut signal = vec![0.0f32; frame_count];
        signal[0] = 1.0;
        signal
    }

    /// Renders in host-sized blocks; `parameters_for_block` may automate knobs per block.
    fn render_with(
        plugin: &mut FollowUpCadence,
        input_left: &[f32],
        input_right: &[f32],
        mut parameters_for_block: impl FnMut(usize) -> (Vec<f32>, TransportSnapshot),
    ) -> (Vec<f32>, Vec<f32>) {
        let frame_count = input_left.len();
        let mut output_left = vec![0.0f32; frame_count];
        let mut output_right = vec![0.0f32; frame_count];
        let mut meters = vec![None; PARAMETERS.len()];
        let mut block_start = 0;
        let mut block_index = 0;
        while block_start < frame_count {
            let block_end = (block_start + BLOCK_FRAMES).min(frame_count);
            let (parameters, transport) = parameters_for_block(block_index);
            plugin.process(ProcessBlock {
                sample_rate: plugin.sample_rate,
                input_left: &input_left[block_start..block_end],
                input_right: &input_right[block_start..block_end],
                sidechain: None,
                output_left: &mut output_left[block_start..block_end],
                output_right: &mut output_right[block_start..block_end],
                parameters: &parameters,
                meters: &mut meters,
                events: &[],
                transport,
            });
            block_start = block_end;
            block_index += 1;
        }
        (output_left, output_right)
    }

    fn render(plugin: &mut FollowUpCadence, parameters: &[f32], input_left: &[f32], input_right: &[f32], transport: TransportSnapshot) -> (Vec<f32>, Vec<f32>) {
        render_with(plugin, input_left, input_right, |_| (parameters.to_vec(), transport))
    }

    /// Frame and absolute value of the loudest sample in `frame_range`.
    fn loudest_in(signal: &[f32], frame_range: std::ops::Range<usize>) -> (usize, f32) {
        frame_range
            .map(|frame_index| (frame_index, signal[frame_index].abs()))
            .fold((0, 0.0), |loudest, candidate| if candidate.1 > loudest.1 { candidate } else { loudest })
    }

    fn root_mean_square(signal: &[f32]) -> f32 {
        (signal.iter().map(|sample| sample * sample).sum::<f32>() / signal.len() as f32).sqrt()
    }

    /// Deterministic white noise in -amplitude..amplitude.
    fn noise(frame_count: usize, amplitude: f32, mut generator_state: u32) -> Vec<f32> {
        (0..frame_count)
            .map(|_| {
                generator_state = generator_state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((generator_state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn is_subnormal(sample: f32) -> bool {
        sample != 0.0 && sample.abs() < f32::MIN_POSITIVE
    }

    #[test]
    fn parameter_ids_are_sequential_and_documented() {
        let titles: Vec<&str> = PARAMETERS.iter().map(|parameter| parameter.title).collect();
        assert_eq!(
            titles,
            ["Sync", "Time", "Division", "Feedback", "Ping-Pong", "Low Cut", "High Cut", "Ducking", "Mix"]
        );
        for (parameter_index, parameter) in PARAMETERS.iter().enumerate() {
            assert_eq!(parameter.identifier as usize, parameter_index);
        }
        assert_eq!(PARAMETERS[DIVISION].format_plain(f64::from(DIVISION_EIGHTH)), "1/8");
        assert_eq!(PARAMETERS[SYNC].format_plain(f64::from(SYNC_TEMPO)), "Tempo");
        assert!(PARAMETERS[FEEDBACK].maximum < 100.0, "feedback must stay below 100 %");
    }

    #[test]
    fn an_eighth_note_at_120_bpm_circles_back_after_exactly_11025_frames() {
        let parameters = single_echo_parameters();
        assert_eq!(parameters[SYNC], SYNC_TEMPO);
        assert_eq!(parameters[DIVISION], DIVISION_EIGHTH);
        let input = impulse(20_000);
        for transport in [transport_at(120.0), TransportSnapshot::default()] {
            let (output_left, output_right) = render(&mut FollowUpCadence::new(SAMPLE_RATE), &parameters, &input, &input, transport);
            for output in [&output_left, &output_right] {
                let (echo_frame, echo_level) = loudest_in(output, 0..20_000);
                assert!(echo_frame.abs_diff(11_025) <= 1, "echo at frame {echo_frame}");
                assert!(echo_level > 0.9, "echo level {echo_level}");
                assert!(loudest_in(output, 0..11_000).1 < 1.0e-3, "nothing before the follow-up");
            }
        }
    }

    #[test]
    fn divisions_follow_the_tempo_and_the_time_signature() {
        let common_time = transport_at(120.0);
        assert_eq!(division_quarter_notes(DIVISION_SIXTEENTH, 4.0), 0.25);
        assert!((division_quarter_notes(DIVISION_EIGHTH_TRIPLET, 4.0) - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(division_quarter_notes(DIVISION_EIGHTH_DOTTED, 4.0), 0.75);
        assert!((division_quarter_notes(DIVISION_QUARTER_TRIPLET, 4.0) - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(division_quarter_notes(DIVISION_QUARTER, 4.0), 1.0);
        assert_eq!(division_quarter_notes(DIVISION_QUARTER_DOTTED, 4.0), 1.5);
        assert_eq!(division_quarter_notes(DIVISION_HALF, 4.0), 2.0);
        assert_eq!(division_quarter_notes(DIVISION_BAR, bar_quarter_notes(&common_time)), 4.0);
        let six_eight = TransportSnapshot {
            time_signature: Some((6, 8)),
            ..common_time
        };
        assert_eq!(bar_quarter_notes(&six_eight), 3.0);
        let hostile_tempo = TransportSnapshot {
            tempo_bpm: Some(f64::NAN),
            ..common_time
        };
        assert_eq!(effective_tempo_bpm(&hostile_tempo), FALLBACK_TEMPO_BPM);

        // One 4/4 bar at 30 BPM is the longest follow-up, and the line has room for all of it.
        let mut parameters = single_echo_parameters();
        parameters[DIVISION] = DIVISION_BAR;
        let slowest_bar_seconds = requested_delay_seconds(&parameters, &transport_at(30.0));
        assert_eq!(slowest_bar_seconds, MAXIMUM_DELAY_SECONDS);
        let plugin = FollowUpCadence::new(SAMPLE_RATE);
        assert!(plugin.maximum_delay_frames() >= MAXIMUM_DELAY_SECONDS * f64::from(SAMPLE_RATE));
    }

    #[test]
    fn a_whole_bar_at_30_bpm_echoes_after_eight_seconds() {
        let sample_rate = 8_000.0;
        let mut parameters = single_echo_parameters();
        parameters[DIVISION] = DIVISION_BAR;
        let input = impulse(70_000);
        let (output_left, _) = render(&mut FollowUpCadence::new(sample_rate), &parameters, &input, &input, transport_at(30.0));
        let (echo_frame, echo_level) = loudest_in(&output_left, 0..70_000);
        assert!(echo_frame.abs_diff(64_000) <= 1, "echo at frame {echo_frame}");
        assert!(echo_level > 0.9);
    }

    #[test]
    fn free_mode_time_matches_the_samples() {
        for (sample_rate, time_milliseconds, expected_frame) in [
            (44_100.0, 250.0, 11_025),
            (48_000.0, 250.0, 12_000),
            (48_000.0, 10.0, 480),
            (96_000.0, 2_000.0, 192_000),
        ] {
            let mut parameters = single_echo_parameters();
            parameters[SYNC] = SYNC_FREE;
            parameters[TIME] = time_milliseconds;
            let frame_count = expected_frame + 2_000;
            let input = impulse(frame_count);
            let (output_left, output_right) = render(&mut FollowUpCadence::new(sample_rate), &parameters, &input, &input, transport_at(97.0));
            for output in [&output_left, &output_right] {
                let (echo_frame, echo_level) = loudest_in(output, 0..frame_count);
                assert!(
                    echo_frame.abs_diff(expected_frame) <= 1,
                    "{time_milliseconds} ms at {sample_rate} Hz echoed at {echo_frame}"
                );
                assert!(echo_level > 0.9);
            }
        }
    }

    #[test]
    fn full_ping_pong_alternates_the_repeats_between_channels() {
        let mut parameters = single_echo_parameters();
        parameters[FEEDBACK] = 60.0;
        parameters[PING_PONG] = 100.0;
        let input = impulse(11_025 * 5);
        let (output_left, output_right) = render(&mut FollowUpCadence::new(SAMPLE_RATE), &parameters, &input, &input, transport_at(120.0));
        for repeat_number in 1..=4usize {
            let window = repeat_number * 11_025 - 4..repeat_number * 11_025 + 5;
            let left_level = loudest_in(&output_left, window.clone()).1;
            let right_level = loudest_in(&output_right, window).1;
            let expected_level = 0.6f32.powi(repeat_number as i32 - 1);
            let (speaking, silent) = if repeat_number % 2 == 1 {
                (left_level, right_level)
            } else {
                (right_level, left_level)
            };
            assert!(
                (speaking - expected_level).abs() < 0.05 * expected_level,
                "repeat {repeat_number}: {speaking} vs {expected_level}"
            );
            assert!(silent < 1.0e-3 * speaking, "repeat {repeat_number} leaked {silent} to the other side");
        }

        // With ping-pong at 0 % the same impulse echoes on both sides at once.
        parameters[PING_PONG] = 0.0;
        let (output_left, output_right) = render(&mut FollowUpCadence::new(SAMPLE_RATE), &parameters, &input, &input, transport_at(120.0));
        for repeat_number in 1..=3usize {
            let window = repeat_number * 11_025 - 4..repeat_number * 11_025 + 5;
            let left_level = loudest_in(&output_left, window.clone()).1;
            let right_level = loudest_in(&output_right, window).1;
            assert!(left_level > 0.1 && (left_level - right_level).abs() < 1.0e-6);
        }
    }

    #[test]
    fn feedback_decays_every_repeat() {
        let mut parameters = single_echo_parameters();
        parameters[FEEDBACK] = 50.0;
        let input = impulse(11_025 * 6);
        let (output_left, _) = render(&mut FollowUpCadence::new(SAMPLE_RATE), &parameters, &input, &input, transport_at(120.0));
        let repeat_levels: Vec<f32> = (1..=5usize)
            .map(|repeat_number| loudest_in(&output_left, repeat_number * 11_025 - 4..repeat_number * 11_025 + 5).1)
            .collect();
        for adjacent in repeat_levels.windows(2) {
            let decay_ratio = adjacent[1] / adjacent[0];
            assert!((0.45..0.55).contains(&decay_ratio), "repeats {repeat_levels:?}");
        }

        // The default loop filters take a little more off every lap.
        let mut filtered_parameters = parameters.clone();
        filtered_parameters[LOW_CUT] = 400.0;
        filtered_parameters[HIGH_CUT] = 2_000.0;
        let mut burst = vec![0.0f32; 11_025 * 6];
        for (frame_index, sample) in burst.iter_mut().enumerate().take(2_000) {
            *sample = (frame_index as f32 * 0.37).sin() * 0.5 + (frame_index as f32 * 2.1).sin() * 0.3;
        }
        let (filtered_left, _) = render(
            &mut FollowUpCadence::new(SAMPLE_RATE),
            &filtered_parameters,
            &burst,
            &burst,
            transport_at(120.0),
        );
        let lap_energy: Vec<f32> = (1..=3usize)
            .map(|repeat_number| root_mean_square(&filtered_left[repeat_number * 11_025..repeat_number * 11_025 + 2_500]))
            .collect();
        assert!(lap_energy[1] < 0.5 * lap_energy[0] && lap_energy[2] < 0.5 * lap_energy[1], "{lap_energy:?}");

        // Feedback 0 %: exactly one follow-up, then nothing.
        parameters[FEEDBACK] = 0.0;
        let (output_left, _) = render(&mut FollowUpCadence::new(SAMPLE_RATE), &parameters, &input, &input, transport_at(120.0));
        assert!(loudest_in(&output_left, 11_025 * 2 - 100..11_025 * 6).1 < 1.0e-4);
    }

    #[test]
    fn mix_zero_passes_the_input_through_bit_exact() {
        let mut parameters = default_parameters();
        parameters[MIX] = 0.0;
        parameters[FEEDBACK] = 90.0;
        parameters[DUCKING] = 24.0;
        let input_left = noise(30_000, 0.8, 7);
        let input_right = noise(30_000, 0.8, 99);
        let (output_left, output_right) = render(
            &mut FollowUpCadence::new(SAMPLE_RATE),
            &parameters,
            &input_left,
            &input_right,
            transport_at(133.0),
        );
        assert_eq!(output_left, input_left);
        assert_eq!(output_right, input_right);
    }

    #[test]
    fn ducking_holds_the_repeats_back_while_the_input_talks() {
        let talking_frames = 44_100;
        let total_frames = 44_100 * 5 / 2;
        let tone: Vec<f32> = (0..total_frames)
            .map(|frame_index| {
                if frame_index < talking_frames {
                    0.5 * (std::f32::consts::TAU * 440.0 * frame_index as f32 / SAMPLE_RATE).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let render_with_ducking = |ducking_decibels: f32| {
            let mut parameters = single_echo_parameters();
            parameters[SYNC] = SYNC_FREE;
            parameters[TIME] = 100.0;
            parameters[FEEDBACK] = 70.0;
            parameters[DUCKING] = ducking_decibels;
            render(&mut FollowUpCadence::new(SAMPLE_RATE), &parameters, &tone, &tone, transport_at(120.0)).0
        };
        let unducked = render_with_ducking(0.0);
        let ducked = render_with_ducking(18.0);
        let level_difference = |frame_range: std::ops::Range<usize>| {
            gain_to_decibels(root_mean_square(&ducked[frame_range.clone()])) - gain_to_decibels(root_mean_square(&unducked[frame_range]))
        };
        let while_talking = level_difference(22_050..44_100);
        assert!(while_talking < -16.0, "repeats ducked by only {while_talking} dB while the input talked");
        // A second after the input stops, the repeats have swelled back.
        let between_phrases = 44_100 * 2..44_100 * 2 + 8_820;
        let after_phrase = level_difference(between_phrases.clone());
        assert!(after_phrase > -1.0, "repeats still {after_phrase} dB down after the phrase");
        assert!(root_mean_square(&ducked[between_phrases]) > 1.0e-3, "and they are still audible");
    }

    #[test]
    fn automating_the_delay_time_never_clicks() {
        let frame_count = 44_100 * 2;
        let tone: Vec<f32> = (0..frame_count)
            .map(|frame_index| 0.5 * (std::f32::consts::TAU * 220.0 * frame_index as f32 / SAMPLE_RATE).sin())
            .collect();
        let largest_step = |signal: &[f32]| signal.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0f32, f32::max);
        let steady_slope = 0.5 * std::f32::consts::TAU * 220.0 / SAMPLE_RATE;

        // Free time jumps 300 → 120 → 1 500 ms, then the host halves its tempo in Tempo mode.
        let mut plugin = FollowUpCadence::new(SAMPLE_RATE);
        let (output_left, output_right) = render_with(&mut plugin, &tone, &tone, |block_index| {
            let mut parameters = single_echo_parameters();
            parameters[FEEDBACK] = 30.0;
            parameters[SYNC] = if block_index < 120 { SYNC_FREE } else { SYNC_TEMPO };
            parameters[TIME] = match block_index {
                0..=39 => 300.0,
                40..=79 => 120.0,
                _ => 1_500.0,
            };
            // Ramp the time continuously for a while too, the way an automation lane would.
            if (60..80).contains(&block_index) {
                parameters[TIME] = 120.0 + 10.0 * (block_index - 60) as f32;
            }
            let tempo_bpm = if block_index < 150 { 120.0 } else { 60.0 };
            (parameters, transport_at(tempo_bpm))
        });
        for output in [&output_left, &output_right] {
            let step = largest_step(output);
            assert!(step < 2.0 * steady_slope, "largest step {step} vs a steady slope of {steady_slope}");
        }
    }

    #[test]
    fn hostile_settings_never_produce_nan_or_infinity() {
        for sample_rate in [8_000.0, 44_100.0, 192_000.0] {
            let mut plugin = FollowUpCadence::new(sample_rate);
            let input_left = noise(40_000, 4.0, 3);
            let mut input_right = noise(40_000, 4.0, 5);
            input_right[1_000] = f32::NAN;
            input_right[2_000] = f32::INFINITY;
            let hostile_tempos = [Some(0.0), Some(-60.0), Some(f64::NAN), Some(1.0e9), Some(1.0), None];
            let (output_left, output_right) = render_with(&mut plugin, &input_left, &input_right, |block_index| {
                let mut parameters = default_parameters();
                parameters[SYNC] = (block_index % 2) as f32;
                parameters[TIME] = if block_index % 3 == 0 { 1.0 } else { 2_000.0 };
                parameters[DIVISION] = (block_index % 9) as f32;
                parameters[FEEDBACK] = MAXIMUM_FEEDBACK_PERCENT as f32;
                parameters[PING_PONG] = if block_index % 4 < 2 { 100.0 } else { 37.0 };
                parameters[LOW_CUT] = 2_000.0;
                parameters[HIGH_CUT] = 1_000.0;
                parameters[DUCKING] = 24.0;
                parameters[MIX] = 100.0;
                let transport = TransportSnapshot {
                    tempo_bpm: hostile_tempos[block_index % hostile_tempos.len()],
                    time_signature: if block_index % 5 == 0 { Some((0, 0)) } else { Some((7, 4)) },
                    ..TransportSnapshot::default()
                };
                (parameters, transport)
            });
            for output in [&output_left, &output_right] {
                assert!(output.iter().all(|sample| sample.is_finite() && sample.abs() < 1_000.0));
            }
        }
    }

    #[test]
    fn tails_flush_to_true_silence() {
        // The shortest delay at the highest feedback: hundreds of laps a second.
        let mut parameters = default_parameters();
        parameters[SYNC] = SYNC_FREE;
        parameters[TIME] = 1.0;
        parameters[FEEDBACK] = MAXIMUM_FEEDBACK_PERCENT as f32;
        parameters[MIX] = 100.0;
        let input = impulse(44_100 * 5);
        let mut plugin = FollowUpCadence::new(SAMPLE_RATE);
        let (output_left, output_right) = render(&mut plugin, &parameters, &input, &input, transport_at(120.0));
        for output in [&output_left, &output_right] {
            assert!(output.iter().all(|sample| !is_subnormal(*sample)), "a subnormal sample escaped");
            assert!(output[44_100 * 4..].iter().all(|sample| *sample == 0.0), "the tail never went quiet");
        }
        assert!(plugin
            .left_delay_line
            .iter()
            .chain(plugin.right_delay_line.iter())
            .all(|sample| !is_subnormal(*sample)));

        // Default settings, a burst of program material and a long silence: never subnormal, and fading.
        let mut burst = noise(44_100 * 20, 0.7, 11);
        burst[22_050..].fill(0.0);
        let mut plugin = FollowUpCadence::new(SAMPLE_RATE);
        let (output_left, output_right) = render(&mut plugin, &default_parameters(), &burst, &burst, transport_at(120.0));
        for output in [&output_left, &output_right] {
            assert!(output.iter().all(|sample| sample.is_finite() && !is_subnormal(*sample)));
            let first_repeat = root_mean_square(&output[22_050..44_100]);
            let last_second = root_mean_square(&output[44_100 * 19..]);
            assert!(last_second < first_repeat * 1.0e-3, "{last_second} vs {first_repeat}");
        }
        assert!(plugin
            .left_delay_line
            .iter()
            .chain(plugin.right_delay_line.iter())
            .all(|sample| !is_subnormal(*sample)));
    }

    #[test]
    fn reset_forgets_every_follow_up() {
        let mut parameters = single_echo_parameters();
        parameters[FEEDBACK] = 80.0;
        let input = noise(20_000, 0.5, 21);
        let mut plugin = FollowUpCadence::new(SAMPLE_RATE);
        render(&mut plugin, &parameters, &input, &input, transport_at(120.0));
        plugin.reset();
        let silence = vec![0.0f32; 30_000];
        let (output_left, output_right) = render(&mut plugin, &parameters, &silence, &silence, transport_at(120.0));
        assert!(output_left.iter().chain(output_right.iter()).all(|sample| *sample == 0.0));
    }

    #[test]
    fn tail_seconds_covers_the_worst_case_decay() {
        let laps_to_minus_sixty_decibels = (1.0e-3f64).ln() / f64::from(MAXIMUM_FEEDBACK).ln();
        let worst_case_seconds = MAXIMUM_DELAY_SECONDS * laps_to_minus_sixty_decibels;
        assert!(
            DESCRIPTION.tail_seconds >= worst_case_seconds,
            "{} < {worst_case_seconds}",
            DESCRIPTION.tail_seconds
        );
        assert!(DESCRIPTION.tail_seconds < worst_case_seconds * 1.05);
    }
}
