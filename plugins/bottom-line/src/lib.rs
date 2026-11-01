//! # Bottom Line
//!
//! Keeps the low end on budget while the rest of the bass gets escalated. A bass amp built
//! the way modern bass rigs are: a crossover splits the signal, everything below it stays
//! clean (the fundamental is the bottom line, and nobody distorts the bottom line), and the
//! mids and highs go through an asymmetric drive stage at 4x oversampling. Blend sets how much
//! of the driven band replaces the clean one; then a four-band EQ and an 8x10, 4x10 or 1x15
//! cabinet. The crossover is Linkwitz–Riley, so with Blend at 0 the two bands sum back to a
//! flat response.

use synergy_substrate::dsp::{
    decibels_to_gain, flush_denormal, oversampling_filter, smoothing_coefficient, BiquadCoefficients, BiquadState, FourTimesOversampler, OVERSAMPLING_FACTOR,
};
use synergy_substrate::{export_synergy_plugin, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin};

pub const DRIVE: usize = 0;
pub const BLEND: usize = 1;
pub const CROSSOVER: usize = 2;
pub const BASS: usize = 3;
pub const LOW_MID: usize = 4;
pub const HIGH_MID: usize = 5;
pub const TREBLE: usize = 6;
pub const CABINET: usize = 7;
pub const OUTPUT: usize = 8;

pub const CABINET_EIGHT_BY_TEN: f32 = 0.0;
pub const CABINET_DIRECT: f32 = 3.0;

