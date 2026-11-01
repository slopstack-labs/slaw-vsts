//! # Escalation Stack
//!
//! Takes a clean idea and escalates it to leadership at high gain. A complete
//! guitar amp in one slot: a tight control (input high-pass), one or two
//! asymmetric preamp stages run at 4x oversampling, a bass/middle/treble tone
//! stack with presence, a power-amp stage that compresses as it is pushed, and
//! a speaker cabinet (4x12 closed, 2x12 open, 1x12 combo, or direct) so the
//! output is a finished guitar tone rather than fizz waiting for an impulse
//! response that never got budget approval.

use synergy_substrate::dsp::{
    decibels_to_gain, flush_denormal, oversampling_filter, smoothing_coefficient, BiquadCoefficients, BiquadState, FourTimesOversampler,
};
use synergy_substrate::{export_synergy_plugin, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin};

pub const VOICING: usize = 0;
pub const GAIN: usize = 1;
pub const TIGHT: usize = 2;
pub const BASS: usize = 3;
pub const MIDDLE: usize = 4;
pub const TREBLE: usize = 5;
pub const PRESENCE: usize = 6;
pub const MASTER: usize = 7;
pub const CABINET: usize = 8;
pub const OUTPUT: usize = 9;

pub const VOICING_CLEAN: f32 = 0.0;
pub const VOICING_CRUNCH: f32 = 1.0;
pub const VOICING_HIGH_GAIN: f32 = 2.0;
pub const CABINET_FOUR_BY_TWELVE: f32 = 0.0;
pub const CABINET_DIRECT: f32 = 3.0;

