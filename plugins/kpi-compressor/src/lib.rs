//! # KPI Compressor
//!
//! Compresses your dynamic range to meet quarterly targets. A feed-forward,
//! stereo-linked peak compressor with a soft knee, program-independent attack
//! and release, makeup gain and a parallel mix — for stakeholders who want the
//! quiet parts louder and the loud parts in a meeting about it.

use synergy_substrate::dsp::{decibels_to_gain, gain_to_decibels, smoothing_coefficient};
use synergy_substrate::{export_synergy_plugin, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin};

pub const THRESHOLD: usize = 0;
pub const RATIO: usize = 1;
pub const KNEE: usize = 2;
pub const ATTACK: usize = 3;
pub const RELEASE: usize = 4;
pub const MAKEUP: usize = 5;
pub const MIX: usize = 6;
pub const GAIN_REDUCTION_METER: usize = 7;

static PARAMETERS: [ParameterSpecification; 8] = [
    ParameterSpecification::linear(0, "Threshold", "dB", -60.0, 0.0, -18.0),
    ParameterSpecification::logarithmic(1, "Ratio", ":1", 1.0, 20.0, 4.0),
    ParameterSpecification::linear(2, "Knee", "dB", 0.0, 24.0, 6.0),
    ParameterSpecification::logarithmic(3, "Attack", "ms", 0.1, 100.0, 10.0),
    ParameterSpecification::logarithmic(4, "Release", "ms", 10.0, 2000.0, 150.0),
    ParameterSpecification::linear(5, "Makeup", "dB", 0.0, 24.0, 0.0),
    ParameterSpecification::linear(6, "Mix", "%", 0.0, 100.0, 100.0),
    ParameterSpecification::linear(7, "Gain Reduction", "dB", 0.0, 48.0, 0.0).as_meter(),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "KPI Compressor",
    class_identifier: [0x4945A9AB, 0x5B00421C, 0xBCBCCA56, 0x91BE4085],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Dynamics",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: false,
    note_input: false,
    tail_seconds: 0.0,
};

/// Static gain computer: how many dB a signal at `level_decibels` is turned down.
pub fn gain_reduction_decibels(level_decibels: f32, threshold_decibels: f32, ratio: f32, knee_decibels: f32) -> f32 {
    let overshoot_decibels = level_decibels - threshold_decibels;
    let slope = 1.0 - 1.0 / ratio.max(1.0);
    if knee_decibels > 0.0 && overshoot_decibels.abs() <= knee_decibels * 0.5 {
        // Quadratic interpolation across the knee: no corner, no audible "click" into compression.
        let knee_position = overshoot_decibels + knee_decibels * 0.5;
        slope * knee_position * knee_position / (2.0 * knee_decibels)
    } else if overshoot_decibels > 0.0 {
        slope * overshoot_decibels
    } else {
        0.0
    }
}

pub struct KpiCompressor {
    sample_rate: f32,
    /// Smoothed gain reduction in dB (positive = turned down).
    smoothed_reduction_decibels: f32,
}

impl SynergyPlugin for KpiCompressor {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            smoothed_reduction_decibels: 0.0,
        }
    }

    fn reset(&mut self) {
        self.smoothed_reduction_decibels = 0.0;
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        let parameters = block.parameters;
        let threshold_decibels = parameters[THRESHOLD];
        let ratio = parameters[RATIO];
        let knee_decibels = parameters[KNEE];
        let attack_coefficient = smoothing_coefficient(parameters[ATTACK], self.sample_rate);
        let release_coefficient = smoothing_coefficient(parameters[RELEASE], self.sample_rate);
        let makeup_gain = decibels_to_gain(parameters[MAKEUP]);
        let wet_mix = (parameters[MIX] / 100.0).clamp(0.0, 1.0);
        let mut deepest_reduction_decibels = 0.0f32;

        for frame_index in 0..block.frames() {
            let left_input = block.input_left[frame_index];
            let right_input = block.input_right[frame_index];
            // Stereo-linked detection: both channels get the same gain, so the image never wanders.
            let detected_level_decibels = gain_to_decibels(left_input.abs().max(right_input.abs()));
            let target_reduction_decibels = gain_reduction_decibels(detected_level_decibels, threshold_decibels, ratio, knee_decibels);
            let coefficient = if target_reduction_decibels > self.smoothed_reduction_decibels {
                attack_coefficient
            } else {
                release_coefficient
            };
            self.smoothed_reduction_decibels = target_reduction_decibels + coefficient * (self.smoothed_reduction_decibels - target_reduction_decibels);
            if self.smoothed_reduction_decibels < 1.0e-6 {
                self.smoothed_reduction_decibels = 0.0;
            }
            deepest_reduction_decibels = deepest_reduction_decibels.max(self.smoothed_reduction_decibels);

            let compressed_gain = decibels_to_gain(-self.smoothed_reduction_decibels) * makeup_gain;
            let dry_mix = 1.0 - wet_mix;
            block.output_left[frame_index] = left_input * (dry_mix + wet_mix * compressed_gain);
            block.output_right[frame_index] = right_input * (dry_mix + wet_mix * compressed_gain);
        }
        block.meters[GAIN_REDUCTION_METER] = Some(deepest_reduction_decibels);
    }
}

