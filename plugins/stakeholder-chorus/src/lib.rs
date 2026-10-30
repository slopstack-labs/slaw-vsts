//! # Stakeholder Chorus
//!
//! More voices in the room, none of them listening. A multi-voice chorus: up
//! to four modulated delay taps ("stakeholders"), each heard at a different
//! phase on the left and the right, so a mono source leaves the meeting wider
//! than it came in.

use synergy_substrate::dsp::flush_denormal;
use synergy_substrate::{export_synergy_plugin, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin};

pub const RATE: usize = 0;
pub const DEPTH: usize = 1;
pub const STAKEHOLDERS: usize = 2;
pub const WIDTH: usize = 3;
pub const FEEDBACK: usize = 4;
pub const MIX: usize = 5;

static PARAMETERS: [ParameterSpecification; 6] = [
    ParameterSpecification::logarithmic(0, "Rate", "Hz", 0.05, 8.0, 0.7),
    ParameterSpecification::linear(1, "Depth", "ms", 0.0, 12.0, 4.0),
    ParameterSpecification::stepped(2, "Stakeholders", "voices", 1.0, 4.0, 3.0),
    ParameterSpecification::linear(3, "Width", "%", 0.0, 100.0, 80.0),
    ParameterSpecification::linear(4, "Feedback", "%", 0.0, 70.0, 10.0),
    ParameterSpecification::linear(5, "Mix", "%", 0.0, 100.0, 50.0),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Stakeholder Chorus",
    class_identifier: [0x2F2C94F5, 0x3C394F4D, 0xB06005EF, 0xD0B554E1],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Modulation",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: false,
    note_input: false,
    tail_seconds: 0.1,
};

const MAXIMUM_STAKEHOLDERS: usize = 4;
/// Every stakeholder's tap sits at least this far back, plus a stagger per voice.
const BASE_DELAY_MILLISECONDS: f32 = 12.0;
const STAGGER_MILLISECONDS: f32 = 3.5;
const DELAY_LINE_MILLISECONDS: f32 = 50.0;

pub struct StakeholderChorus {
    sample_rate: f32,
    left_delay_line: Vec<f32>,
    right_delay_line: Vec<f32>,
    write_position: usize,
    /// LFO phase in cycles, shared by all voices (each adds its own offset).
    modulation_phase: f32,
}

impl StakeholderChorus {
    /// Reads a delay line `delay_frames` behind the write head with linear interpolation.
    fn read_delayed(delay_line: &[f32], write_position: usize, delay_frames: f32) -> f32 {
        let line_length = delay_line.len();
        let read_position = write_position as f32 - delay_frames + line_length as f32;
        let lower_index = read_position.floor() as usize % line_length;
        let upper_index = (lower_index + 1) % line_length;
        let fraction = read_position.fract();
        delay_line[lower_index] + (delay_line[upper_index] - delay_line[lower_index]) * fraction
    }
}

impl SynergyPlugin for StakeholderChorus {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        let line_length = (DELAY_LINE_MILLISECONDS * 0.001 * sample_rate).ceil() as usize + 4;
        Self {
            sample_rate,
            left_delay_line: vec![0.0; line_length],
            right_delay_line: vec![0.0; line_length],
            write_position: 0,
            modulation_phase: 0.0,
        }
    }

    fn reset(&mut self) {
        self.left_delay_line.fill(0.0);
        self.right_delay_line.fill(0.0);
        self.write_position = 0;
        self.modulation_phase = 0.0;
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        let parameters = block.parameters;
        let phase_increment = parameters[RATE] / self.sample_rate;
        let depth_frames = parameters[DEPTH] * 0.001 * self.sample_rate;
        let stakeholder_count = (parameters[STAKEHOLDERS].round() as usize).clamp(1, MAXIMUM_STAKEHOLDERS);
        let width = (parameters[WIDTH] / 100.0).clamp(0.0, 1.0);
        let feedback = (parameters[FEEDBACK] / 100.0).clamp(0.0, 0.7);
        let wet_mix = (parameters[MIX] / 100.0).clamp(0.0, 1.0);
        let voice_gain = 1.0 / (stakeholder_count as f32).sqrt();
        let line_length = self.left_delay_line.len();

        for frame_index in 0..block.frames() {
            let left_input = block.input_left[frame_index];
            let right_input = block.input_right[frame_index];
            let mut wet_left = 0.0f32;
            let mut wet_right = 0.0f32;
            let mut feedback_left = 0.0f32;
            let mut feedback_right = 0.0f32;
            for voice_index in 0..stakeholder_count {
                let voice_offset = voice_index as f32 / stakeholder_count as f32;
                let base_delay_frames = (BASE_DELAY_MILLISECONDS + STAGGER_MILLISECONDS * voice_index as f32) * 0.001 * self.sample_rate;
                // The right ear hears each stakeholder up to a quarter cycle later: that is the width.
                let left_phase = self.modulation_phase + voice_offset;
                let right_phase = left_phase + 0.25 * width;
                let left_delay = base_delay_frames + depth_frames * 0.5 * (1.0 + (std::f32::consts::TAU * left_phase).sin());
                let right_delay = base_delay_frames + depth_frames * 0.5 * (1.0 + (std::f32::consts::TAU * right_phase).sin());
                let left_tap = Self::read_delayed(&self.left_delay_line, self.write_position, left_delay);
                let right_tap = Self::read_delayed(&self.right_delay_line, self.write_position, right_delay);
                feedback_left += left_tap;
                feedback_right += right_tap;
                // Alternate stakeholders lean left and right; width decides how far.
                let lean = if voice_index % 2 == 0 { 0.5 } else { -0.5 } * width;
                wet_left += (left_tap * (1.0 + lean) + right_tap * (1.0 - lean) * 0.25 * width) * voice_gain;
                wet_right += (right_tap * (1.0 - lean) + left_tap * (1.0 + lean) * 0.25 * width) * voice_gain;
            }
            let normalization = 1.0 / (1.0 + 0.25 * width);
            wet_left *= normalization;
            wet_right *= normalization;

            // Feedback uses the plain average of the taps (gain ≤ 1), so the loop stays below the knob's 0.7.
            let feedback_scale = feedback / stakeholder_count as f32;
            self.left_delay_line[self.write_position] = flush_denormal(left_input + feedback_left * feedback_scale);
            self.right_delay_line[self.write_position] = flush_denormal(right_input + feedback_right * feedback_scale);
            self.write_position = (self.write_position + 1) % line_length;
            self.modulation_phase = (self.modulation_phase + phase_increment).fract();

            block.output_left[frame_index] = left_input * (1.0 - wet_mix) + wet_left * wet_mix;
            block.output_right[frame_index] = right_input * (1.0 - wet_mix) + wet_right * wet_mix;
        }
    }
}