static PARAMETERS: [ParameterSpecification; 10] = [
    ParameterSpecification::list(0, "Voicing", &["Clean", "Crunch", "High Gain"], 1),
    ParameterSpecification::linear(1, "Gain", "", 0.0, 10.0, 5.0),
    ParameterSpecification::logarithmic(2, "Tight", "Hz", 20.0, 400.0, 80.0),
    ParameterSpecification::linear(3, "Bass", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(4, "Middle", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(5, "Treble", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(6, "Presence", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(7, "Master", "", 0.0, 10.0, 4.0),
    ParameterSpecification::list(8, "Cabinet", &["4x12 Closed", "2x12 Open", "1x12 Combo", "Direct"], 0),
    ParameterSpecification::linear(9, "Output", "dB", -24.0, 12.0, 0.0),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Escalation Stack",
    class_identifier: [0xEB9BA239, 0x13B64AB9, 0xB10D2A5B, 0x3FCFE1D5],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Distortion",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: false,
    note_input: false,
    tail_seconds: 0.05,
};

// ── Voicings and cabinets ───────────────────────────────────────────────────

/// How a voicing stages its gain: the first stage's drive range across the Gain knob,
/// its bias (asymmetry = even harmonics), the optional second stage, and makeup.
struct VoicingProfile {
    first_stage_minimum_decibels: f64,
    first_stage_maximum_decibels: f64,
    first_stage_bias: f64,
    second_stage_decibels: Option<f64>,
    makeup_decibels: f64,
}

const VOICING_PROFILES: [VoicingProfile; 3] = [
    VoicingProfile {
        first_stage_minimum_decibels: -6.0,
        first_stage_maximum_decibels: 18.0,
        first_stage_bias: 0.05,
        second_stage_decibels: None,
        makeup_decibels: 4.0,
    },
    VoicingProfile {
        first_stage_minimum_decibels: 6.0,
        first_stage_maximum_decibels: 34.0,
        first_stage_bias: 0.25,
        second_stage_decibels: None,
        makeup_decibels: 0.0,
    },
    VoicingProfile {
        first_stage_minimum_decibels: 10.0,
        first_stage_maximum_decibels: 34.0,
        first_stage_bias: 0.3,
        second_stage_decibels: Some(20.0),
        makeup_decibels: 0.0,
    },
];

#[derive(Clone, Copy)]
enum CabinetStageShape {
    Highpass,
    Lowpass,
    Peaking,
}

/// One filter of a cabinet model: shape, frequency, quality and (peaking only) gain.
#[derive(Clone, Copy)]
struct CabinetStage(CabinetStageShape, f64, f64, f64);

pub const CABINET_STAGE_COUNT: usize = 6;

/// Speaker-and-box approximations: a resonant low-end bump where the cone and cabinet
/// meet, a presence peak, and the steep top-end roll-off that makes distortion sound
/// like a guitar instead of a fax machine.
const CABINET_MODELS: [[Option<CabinetStage>; CABINET_STAGE_COUNT]; 4] = {
    use CabinetStageShape::{Highpass, Lowpass, Peaking};
    [
        // 4x12 closed back: tight thump, scooped low mids, a bite at 2.4 kHz, dark above 5 kHz.
        [
            Some(CabinetStage(Highpass, 75.0, 1.1, 0.0)),
            Some(CabinetStage(Peaking, 120.0, 1.2, 2.5)),
            Some(CabinetStage(Peaking, 550.0, 1.0, -2.5)),
            Some(CabinetStage(Peaking, 2_400.0, 1.6, 3.0)),
            Some(CabinetStage(Lowpass, 4_800.0, 0.9, 0.0)),
            Some(CabinetStage(Lowpass, 5_600.0, 0.6, 0.0)),
        ],
        // 2x12 open back: less low thump, more air, a wider top.
        [
            Some(CabinetStage(Highpass, 90.0, 0.7, 0.0)),
            Some(CabinetStage(Peaking, 400.0, 1.0, -1.5)),
            Some(CabinetStage(Peaking, 2_800.0, 1.2, 2.0)),
            Some(CabinetStage(Lowpass, 5_500.0, 0.8, 0.0)),
            Some(CabinetStage(Lowpass, 7_000.0, 0.6, 0.0)),
            None,
        ],
        // 1x12 combo: boxy, mid-forward, early roll-off.
        [
            Some(CabinetStage(Highpass, 110.0, 0.8, 0.0)),
            Some(CabinetStage(Peaking, 1_800.0, 1.0, 3.0)),
            Some(CabinetStage(Lowpass, 4_500.0, 0.8, 0.0)),
            Some(CabinetStage(Lowpass, 6_000.0, 0.6, 0.0)),
            None,
            None,
        ],
        // Direct: only a DC blocker, for stakeholders who bring their own impulse response.
        [Some(CabinetStage(Highpass, 20.0, 0.707, 0.0)), None, None, None, None, None],
    ]
};

// ── The amp ─────────────────────────────────────────────────────────────────

/// Every coefficient the amp needs, recomputed only when a knob moves.
struct FilterBank {
    oversampling: [BiquadCoefficients; 4],
    tight: BiquadCoefficients,
    interstage_lowpass: BiquadCoefficients,
    interstage_highpass: BiquadCoefficients,
    bass: BiquadCoefficients,
    middle: BiquadCoefficients,
    treble: BiquadCoefficients,
    presence: BiquadCoefficients,
    cabinet: [BiquadCoefficients; CABINET_STAGE_COUNT],
}

impl FilterBank {
    fn design(parameters: &[f32], sample_rate: f64) -> Self {
        let oversampled_rate = sample_rate * synergy_substrate::dsp::OVERSAMPLING_FACTOR as f64;
        let cabinet_index = (parameters[CABINET].round() as usize).min(CABINET_MODELS.len() - 1);
        let mut cabinet = [BiquadCoefficients::IDENTITY; CABINET_STAGE_COUNT];
        for (stage_coefficients, stage) in cabinet.iter_mut().zip(CABINET_MODELS[cabinet_index].iter()) {
            if let Some(CabinetStage(shape, frequency_hz, quality, gain_decibels)) = stage {
                *stage_coefficients = match shape {
                    CabinetStageShape::Highpass => BiquadCoefficients::highpass(*frequency_hz, *quality, sample_rate),
                    CabinetStageShape::Lowpass => BiquadCoefficients::lowpass(*frequency_hz, *quality, sample_rate),
                    CabinetStageShape::Peaking => BiquadCoefficients::peaking(*frequency_hz, *quality, *gain_decibels, sample_rate),
                };
            }
        }
        Self {
            oversampling: oversampling_filter(sample_rate),
            tight: BiquadCoefficients::highpass(parameters[TIGHT] as f64, 0.707, oversampled_rate),
            interstage_lowpass: BiquadCoefficients::lowpass(6_500.0, 0.707, oversampled_rate),
            interstage_highpass: BiquadCoefficients::highpass(40.0, 0.707, oversampled_rate),
            bass: BiquadCoefficients::shelf(120.0, parameters[BASS] as f64, oversampled_rate, false),
            middle: BiquadCoefficients::peaking(750.0, 0.8, parameters[MIDDLE] as f64, oversampled_rate),
            treble: BiquadCoefficients::shelf(2_800.0, parameters[TREBLE] as f64, oversampled_rate, true),
            presence: BiquadCoefficients::shelf(4_500.0, parameters[PRESENCE] as f64, oversampled_rate, true),
            cabinet,
        }
    }
}

/// Gains for the current block; the drive and output gains glide per sample.
#[derive(Clone, Copy)]
struct StageSettings {
    first_stage_bias: f64,
    second_stage_gain: Option<f64>,
    power_drive: f64,
    power_normalizer: f64,
}

/// Asymmetric soft clipper: the bias shifts the curve so positive and negative halves
/// clip differently (even harmonics, like a single-ended triode), and subtracting
/// tanh(bias) keeps silence exactly silent.
#[inline]
fn asymmetric_stage(input_sample: f64, bias: f64) -> f64 {
    (input_sample + bias).tanh() - bias.tanh()
}

#[derive(Default)]
struct AmpChannel {
    oversampler: FourTimesOversampler,
    tight: BiquadState,
    interstage_lowpass: BiquadState,
    interstage_highpass: BiquadState,
    post_preamp_highpass: BiquadState,
    bass: BiquadState,
    middle: BiquadState,
    treble: BiquadState,
    presence: BiquadState,
    cabinet: [BiquadState; CABINET_STAGE_COUNT],
}

impl AmpChannel {
    /// One host-rate sample through the whole amp: up, preamp, tone, power amp, down, cabinet.
    fn process_sample(&mut self, input_sample: f64, first_stage_gain: f64, settings: &StageSettings, filters: &FilterBank) -> f64 {
        let AmpChannel {
            oversampler,
            tight,
            interstage_lowpass,
            interstage_highpass,
            post_preamp_highpass,
            bass,
            middle,
            treble,
            presence,
            cabinet,
        } = self;
        let decimated_sample = oversampler.process(&filters.oversampling, input_sample, |oversampled_sample| {
            let tightened_sample = tight.process(&filters.tight, oversampled_sample);
            let mut preamp_sample = asymmetric_stage(tightened_sample * first_stage_gain, settings.first_stage_bias);
            preamp_sample = interstage_lowpass.process(&filters.interstage_lowpass, preamp_sample);
            preamp_sample = interstage_highpass.process(&filters.interstage_highpass, preamp_sample);
            if let Some(second_stage_gain) = settings.second_stage_gain {
                preamp_sample = asymmetric_stage(preamp_sample * second_stage_gain, settings.first_stage_bias * 0.5);
                preamp_sample = post_preamp_highpass.process(&filters.interstage_highpass, preamp_sample);
            }

            let mut toned_sample = bass.process(&filters.bass, preamp_sample);
            toned_sample = middle.process(&filters.middle, toned_sample);
            toned_sample = treble.process(&filters.treble, toned_sample);
            toned_sample = presence.process(&filters.presence, toned_sample);
            // Power amp: symmetric, normalized so full scale stays full scale and quieter
            // passages come up as Master is pushed (the amp compresses).
            (toned_sample * settings.power_drive).tanh() * settings.power_normalizer
        });
        let mut cabinet_sample = decimated_sample;
        for (state, coefficients) in cabinet.iter_mut().zip(filters.cabinet.iter()) {
            cabinet_sample = state.process(coefficients, cabinet_sample);
        }
        cabinet_sample
    }
}

pub struct EscalationStack {
    sample_rate: f32,
    left_channel: AmpChannel,
    right_channel: AmpChannel,
    filters: FilterBank,
    designed_parameters: [f32; 10],
    smoothed_first_stage_gain: f64,
    smoothed_output_gain: f64,
}

impl EscalationStack {
    fn targets(parameters: &[f32]) -> (f64, f64, StageSettings) {
        let voicing = &VOICING_PROFILES[(parameters[VOICING].round() as usize).min(VOICING_PROFILES.len() - 1)];
        let gain_position = (parameters[GAIN] as f64 / 10.0).clamp(0.0, 1.0);
        let first_stage_decibels =
            voicing.first_stage_minimum_decibels + (voicing.first_stage_maximum_decibels - voicing.first_stage_minimum_decibels) * gain_position;
        let power_drive = 0.4 + 0.3 * parameters[MASTER].clamp(0.0, 10.0) as f64;
        // Fixed -12 dB so a pushed amp lands near a sensible track level before the Output knob.
        let output_decibels = parameters[OUTPUT] as f64 + voicing.makeup_decibels - 12.0;
        (
            decibels_to_gain(first_stage_decibels as f32) as f64,
            decibels_to_gain(output_decibels as f32) as f64,
            StageSettings {
                first_stage_bias: voicing.first_stage_bias,
                second_stage_gain: voicing.second_stage_decibels.map(|decibels| decibels_to_gain(decibels as f32) as f64),
                power_drive,
                power_normalizer: 1.0 / power_drive.tanh(),
            },
        )
    }
}

impl SynergyPlugin for EscalationStack {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        let default_parameters: [f32; 10] = PARAMETERS.map(|parameter| parameter.default_plain as f32);
        let (first_stage_gain, output_gain, _) = Self::targets(&default_parameters);
        Self {
            sample_rate,
            left_channel: AmpChannel::default(),
            right_channel: AmpChannel::default(),
            filters: FilterBank::design(&default_parameters, sample_rate as f64),
            designed_parameters: default_parameters,
            smoothed_first_stage_gain: first_stage_gain,
            smoothed_output_gain: output_gain,
        }
    }

    fn reset(&mut self) {
        self.left_channel = AmpChannel::default();
        self.right_channel = AmpChannel::default();
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        let parameters = block.parameters;
        if parameters != self.designed_parameters {
            self.filters = FilterBank::design(parameters, self.sample_rate as f64);
            self.designed_parameters.copy_from_slice(parameters);
        }
        let (target_first_stage_gain, target_output_gain, settings) = Self::targets(parameters);
        let glide_coefficient = smoothing_coefficient(20.0, self.sample_rate) as f64;

        for frame_index in 0..block.frames() {
            self.smoothed_first_stage_gain = target_first_stage_gain + glide_coefficient * (self.smoothed_first_stage_gain - target_first_stage_gain);
            self.smoothed_output_gain = target_output_gain + glide_coefficient * (self.smoothed_output_gain - target_output_gain);
            let left_output = self
                .left_channel
                .process_sample(block.input_left[frame_index] as f64, self.smoothed_first_stage_gain, &settings, &self.filters);
            let right_output =
                self.right_channel
                    .process_sample(block.input_right[frame_index] as f64, self.smoothed_first_stage_gain, &settings, &self.filters);
            block.output_left[frame_index] = flush_denormal((left_output * self.smoothed_output_gain) as f32);
            block.output_right[frame_index] = flush_denormal((right_output * self.smoothed_output_gain) as f32);
        }
    }
}

export_synergy_plugin!(EscalationStack);

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 48_000.0;

    fn default_parameters() -> Vec<f32> {
        PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect()
    }

    fn run(amp: &mut EscalationStack, parameters: &[f32], input_left: &[f32], input_right: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut output_left = vec![0.0; input_left.len()];
        let mut output_right = vec![0.0; input_left.len()];
        let mut meters = vec![None; PARAMETERS.len()];
        // Host-sized blocks, so parameter handling and state carry across block edges like in a DAW.
        for block_start in (0..input_left.len()).step_by(512) {
            let block_end = (block_start + 512).min(input_left.len());
            amp.process(ProcessBlock {
                sample_rate: SAMPLE_RATE,
                input_left: &input_left[block_start..block_end],
                input_right: &input_right[block_start..block_end],
                sidechain: None,
                output_left: &mut output_left[block_start..block_end],
                output_right: &mut output_right[block_start..block_end],
                parameters,
                meters: &mut meters,
                events: &[],
                transport: Default::default(),
            });
        }
        (output_left, output_right)
    }

    fn sine(frequency_hz: f32, amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|frame_index| amplitude * (2.0 * std::f32::consts::PI * frequency_hz * frame_index as f32 / SAMPLE_RATE).sin())
            .collect()
    }

    /// Goertzel magnitude of one frequency over a slice: the energy one bin of a DFT would report.
    fn tone_magnitude(signal: &[f32], frequency_hz: f32) -> f64 {
        let angular_step = 2.0 * std::f64::consts::PI * frequency_hz as f64 / SAMPLE_RATE as f64;
        let coefficient = 2.0 * angular_step.cos();
        let (mut previous_state, mut second_previous_state) = (0.0f64, 0.0f64);
        for sample in signal {
            let current_state = *sample as f64 + coefficient * previous_state - second_previous_state;
            second_previous_state = previous_state;
            previous_state = current_state;
        }
        (previous_state * previous_state + second_previous_state * second_previous_state - coefficient * previous_state * second_previous_state)
            .max(0.0)
            .sqrt()
    }

    fn rms(signal: &[f32]) -> f64 {
        (signal.iter().map(|sample| (*sample as f64).powi(2)).sum::<f64>() / signal.len().max(1) as f64).sqrt()
    }

    /// Harmonic energy (2nd to 8th) relative to the fundamental.
    fn harmonic_ratio(signal: &[f32], fundamental_hz: f32) -> f64 {
        let fundamental = tone_magnitude(signal, fundamental_hz);
        let harmonics: f64 = (2..=8).map(|harmonic| tone_magnitude(signal, fundamental_hz * harmonic as f32).powi(2)).sum();
        harmonics.sqrt() / fundamental.max(1e-12)
    }

    #[test]
    fn more_gain_escalates_the_harmonics() {
        let input = sine(220.0, 0.1, 24_000);
        let mut parameters = default_parameters();
        parameters[CABINET] = CABINET_DIRECT;
        parameters[VOICING] = VOICING_CRUNCH;
        parameters[GAIN] = 0.5;
        let (polite_output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &input, &input);
        parameters[GAIN] = 9.5;
        let (escalated_output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &input, &input);
        let polite_ratio = harmonic_ratio(&polite_output[12_000..], 220.0);
        let escalated_ratio = harmonic_ratio(&escalated_output[12_000..], 220.0);
        assert!(escalated_ratio > 3.0 * polite_ratio, "harmonics {polite_ratio:.3} → {escalated_ratio:.3}");
        assert!(
            escalated_ratio > 0.3,
            "a cranked crunch channel should be audibly distorted: {escalated_ratio:.3}"
        );
        // Asymmetry means even harmonics: the 2nd is present, not just the odd ones.
        assert!(tone_magnitude(&escalated_output[12_000..], 440.0) > 0.02 * tone_magnitude(&escalated_output[12_000..], 220.0));
    }

    #[test]
    fn the_cabinet_rolls_off_the_fizz_and_direct_does_not() {
        let mut parameters = default_parameters();
        parameters[VOICING] = VOICING_CLEAN;
        parameters[GAIN] = 0.0;
        parameters[MASTER] = 0.0;
        let level_at = |parameters: &[f32], frequency_hz: f32| {
            let input = sine(frequency_hz, 0.05, 24_000);
            let (output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), parameters, &input, &input);
            rms(&output[12_000..])
        };
        let relative_decibels = |parameters: &[f32], frequency_hz: f32| 20.0 * (level_at(parameters, frequency_hz) / level_at(parameters, 1_000.0)).log10();
        parameters[CABINET] = CABINET_DIRECT;
        let direct_at_three_kilohertz = relative_decibels(&parameters, 3_000.0);
        let direct_at_ten_kilohertz = relative_decibels(&parameters, 10_000.0);
        parameters[CABINET] = CABINET_FOUR_BY_TWELVE;
        let cabinet_at_ten_kilohertz = relative_decibels(&parameters, 10_000.0);
        assert!(
            direct_at_three_kilohertz.abs() < 3.0,
            "direct at 3 kHz vs 1 kHz: {direct_at_three_kilohertz:.1} dB"
        );
        assert!(
            cabinet_at_ten_kilohertz < direct_at_ten_kilohertz - 15.0,
            "10 kHz vs 1 kHz: 4x12 {cabinet_at_ten_kilohertz:.1} dB, direct {direct_at_ten_kilohertz:.1} dB"
        );
    }

    #[test]
    fn tight_keeps_the_low_end_out_of_the_clipper() {
        let input = sine(70.0, 0.3, 24_000);
        let mut parameters = default_parameters();
        parameters[CABINET] = CABINET_DIRECT;
        parameters[TIGHT] = 20.0;
        let (loose_output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &input, &input);
        parameters[TIGHT] = 400.0;
        let (tight_output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &input, &input);
        let loose_fundamental = tone_magnitude(&loose_output[12_000..], 70.0);
        let tight_fundamental = tone_magnitude(&tight_output[12_000..], 70.0);
        assert!(
            tight_fundamental < 0.5 * loose_fundamental,
            "70 Hz {loose_fundamental:.1} → {tight_fundamental:.1}"
        );
    }

    #[test]
    fn oversampling_keeps_aliases_below_the_harmonics() {
        // A cranked 4.1 kHz tone: its 9th harmonic (36.9 kHz) would fold back to an inharmonic
        // 11.1 kHz at 48 kHz without oversampling, at roughly the level of a real harmonic.
        let input = sine(4_100.0, 0.5, 24_000);
        let mut parameters = default_parameters();
        parameters[CABINET] = CABINET_DIRECT;
        parameters[VOICING] = VOICING_HIGH_GAIN;
        parameters[GAIN] = 10.0;
        let (output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &input, &input);
        let fundamental = tone_magnitude(&output[12_000..], 4_100.0);
        let folded_ninth = tone_magnitude(&output[12_000..], 48_000.0 - 9.0 * 4_100.0);
        let alias_decibels = 20.0 * (folded_ninth / fundamental).log10();
        assert!(alias_decibels < -40.0, "folded 9th harmonic at {alias_decibels:.1} dB");
    }

    #[test]
    fn hostile_settings_stay_finite_and_mono_stays_mono() {
        let mut parameters = default_parameters();
        parameters[VOICING] = VOICING_HIGH_GAIN;
        parameters[GAIN] = 10.0;
        parameters[MASTER] = 10.0;
        parameters[BASS] = 12.0;
        parameters[TREBLE] = 12.0;
        parameters[PRESENCE] = 12.0;
        parameters[OUTPUT] = 12.0;
        let square: Vec<f32> = (0..24_000).map(|frame_index| if (frame_index / 37) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let (output_left, output_right) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &square, &square);
        assert!(output_left.iter().all(|sample| sample.is_finite() && sample.abs() < 8.0));
        assert_eq!(output_left, output_right);
    }

    #[test]
    fn silence_stays_silent_and_tails_reach_zero_without_denormals() {
        let mut parameters = default_parameters();
        parameters[VOICING] = VOICING_HIGH_GAIN;
        parameters[GAIN] = 8.0;
        let silence = vec![0.0f32; 4_800];
        let (silent_output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &silence, &silence);
        assert!(silent_output.iter().all(|sample| *sample == 0.0));

        let mut burst_then_silence = sine(110.0, 0.5, 24_000);
        burst_then_silence.extend(std::iter::repeat_n(0.0f32, 96_000));
        let (output, _) = run(&mut EscalationStack::new(SAMPLE_RATE), &parameters, &burst_then_silence, &burst_then_silence);
        assert!(output.iter().all(|sample| sample.is_finite() && !sample.is_subnormal()));
        assert!(output[72_000..].iter().all(|sample| *sample == 0.0), "the tail never reached silence");
    }
}
