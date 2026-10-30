//! # Headcount
//!
//! Scales headcount up to 8 voices and lays them off on note-off. A polyphonic
//! subtractive synth: two band-limited oscillators (saw or square, the second
//! detunable and octave-shiftable), a resonant zero-delay-feedback lowpass
//! with an envelope sweep, an ADSR, velocity, and oldest-first voice stealing
//! when the hiring freeze kicks in.

use synergy_substrate::dsp::{decibels_to_gain, flush_denormal, poly_blep, ZeroDelayStateVariableFilter};
use synergy_substrate::{export_synergy_plugin, NoteEvent, NoteEventKind, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin};

pub const OSCILLATOR_ONE_WAVE: usize = 0;
pub const OSCILLATOR_TWO_WAVE: usize = 1;
pub const OSCILLATOR_TWO_DETUNE: usize = 2;
pub const OSCILLATOR_TWO_OCTAVE: usize = 3;
pub const OSCILLATOR_MIX: usize = 4;
pub const CUTOFF: usize = 5;
pub const RESONANCE: usize = 6;
pub const FILTER_ENVELOPE: usize = 7;
pub const ATTACK: usize = 8;
pub const DECAY: usize = 9;
pub const SUSTAIN: usize = 10;
pub const RELEASE: usize = 11;
pub const HEADCOUNT: usize = 12;
pub const VOLUME: usize = 13;
pub const VOICES_EMPLOYED_METER: usize = 14;

const WAVEFORMS: &[&str] = &["Saw", "Square"];

static PARAMETERS: [ParameterSpecification; 15] = [
    ParameterSpecification::list(0, "Osc 1 Wave", WAVEFORMS, 0),
    ParameterSpecification::list(1, "Osc 2 Wave", WAVEFORMS, 0),
    ParameterSpecification::linear(2, "Osc 2 Detune", "ct", -50.0, 50.0, 9.0),
    ParameterSpecification::stepped(3, "Osc 2 Octave", "oct", -2.0, 2.0, 0.0),
    ParameterSpecification::linear(4, "Osc Mix", "%", 0.0, 100.0, 50.0),
    ParameterSpecification::logarithmic(5, "Cutoff", "Hz", 40.0, 18_000.0, 2_400.0),
    ParameterSpecification::linear(6, "Resonance", "%", 0.0, 100.0, 25.0),
    ParameterSpecification::linear(7, "Filter Envelope", "%", 0.0, 100.0, 35.0),
    ParameterSpecification::logarithmic(8, "Attack", "ms", 1.0, 3_000.0, 5.0),
    ParameterSpecification::logarithmic(9, "Decay", "ms", 5.0, 5_000.0, 300.0),
    ParameterSpecification::linear(10, "Sustain", "%", 0.0, 100.0, 70.0),
    ParameterSpecification::logarithmic(11, "Release", "ms", 5.0, 5_000.0, 250.0),
    ParameterSpecification::stepped(12, "Headcount", "voices", 1.0, 8.0, 8.0),
    ParameterSpecification::linear(13, "Volume", "dB", -36.0, 6.0, -6.0),
    ParameterSpecification::linear(14, "Voices Employed", "voices", 0.0, 8.0, 0.0).as_meter(),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Headcount",
    class_identifier: [0xA630AA13, 0x02304F22, 0x880227B4, 0x89E55BAB],
    kind: PluginKind::Instrument,
    sub_categories: "Instrument|Synth",
    version: env!("CARGO_PKG_VERSION"),
    parameters: &PARAMETERS,
    sidechain_input: false,
    note_input: true,
    tail_seconds: 0.0,
};

const MAXIMUM_HEADCOUNT: usize = 8;
/// The filter envelope opens the cutoff by up to this many octaves.
const FILTER_ENVELOPE_OCTAVES: f32 = 4.0;
/// A voice whose release has fallen below this is laid off.
const LAYOFF_LEVEL: f32 = 1.0e-4;

#[derive(Clone, Copy, Debug, PartialEq)]
enum EnvelopeStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    stage: EnvelopeStage,
    pitch: u8,
    velocity: f32,
    envelope_level: f32,
    oscillator_one_phase: f32,
    oscillator_two_phase: f32,
    filter: ZeroDelayStateVariableFilter,
    /// Hiring order, for oldest-first stealing.
    seniority: u64,
}