export_synergy_plugin!(StakeholderChorus);

#[cfg(test)]
mod tests {
    use super::*;

    fn default_parameters() -> Vec<f32> {
        PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect()
    }

    fn run(parameters: &[f32], input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut chorus = StakeholderChorus::new(48_000.0);
        let mut output_left = vec![0.0; input.len()];
        let mut output_right = vec![0.0; input.len()];
        let mut meters = vec![None; PARAMETERS.len()];
        chorus.process(ProcessBlock {
            sample_rate: 48_000.0,
            input_left: input,
            input_right: input,
            sidechain: None,
            output_left: &mut output_left,
            output_right: &mut output_right,
            parameters,
            meters: &mut meters,
            events: &[],
            transport: Default::default(),
        });
        (output_left, output_right)
    }

    fn correlation(left: &[f32], right: &[f32]) -> f32 {
        let (mut product, mut left_energy, mut right_energy) = (0.0f64, 0.0f64, 0.0f64);
        for (left_sample, right_sample) in left.iter().zip(right) {
            product += f64::from(*left_sample) * f64::from(*right_sample);
            left_energy += f64::from(*left_sample).powi(2);
            right_energy += f64::from(*right_sample).powi(2);
        }
        (product / (left_energy * right_energy).sqrt().max(1e-12)) as f32
    }

    #[test]
    fn a_mono_source_leaves_wider_than_it_came_in() {
        let input: Vec<f32> = (0..96_000)
            .map(|frame_index| (frame_index as f32 * std::f32::consts::TAU * 330.0 / 48_000.0).sin() * 0.5)
            .collect();
        let mut parameters = default_parameters();
        parameters[MIX] = 100.0;
        let (output_left, output_right) = run(&parameters, &input);
        let stereo_correlation = correlation(&output_left[4_800..], &output_right[4_800..]);
        assert!(stereo_correlation < 0.95, "correlation {stereo_correlation}");
        assert!(output_left.iter().chain(&output_right).all(|sample| sample.is_finite()));

        parameters[WIDTH] = 0.0;
        let (narrow_left, narrow_right) = run(&parameters, &input);
        assert!(correlation(&narrow_left[4_800..], &narrow_right[4_800..]) > 0.999, "width 0 should stay mono");
    }

    #[test]
    fn mix_zero_passes_the_input_and_silence_decays_to_zero() {
        let input: Vec<f32> = (0..4_800).map(|frame_index| (frame_index as f32 * 0.03).sin()).collect();
        let mut parameters = default_parameters();
        parameters[MIX] = 0.0;
        assert_eq!(run(&parameters, &input).0, input);

        let mut burst_then_silence = vec![0.0f32; 48_000];
        burst_then_silence[..480].iter_mut().for_each(|sample| *sample = 0.8);
        let mut parameters = default_parameters();
        parameters[FEEDBACK] = 70.0;
        let (tail_left, _) = run(&parameters, &burst_then_silence);
        assert!(tail_left[47_000..].iter().all(|sample| sample.abs() < 1e-3));
    }
}