static PARAMETERS: [ParameterSpecification; 9] = [
    ParameterSpecification::linear(0, "Drive", "", 0.0, 10.0, 4.0),
    ParameterSpecification::linear(1, "Blend", "%", 0.0, 100.0, 60.0),
    ParameterSpecification::logarithmic(2, "Crossover", "Hz", 60.0, 400.0, 150.0),
    ParameterSpecification::linear(3, "Bass", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(4, "Low Mid", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(5, "High Mid", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::linear(6, "Treble", "dB", -12.0, 12.0, 0.0),
    ParameterSpecification::list(7, "Cabinet", &["8x10", "4x10", "1x15", "Direct"], 1),
    ParameterSpecification::linear(8, "Output", "dB", -24.0, 12.0, 0.0),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Bottom Line",
    class_identifier: [0x079E9B20, 0x0DE24D18, 0xBF355217, 0xFC4B90A4],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Distortion",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: false,
    note_input: false,
    tail_seconds: 0.05,
};

const CABINET_STAGE_COUNT: usize = 6;

#[derive(Clone, Copy)]
enum CabinetStageShape {
    Highpass,
    Lowpass,
    Peaking,
}

/// One filter of a cabinet model: shape, frequency, quality and (peaking only) gain.
#[derive(Clone, Copy)]
struct CabinetStage(CabinetStageShape, f64, f64, f64);

/// Bass cabinets: where the box stops reproducing lows, the low-mid growl, and the early
/// top-end roll-off (bass speakers give up long before guitar ones; tweeters are not in scope).
const CABINET_MODELS: [[Option<CabinetStage>; CABINET_STAGE_COUNT]; 4] = {
    use CabinetStageShape::{Highpass, Lowpass, Peaking};
    [
        // 8x10: deep and wide, scooped low mids, a growl at 2 kHz.
        [
            Some(CabinetStage(Highpass, 45.0, 0.9, 0.0)),
            Some(CabinetStage(Peaking, 90.0, 1.0, 2.0)),
            Some(CabinetStage(Peaking, 400.0, 1.0, -2.0)),
            Some(CabinetStage(Peaking, 2_000.0, 1.2, 2.0)),
            Some(CabinetStage(Lowpass, 4_500.0, 0.8, 0.0)),
            Some(CabinetStage(Lowpass, 5_500.0, 0.6, 0.0)),
        ],
        // 4x10: punchy, mid-forward.
        [
            Some(CabinetStage(Highpass, 50.0, 0.8, 0.0)),
            Some(CabinetStage(Peaking, 110.0, 1.0, 2.0)),
            Some(CabinetStage(Peaking, 1_600.0, 1.0, 2.5)),
            Some(CabinetStage(Lowpass, 5_000.0, 0.8, 0.0)),
            Some(CabinetStage(Lowpass, 6_500.0, 0.6, 0.0)),
            None,
        ],
        // 1x15: round and old-school, dark early.
        [
            Some(CabinetStage(Highpass, 40.0, 1.0, 0.0)),
            Some(CabinetStage(Peaking, 70.0, 1.0, 3.0)),
            Some(CabinetStage(Lowpass, 2_800.0, 0.8, 0.0)),
            Some(CabinetStage(Lowpass, 3_600.0, 0.6, 0.0)),
            None,
            None,
        ],
        // Direct: only a DC blocker, for the DI channel.
        [Some(CabinetStage(Highpass, 20.0, 0.707, 0.0)), None, None, None, None, None],
    ]
};

/// Every coefficient the amp needs, recomputed only when a knob moves.
struct FilterBank {
    crossover_lowpass: BiquadCoefficients,
    crossover_highpass: BiquadCoefficients,
    oversampling: [BiquadCoefficients; 4],
    driven_highpass: BiquadCoefficients,
    driven_lowpass: BiquadCoefficients,
    bass: BiquadCoefficients,
    low_mid: BiquadCoefficients,
    high_mid: BiquadCoefficients,
    treble: BiquadCoefficients,
    cabinet: [BiquadCoefficients; CABINET_STAGE_COUNT],
}

impl FilterBank {
    fn design(parameters: &[f32], sample_rate: f64) -> Self {
        let oversampled_rate = sample_rate * OVERSAMPLING_FACTOR as f64;
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
            // Each applied twice: a 4th-order Linkwitz–Riley pair, whose bands sum flat.
            crossover_lowpass: BiquadCoefficients::lowpass(parameters[CROSSOVER] as f64, std::f64::consts::FRAC_1_SQRT_2, sample_rate),
            crossover_highpass: BiquadCoefficients::highpass(parameters[CROSSOVER] as f64, std::f64::consts::FRAC_1_SQRT_2, sample_rate),
            oversampling: oversampling_filter(sample_rate),
            driven_highpass: BiquadCoefficients::highpass(30.0, 0.707, oversampled_rate),
            driven_lowpass: BiquadCoefficients::lowpass(5_000.0, 0.707, oversampled_rate),
            bass: BiquadCoefficients::shelf(80.0, parameters[BASS] as f64, sample_rate, false),
            low_mid: BiquadCoefficients::peaking(300.0, 0.9, parameters[LOW_MID] as f64, sample_rate),
            high_mid: BiquadCoefficients::peaking(1_200.0, 0.9, parameters[HIGH_MID] as f64, sample_rate),
            treble: BiquadCoefficients::shelf(4_000.0, parameters[TREBLE] as f64, sample_rate, true),
            cabinet,
        }
    }
}

/// Asymmetric soft clipper; subtracting tanh(bias) keeps silence exactly silent.
#[inline]
fn asymmetric_stage(input_sample: f64, bias: f64) -> f64 {
    (input_sample + bias).tanh() - bias.tanh()
}

const DRIVE_BIAS: f64 = 0.2;

#[derive(Default)]
struct BassChannel {
    first_lowpass: BiquadState,
    second_lowpass: BiquadState,
    first_highpass: BiquadState,
    second_highpass: BiquadState,
    oversampler: FourTimesOversampler,
    driven_highpass: BiquadState,
    driven_lowpass: BiquadState,
    bass: BiquadState,
    low_mid: BiquadState,
    high_mid: BiquadState,
    treble: BiquadState,
    cabinet: [BiquadState; CABINET_STAGE_COUNT],
}

impl BassChannel {
    fn process_sample(&mut self, input_sample: f64, drive_gain: f64, blend: f64, filters: &FilterBank) -> f64 {
        let BassChannel {
            first_lowpass,
            second_lowpass,
            first_highpass,
            second_highpass,
            oversampler,
            driven_highpass,
            driven_lowpass,
            bass,
            low_mid,
            high_mid,
            treble,
            cabinet,
        } = self;
        let low_band = second_lowpass.process(&filters.crossover_lowpass, first_lowpass.process(&filters.crossover_lowpass, input_sample));
        let high_band = second_highpass.process(&filters.crossover_highpass, first_highpass.process(&filters.crossover_highpass, input_sample));
        let mut combined = low_band + (1.0 - blend) * high_band;
        if blend > 0.0 {
            // Geometric makeup: quiet passages come out a little louder, cranked ones a little quieter, as amps do.
            let makeup = 1.0 / drive_gain.sqrt();
            let driven_band = oversampler.process(&filters.oversampling, high_band, |oversampled_sample| {
                let clipped_sample = asymmetric_stage(oversampled_sample * drive_gain, DRIVE_BIAS);
                driven_lowpass.process(&filters.driven_lowpass, driven_highpass.process(&filters.driven_highpass, clipped_sample))
            });
            combined += blend * driven_band * makeup;
        }
        let mut shaped_sample = bass.process(&filters.bass, combined);
        shaped_sample = low_mid.process(&filters.low_mid, shaped_sample);
        shaped_sample = high_mid.process(&filters.high_mid, shaped_sample);
        shaped_sample = treble.process(&filters.treble, shaped_sample);
        for (state, coefficients) in cabinet.iter_mut().zip(filters.cabinet.iter()) {
            shaped_sample = state.process(coefficients, shaped_sample);
        }
        shaped_sample
    }
}

pub struct BottomLine {
    sample_rate: f32,
    left_channel: BassChannel,
    right_channel: BassChannel,
    filters: FilterBank,
    designed_parameters: [f32; 9],
    smoothed_drive_gain: f64,
    smoothed_output_gain: f64,
}

/// Drive 0..10 maps to 6..38 dB into the clipper.
fn drive_gain_for(drive: f32) -> f64 {
    decibels_to_gain(6.0 + 3.2 * drive.clamp(0.0, 10.0)) as f64
}

impl SynergyPlugin for BottomLine {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        let default_parameters: [f32; 9] = PARAMETERS.map(|parameter| parameter.default_plain as f32);
        Self {
            sample_rate,
            left_channel: BassChannel::default(),
            right_channel: BassChannel::default(),
            filters: FilterBank::design(&default_parameters, sample_rate as f64),
            designed_parameters: default_parameters,
            smoothed_drive_gain: drive_gain_for(default_parameters[DRIVE]),
            smoothed_output_gain: decibels_to_gain(default_parameters[OUTPUT]) as f64,
        }
    }

    fn reset(&mut self) {
        self.left_channel = BassChannel::default();
        self.right_channel = BassChannel::default();
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        let parameters = block.parameters;
        if parameters != self.designed_parameters {
            self.filters = FilterBank::design(parameters, self.sample_rate as f64);
            self.designed_parameters.copy_from_slice(parameters);
        }
        let target_drive_gain = drive_gain_for(parameters[DRIVE]);
        let target_output_gain = decibels_to_gain(parameters[OUTPUT]) as f64;
        let blend = (parameters[BLEND] as f64 / 100.0).clamp(0.0, 1.0);
        let glide_coefficient = smoothing_coefficient(20.0, self.sample_rate) as f64;
        for frame_index in 0..block.frames() {
            self.smoothed_drive_gain = target_drive_gain + glide_coefficient * (self.smoothed_drive_gain - target_drive_gain);
            self.smoothed_output_gain = target_output_gain + glide_coefficient * (self.smoothed_output_gain - target_output_gain);
            let left_output = self
                .left_channel
                .process_sample(block.input_left[frame_index] as f64, self.smoothed_drive_gain, blend, &self.filters);
            let right_output = self
                .right_channel
                .process_sample(block.input_right[frame_index] as f64, self.smoothed_drive_gain, blend, &self.filters);
            block.output_left[frame_index] = flush_denormal((left_output * self.smoothed_output_gain) as f32);
            block.output_right[frame_index] = flush_denormal((right_output * self.smoothed_output_gain) as f32);
        }
    }
}

export_synergy_plugin!(BottomLine);

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 48_000.0;

    fn default_parameters() -> Vec<f32> {
        PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect()
    }

    fn run(parameters: &[f32], input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut amp = BottomLine::new(SAMPLE_RATE);
        let mut output_left = vec![0.0; input.len()];
        let mut output_right = vec![0.0; input.len()];
        let mut meters = vec![None; PARAMETERS.len()];
        for block_start in (0..input.len()).step_by(512) {
            let block_end = (block_start + 512).min(input.len());
            amp.process(ProcessBlock {
                sample_rate: SAMPLE_RATE,
                input_left: &input[block_start..block_end],
                input_right: &input[block_start..block_end],
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

    /// Goertzel magnitude of one frequency over a slice.
    fn tone_magnitude(signal: &[f32], frequency_hz: f32) -> f64 {
        let coefficient = 2.0 * (2.0 * std::f64::consts::PI * frequency_hz as f64 / SAMPLE_RATE as f64).cos();
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

    fn harmonic_ratio(signal: &[f32], fundamental_hz: f32) -> f64 {
        let fundamental = tone_magnitude(signal, fundamental_hz);
        let harmonics: f64 = (2..=6).map(|harmonic| tone_magnitude(signal, fundamental_hz * harmonic as f32).powi(2)).sum();
        harmonics.sqrt() / fundamental.max(1e-12)
    }

    fn rms(signal: &[f32]) -> f64 {
        (signal.iter().map(|sample| (*sample as f64).powi(2)).sum::<f64>() / signal.len().max(1) as f64).sqrt()
    }

    #[test]
    fn blend_zero_through_a_direct_cabinet_is_flat() {
        let mut parameters = default_parameters();
        parameters[BLEND] = 0.0;
        parameters[CABINET] = CABINET_DIRECT;
        for frequency_hz in [60.0, 150.0, 400.0, 1_300.0, 5_000.0] {
            let input = sine(frequency_hz, 0.3, 24_000);
            let (output, _) = run(&parameters, &input);
            let change_decibels = 20.0 * (rms(&output[12_000..]) / rms(&input[12_000..])).log10();
            assert!(change_decibels.abs() < 0.3, "{frequency_hz} Hz changed by {change_decibels:.2} dB");
        }
    }

    #[test]
    fn the_bottom_stays_clean_while_the_mids_escalate() {
        let mut parameters = default_parameters();
        parameters[DRIVE] = 10.0;
        parameters[BLEND] = 100.0;
        parameters[CABINET] = CABINET_DIRECT;
        let (low_output, _) = run(&parameters, &sine(45.0, 0.4, 48_000));
        let (mid_output, _) = run(&parameters, &sine(440.0, 0.4, 48_000));
        let low_ratio = harmonic_ratio(&low_output[24_000..], 45.0);
        let mid_ratio = harmonic_ratio(&mid_output[24_000..], 440.0);
        assert!(mid_ratio > 0.3, "a cranked mid should distort: {mid_ratio:.3}");
        assert!(low_ratio < 0.3 * mid_ratio, "45 Hz {low_ratio:.3} vs 440 Hz {mid_ratio:.3}");
    }

    #[test]
    fn the_cabinet_rolls_off_the_top() {
        let mut parameters = default_parameters();
        parameters[BLEND] = 0.0;
        let level = |parameters: &[f32], frequency_hz: f32| rms(&run(parameters, &sine(frequency_hz, 0.1, 24_000)).0[12_000..]);
        parameters[CABINET] = CABINET_EIGHT_BY_TEN;
        let cabinet_roll_off = 20.0 * (level(&parameters, 8_000.0) / level(&parameters, 1_000.0)).log10();
        parameters[CABINET] = CABINET_DIRECT;
        let direct_roll_off = 20.0 * (level(&parameters, 8_000.0) / level(&parameters, 1_000.0)).log10();
        assert!(
            cabinet_roll_off < direct_roll_off - 15.0,
            "8x10 {cabinet_roll_off:.1} dB, direct {direct_roll_off:.1} dB"
        );
    }

    #[test]
    fn hostile_settings_stay_finite_silence_stays_silent_and_tails_end() {
        let mut parameters = default_parameters();
        parameters[DRIVE] = 10.0;
        parameters[BLEND] = 100.0;
        parameters[BASS] = 12.0;
        parameters[TREBLE] = 12.0;
        parameters[OUTPUT] = 12.0;
        let square: Vec<f32> = (0..24_000).map(|frame_index| if (frame_index / 53) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let (output_left, output_right) = run(&parameters, &square);
        assert!(output_left.iter().all(|sample| sample.is_finite() && sample.abs() < 16.0));
        assert_eq!(output_left, output_right);

        let (silent_output, _) = run(&parameters, &[0.0; 4_800]);
        assert!(silent_output.iter().all(|sample| *sample == 0.0));

        let mut burst_then_silence = sine(55.0, 0.5, 24_000);
        burst_then_silence.extend(std::iter::repeat_n(0.0f32, 96_000));
        let (tail, _) = run(&parameters, &burst_then_silence);
        assert!(tail.iter().all(|sample| sample.is_finite() && !sample.is_subnormal()));
        assert!(tail[72_000..].iter().all(|sample| *sample == 0.0), "the tail never reached silence");
    }
}