impl Voice {
    const VACANT: Voice = Voice {
        stage: EnvelopeStage::Idle,
        pitch: 0,
        velocity: 0.0,
        envelope_level: 0.0,
        oscillator_one_phase: 0.0,
        oscillator_two_phase: 0.0,
        filter: ZeroDelayStateVariableFilter::new(),
        seniority: 0,
    };

    fn is_employed(&self) -> bool {
        self.stage != EnvelopeStage::Idle
    }
}

/// Note number to frequency, A4 (69) = 440 Hz.
pub fn note_frequency(pitch: f32) -> f32 {
    440.0 * 2.0f32.powf((pitch - 69.0) / 12.0)
}

#[inline]
fn oscillator_sample(waveform_index: f32, phase: f32, phase_increment: f32) -> f32 {
    if waveform_index < 0.5 {
        (2.0 * phase - 1.0) - poly_blep(phase, phase_increment)
    } else {
        let naive_square = if phase < 0.5 { 1.0 } else { -1.0 };
        naive_square + poly_blep(phase, phase_increment) - poly_blep((phase + 0.5).fract(), phase_increment)
    }
}

pub struct Headcount {
    sample_rate: f32,
    voices: [Voice; MAXIMUM_HEADCOUNT],
    next_seniority: u64,
}

impl Headcount {
    fn hire(&mut self, pitch: u8, velocity: f32, headcount_limit: usize) {
        // Re-striking a held note reuses its voice rather than doubling it.
        let voice_index = self.voices[..headcount_limit]
            .iter()
            .position(|voice| voice.is_employed() && voice.pitch == pitch)
            .or_else(|| self.voices[..headcount_limit].iter().position(|voice| !voice.is_employed()))
            .unwrap_or_else(|| {
                // Hiring freeze: the most senior voice is restructured into the new note.
                (0..headcount_limit).min_by_key(|voice_index| self.voices[*voice_index].seniority).unwrap_or(0)
            });
        let voice = &mut self.voices[voice_index];
        let was_employed = voice.is_employed();
        voice.stage = EnvelopeStage::Attack;
        voice.pitch = pitch;
        voice.velocity = velocity;
        if !was_employed {
            voice.envelope_level = 0.0;
            voice.oscillator_one_phase = 0.0;
            voice.oscillator_two_phase = 0.25;
            voice.filter.reset();
        }
        voice.seniority = self.next_seniority;
        self.next_seniority += 1;
    }

    fn release(&mut self, pitch: u8) {
        for voice in self
            .voices
            .iter_mut()
            .filter(|voice| voice.is_employed() && voice.pitch == pitch && voice.stage != EnvelopeStage::Release)
        {
            voice.stage = EnvelopeStage::Release;
        }
    }

    fn handle_event(&mut self, note_event: &NoteEvent, headcount_limit: usize) {
        match note_event.kind {
            NoteEventKind::On { pitch, velocity } => self.hire(pitch, velocity, headcount_limit),
            NoteEventKind::Off { pitch } => self.release(pitch),
        }
    }