export_synergy_plugin!(KpiCompressor);

#[cfg(test)]
mod tests {
    use super::*;

    fn default_parameters() -> Vec<f32> {
        PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect()
    }

    fn run(compressor: &mut KpiCompressor, parameters: &[f32], input_left: &[f32], input_right: &[f32]) -> (Vec<f32>, Vec<f32>, f32) {
        let mut output_left = vec![0.0; input_left.len()];
        let mut output_right = vec![0.0; input_left.len()];
        let mut meters = vec![None; PARAMETERS.len()];
        compressor.process(ProcessBlock {
            sample_rate: 48_000.0,
            input_left,
            input_right,
            sidechain: None,
            output_left: &mut output_left,
            output_right: &mut output_right,
            parameters,
            meters: &mut meters,
            events: &[],
            transport: Default::default(),
        });
        (output_left, output_right, meters[GAIN_REDUCTION_METER].unwrap_or(0.0))
    }

    #[test]
    fn gain_curve_matches_the_textbook() {
        // Below threshold and knee: untouched.
        assert_eq!(gain_reduction_decibels(-40.0, -18.0, 4.0, 6.0), 0.0);
        // Well above: (1 - 1/ratio) of the overshoot.
        assert!((gain_reduction_decibels(-6.0, -18.0, 4.0, 6.0) - 9.0).abs() < 1e-5);
        // In the knee: continuous at both edges and strictly between.
        let knee_bottom = gain_reduction_decibels(-21.0, -18.0, 4.0, 6.0);
        let knee_top = gain_reduction_decibels(-15.0, -18.0, 4.0, 6.0);
        assert!(knee_bottom.abs() < 1e-5);
        assert!((knee_top - 2.25).abs() < 1e-4);
        assert!(gain_reduction_decibels(-18.0, -18.0, 4.0, 6.0) > 0.0);
        // Ratio 1 is a polite no-op.
        assert_eq!(gain_reduction_decibels(0.0, -30.0, 1.0, 0.0), 0.0);
    }

    #[test]
    fn loud_signals_settle_at_the_expected_reduction_and_report_it() {
        let mut compressor = KpiCompressor::new(48_000.0);
        let mut parameters = default_parameters();
        parameters[KNEE] = 0.0;
        // A full-scale square wave: detected level 0 dBFS, 18 dB over threshold → 13.5 dB reduction at 4:1.
        let loud_input: Vec<f32> = (0..48_000).map(|frame_index| if (frame_index / 50) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let (output_left, output_right, metered_reduction) = run(&mut compressor, &parameters, &loud_input, &loud_input);
        let settled_peak = output_left[40_000..].iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!(
            (gain_to_decibels(settled_peak) + 13.5).abs() < 0.2,
            "settled at {} dB",
            gain_to_decibels(settled_peak)
        );
        assert!((metered_reduction - 13.5).abs() < 0.3);
        assert_eq!(output_left, output_right);
        assert!(output_left.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn mix_zero_is_a_bypass_and_silence_stays_silent() {
        let mut compressor = KpiCompressor::new(48_000.0);
        let mut parameters = default_parameters();
        parameters[MIX] = 0.0;
        let input: Vec<f32> = (0..4_800).map(|frame_index| (frame_index as f32 * 0.05).sin()).collect();
        let (output_left, _, _) = run(&mut compressor, &parameters, &input, &input);
        assert_eq!(output_left, input);

        let silence = vec![0.0f32; 4_800];
        let (silent_output, _, metered_reduction) = run(&mut KpiCompressor::new(48_000.0), &default_parameters(), &silence, &silence);
        assert!(silent_output.iter().all(|sample| *sample == 0.0));
        assert_eq!(metered_reduction, 0.0);
    }
}