    fn render(&mut self, output_left: &mut [f32], output_right: &mut [f32], parameters: &[f32]) {
        let sample_rate = self.sample_rate;
        let oscillator_one_wave = parameters[OSCILLATOR_ONE_WAVE];
        let oscillator_two_wave = parameters[OSCILLATOR_TWO_WAVE];
        let oscillator_two_ratio = 2.0f32.powf(parameters[OSCILLATOR_TWO_OCTAVE].round() + parameters[OSCILLATOR_TWO_DETUNE] / 1200.0);
        let oscillator_two_mix = (parameters[OSCILLATOR_MIX] / 100.0).clamp(0.0, 1.0);
        let cutoff_hz = parameters[CUTOFF];
        let resonance = (parameters[RESONANCE] / 100.0).clamp(0.0, 1.0);
        let filter_envelope_amount = (parameters[FILTER_ENVELOPE] / 100.0).clamp(0.0, 1.0);
        let attack_step = 1.0 / (parameters[ATTACK].max(1.0) * 0.001 * sample_rate);
        let decay_coefficient = (-1.0 / (parameters[DECAY].max(1.0) * 0.001 * sample_rate)).exp();
        let sustain_level = (parameters[SUSTAIN] / 100.0).clamp(0.0, 1.0);
        let release_coefficient = (-1.0 / (parameters[RELEASE].max(1.0) * 0.001 * sample_rate)).exp();
        let output_gain = decibels_to_gain(parameters[VOLUME]) * 0.5;

        for (voice_index, voice) in self.voices.iter_mut().enumerate().filter(|(_, voice)| voice.is_employed()) {
            let base_frequency = note_frequency(f32::from(voice.pitch));
            let oscillator_one_increment = (base_frequency / sample_rate).min(0.49);
            let oscillator_two_increment = (base_frequency * oscillator_two_ratio / sample_rate).min(0.49);
            // Employees sit slightly left or right of centre, so a chord has width.
            let seat = if voice_index % 2 == 0 { -0.15 } else { 0.15 };
            let (left_gain, right_gain) = (1.0 - seat, 1.0 + seat);
            for (left_sample, right_sample) in output_left.iter_mut().zip(output_right.iter_mut()) {
                match voice.stage {
                    EnvelopeStage::Attack => {
                        voice.envelope_level += attack_step;
                        if voice.envelope_level >= 1.0 {
                            voice.envelope_level = 1.0;
                            voice.stage = EnvelopeStage::Decay;
                        }
                    }
                    EnvelopeStage::Decay => {
                        voice.envelope_level = sustain_level + (voice.envelope_level - sustain_level) * decay_coefficient;
                        if (voice.envelope_level - sustain_level).abs() < 1.0e-4 {
                            voice.envelope_level = sustain_level;
                            voice.stage = EnvelopeStage::Sustain;
                        }
                    }
                    EnvelopeStage::Sustain => voice.envelope_level = sustain_level,
                    EnvelopeStage::Release => {
                        voice.envelope_level *= release_coefficient;
                        if voice.envelope_level < LAYOFF_LEVEL {
                            voice.envelope_level = 0.0;
                            voice.stage = EnvelopeStage::Idle;
                            break;
                        }
                    }
                    EnvelopeStage::Idle => break,
                }
                let oscillator_one = oscillator_sample(oscillator_one_wave, voice.oscillator_one_phase, oscillator_one_increment);
                let oscillator_two = oscillator_sample(oscillator_two_wave, voice.oscillator_two_phase, oscillator_two_increment);
                voice.oscillator_one_phase = (voice.oscillator_one_phase + oscillator_one_increment).fract();
                voice.oscillator_two_phase = (voice.oscillator_two_phase + oscillator_two_increment).fract();
                let mixed_oscillators = oscillator_one * (1.0 - oscillator_two_mix) + oscillator_two * oscillator_two_mix;
                let swept_cutoff_hz = cutoff_hz * 2.0f32.powf(FILTER_ENVELOPE_OCTAVES * filter_envelope_amount * voice.envelope_level);
                let filtered = voice.filter.process(mixed_oscillators, swept_cutoff_hz, resonance, sample_rate).lowpass;
                let voice_sample = flush_denormal(filtered * voice.envelope_level * voice.velocity * output_gain);
                *left_sample += voice_sample * left_gain;
                *right_sample += voice_sample * right_gain;
            }
        }
    }

    pub fn voices_employed(&self) -> usize {
        self.voices.iter().filter(|voice| voice.is_employed()).count()
    }
}

impl SynergyPlugin for Headcount {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }

    fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            voices: [Voice::VACANT; MAXIMUM_HEADCOUNT],
            next_seniority: 1,
        }
    }

    fn reset(&mut self) {
        self.voices = [Voice::VACANT; MAXIMUM_HEADCOUNT];
    }

    fn process(&mut self, block: ProcessBlock<'_>) {
        block.output_left.fill(0.0);
        block.output_right.fill(0.0);
        let headcount_limit = (block.parameters[HEADCOUNT].round() as usize).clamp(1, MAXIMUM_HEADCOUNT);
        // Voices above a lowered limit finish their notes but take no new ones.
        let frame_count = block.frames();
        let mut segment_start = 0;
        let mut event_index = 0;
        while segment_start < frame_count {
            while event_index < block.events.len() && block.events[event_index].sample_offset <= segment_start {
                self.handle_event(&block.events[event_index], headcount_limit);
                event_index += 1;
            }
            let segment_end = block
                .events
                .get(event_index)
                .map_or(frame_count, |next_event| next_event.sample_offset.min(frame_count));
            self.render(
                &mut block.output_left[segment_start..segment_end],
                &mut block.output_right[segment_start..segment_end],
                block.parameters,
            );
            segment_start = segment_end;
        }
        block.meters[VOICES_EMPLOYED_METER] = Some(self.voices_employed() as f32);
    }
}

export_synergy_plugin!(Headcount);

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 48_000.0;

    fn default_parameters() -> Vec<f32> {
        PARAMETERS.iter().map(|parameter| parameter.default_plain as f32).collect()
    }

    fn run(synth: &mut Headcount, parameters: &[f32], frame_count: usize, events: &[NoteEvent]) -> (Vec<f32>, Vec<f32>, f32) {
        let silence = vec![0.0f32; frame_count];
        let mut output_left = vec![0.0; frame_count];
        let mut output_right = vec![0.0; frame_count];
        let mut meters = vec![None; PARAMETERS.len()];
        synth.process(ProcessBlock {
            sample_rate: SAMPLE_RATE,
            input_left: &silence,
            input_right: &silence,
            sidechain: None,
            output_left: &mut output_left,
            output_right: &mut output_right,
            parameters,
            meters: &mut meters,
            events,
            transport: Default::default(),
        });
        (output_left, output_right, meters[VOICES_EMPLOYED_METER].unwrap_or(-1.0))
    }

    fn note_on(sample_offset: usize, pitch: u8) -> NoteEvent {
        NoteEvent {
            sample_offset,
            kind: NoteEventKind::On { pitch, velocity: 1.0 },
        }
    }

    fn note_off(sample_offset: usize, pitch: u8) -> NoteEvent {
        NoteEvent {
            sample_offset,
            kind: NoteEventKind::Off { pitch },
        }
    }

    #[test]
    fn a4_plays_at_440_hz() {
        let mut parameters = default_parameters();
        parameters[OSCILLATOR_MIX] = 0.0;
        parameters[CUTOFF] = 18_000.0;
        parameters[FILTER_ENVELOPE] = 0.0;
        parameters[RESONANCE] = 0.0;
        let mut synth = Headcount::new(SAMPLE_RATE);
        let (output_left, _, voices_employed) = run(&mut synth, &parameters, 48_000, &[note_on(0, 69)]);
        assert_eq!(voices_employed, 1.0);
        // Count rising zero crossings over the last half second.
        let settled = &output_left[24_000..];
        let rising_crossings = settled.windows(2).filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0).count();
        let measured_frequency = rising_crossings as f32 / 0.5;
        assert!((measured_frequency - 440.0).abs() <= 2.0, "measured {measured_frequency} Hz");
        assert_eq!(note_frequency(60.0).round(), 262.0);
    }

    #[test]
    fn note_off_releases_and_lays_off_the_voice() {
        let mut parameters = default_parameters();
        parameters[RELEASE] = 50.0;
        let mut synth = Headcount::new(SAMPLE_RATE);
        let (held, _, _) = run(&mut synth, &parameters, 24_000, &[note_on(0, 57), note_on(0, 60), note_on(0, 64)]);
        assert!(held[12_000..].iter().any(|sample| sample.abs() > 0.05));
        let (released, _, voices_left) = run(&mut synth, &parameters, 48_000, &[note_off(0, 57), note_off(0, 60), note_off(0, 64)]);
        assert_eq!(voices_left, 0.0, "every voice should be laid off after the release");
        assert!(released[24_000..].iter().all(|sample| *sample == 0.0));
        assert!(held.iter().chain(&released).all(|sample| sample.is_finite()));
    }

    #[test]
    fn the_hiring_freeze_steals_the_oldest_voice() {
        let mut parameters = default_parameters();
        parameters[HEADCOUNT] = 2.0;
        let mut synth = Headcount::new(SAMPLE_RATE);
        run(&mut synth, &parameters, 480, &[note_on(0, 60), note_on(100, 64), note_on(200, 67)]);
        let employed_pitches: Vec<u8> = synth.voices.iter().filter(|voice| voice.is_employed()).map(|voice| voice.pitch).collect();
        assert_eq!(employed_pitches.len(), 2);
        assert!(employed_pitches.contains(&64) && employed_pitches.contains(&67), "{employed_pitches:?}");
    }

    #[test]
    fn chords_have_width_and_never_misbehave() {
        let mut parameters = default_parameters();
        parameters[RESONANCE] = 100.0;
        parameters[CUTOFF] = 18_000.0;
        let mut synth = Headcount::new(SAMPLE_RATE);
        let (output_left, output_right, _) = run(
            &mut synth,
            &parameters,
            48_000,
            &[note_on(0, 48), note_on(0, 55), note_on(0, 60), note_on(0, 63)],
        );
        assert!(output_left.iter().chain(&output_right).all(|sample| sample.is_finite() && sample.abs() < 4.0));
        assert_ne!(output_left, output_right);
    }
}
