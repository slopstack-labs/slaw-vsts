//! # Headcount
//!
//! Scales headcount up to 8 voices and lays them off on note-off. A polyphonic
//! subtractive synth: two band-limited oscillators (saw or square, the second
//! detunable and octave-shiftable), a resonant zero-delay-feedback lowpass
//! with an envelope sweep, an ADSR, velocity, and oldest-first voice stealing
//! when the hiring freeze kicks in. Pitch bend and the mod wheel arrive as
//! MIDI-mapped parameters: bend glides every voice up to ±Bend Range, and the
//! mod wheel dials in a 5.5 Hz vibrato up to Vibrato Depth.

use synergy_substrate::dsp::{decibels_to_gain, flush_denormal, poly_blep, ZeroDelayStateVariableFilter};
use synergy_substrate::{
    export_synergy_plugin, MidiController, MidiControllerAssignment, NoteEvent, NoteEventKind, ParameterSpecification, PluginDescription, PluginKind,
    ProcessBlock, SynergyPlugin,
};

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
pub const PITCH_BEND: usize = 15;
pub const MOD_WHEEL: usize = 16;
pub const BEND_RANGE: usize = 17;
pub const VIBRATO_DEPTH: usize = 18;

const WAVEFORMS: &[&str] = &["Saw", "Square"];

static PARAMETERS: [ParameterSpecification; 19] = [
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
    // Performance controllers, appended with fresh ids. Visible and automatable on purpose:
    // hosts without MIDI mapping (and agents writing automation lanes) can still bend.
    ParameterSpecification::linear(15, "Pitch Bend", "%", -100.0, 100.0, 0.0),
    ParameterSpecification::linear(16, "Mod Wheel", "%", 0.0, 100.0, 0.0),
    ParameterSpecification::stepped(17, "Bend Range", "st", 0.0, 24.0, 2.0),
    ParameterSpecification::linear(18, "Vibrato Depth", "ct", 0.0, 100.0, 30.0),
];

/// The host sends the bend wheel and CC 1 to these parameters on any channel.
static MIDI_CONTROLLER_ASSIGNMENTS: [MidiControllerAssignment; 2] = [
    MidiControllerAssignment::new(MidiController::PitchBend, PARAMETERS[PITCH_BEND].identifier),
    MidiControllerAssignment::new(MidiController::ModWheel, PARAMETERS[MOD_WHEEL].identifier),
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
    midi_controller_assignments: &MIDI_CONTROLLER_ASSIGNMENTS,
};

const MAXIMUM_HEADCOUNT: usize = 8;
/// The filter envelope opens the cutoff by up to this many octaves.
const FILTER_ENVELOPE_OCTAVES: f32 = 4.0;
/// A voice whose release has fallen below this is laid off.
const LAYOFF_LEVEL: f32 = 1.0e-4;
/// The mod wheel's vibrato rate: the classic, stakeholder-approved wobble.
const VIBRATO_RATE_HZ: f32 = 5.5;
/// Time constant that turns a host's stepped bend and mod-wheel values into a glide.
const PERFORMANCE_SMOOTHING_SECONDS: f32 = 0.008;
/// Pitch modulation is computed once per frame for this many frames, shared by every voice.
const MODULATION_CHUNK_FRAMES: usize = 32;

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

/// Pitch bend and vibrato, shared by every voice: smoothed toward the host's
/// latest controller values and turned into a per-frame frequency ratio.
#[derive(Clone, Copy, Debug)]
struct PerformanceControllerSubstrate {
    smoothed_bend_semitones: f32,
    smoothed_vibrato_cents: f32,
    /// Vibrato position in cycles, 0..1.
    modulation_phase: f32,
    modulation_phase_increment: f32,
    smoothing_coefficient: f32,
    /// False until the first block, so a project that opens bent starts bent instead of gliding there.
    primed: bool,
}

impl PerformanceControllerSubstrate {
    fn new(sample_rate: f32) -> Self {
        Self {
            smoothed_bend_semitones: 0.0,
            smoothed_vibrato_cents: 0.0,
            modulation_phase: 0.0,
            modulation_phase_increment: VIBRATO_RATE_HZ / sample_rate,
            smoothing_coefficient: (-1.0 / (PERFORMANCE_SMOOTHING_SECONDS * sample_rate)).exp(),
            primed: false,
        }
    }

    fn reset(&mut self) {
        self.modulation_phase = 0.0;
        self.primed = false;
    }

    /// Fills `pitch_ratios` with one frequency multiplier per frame.
    fn render_pitch_ratios(&mut self, pitch_ratios: &mut [f32], bend_target_semitones: f32, vibrato_target_cents: f32) {
        if !self.primed {
            self.smoothed_bend_semitones = bend_target_semitones;
            self.smoothed_vibrato_cents = vibrato_target_cents;
            self.primed = true;
        }
        for pitch_ratio in pitch_ratios.iter_mut() {
            // The remaining distance decays exponentially and is flushed once it turns sub-normal.
            self.smoothed_bend_semitones =
                bend_target_semitones + flush_denormal((self.smoothed_bend_semitones - bend_target_semitones) * self.smoothing_coefficient);
            self.smoothed_vibrato_cents =
                vibrato_target_cents + flush_denormal((self.smoothed_vibrato_cents - vibrato_target_cents) * self.smoothing_coefficient);
            let vibrato_cents = self.smoothed_vibrato_cents * (std::f32::consts::TAU * self.modulation_phase).sin();
            self.modulation_phase = (self.modulation_phase + self.modulation_phase_increment).fract();
            *pitch_ratio = 2.0f32.powf(self.smoothed_bend_semitones / 12.0 + vibrato_cents / 1200.0);
        }
    }
}

pub struct Headcount {
    sample_rate: f32,
    voices: [Voice; MAXIMUM_HEADCOUNT],
    next_seniority: u64,
    performance_controllers: PerformanceControllerSubstrate,
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
        let bend_target_semitones = (parameters[PITCH_BEND] / 100.0).clamp(-1.0, 1.0) * parameters[BEND_RANGE].max(0.0);
        let vibrato_target_cents = (parameters[MOD_WHEEL] / 100.0).clamp(0.0, 1.0) * parameters[VIBRATO_DEPTH].max(0.0);

        let frame_count = output_left.len().min(output_right.len());
        let mut chunk_start = 0;
        while chunk_start < frame_count {
            let chunk_end = (chunk_start + MODULATION_CHUNK_FRAMES).min(frame_count);
            let mut pitch_ratio_storage = [1.0f32; MODULATION_CHUNK_FRAMES];
            let pitch_ratios = &mut pitch_ratio_storage[..chunk_end - chunk_start];
            self.performance_controllers
                .render_pitch_ratios(pitch_ratios, bend_target_semitones, vibrato_target_cents);

            for (voice_index, voice) in self.voices.iter_mut().enumerate().filter(|(_, voice)| voice.is_employed()) {
                let base_increment = note_frequency(f32::from(voice.pitch)) / sample_rate;
                // Employees sit slightly left or right of centre, so a chord has width.
                let seat = if voice_index % 2 == 0 { -0.15 } else { 0.15 };
                let (left_gain, right_gain) = (1.0 - seat, 1.0 + seat);
                for ((left_sample, right_sample), pitch_ratio) in output_left[chunk_start..chunk_end]
                    .iter_mut()
                    .zip(output_right[chunk_start..chunk_end].iter_mut())
                    .zip(pitch_ratios.iter())
                {
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
                    // Bend and vibrato move both oscillators together, so the detune interval holds.
                    let oscillator_one_increment = (base_increment * pitch_ratio).min(0.49);
                    let oscillator_two_increment = (base_increment * oscillator_two_ratio * pitch_ratio).min(0.49);
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
            chunk_start = chunk_end;
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
            performance_controllers: PerformanceControllerSubstrate::new(sample_rate),
        }
    }

    fn reset(&mut self) {
        self.voices = [Voice::VACANT; MAXIMUM_HEADCOUNT];
        self.performance_controllers.reset();
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

    /// One oscillator through a gentle lowpass: near-sinusoidal, one rising zero crossing per cycle.
    fn sine_ish_parameters() -> Vec<f32> {
        let mut parameters = default_parameters();
        parameters[OSCILLATOR_MIX] = 0.0;
        parameters[CUTOFF] = 1_200.0;
        parameters[RESONANCE] = 0.0;
        parameters[FILTER_ENVELOPE] = 0.0;
        parameters
    }

    /// Rising zero crossings, in fractional frames (linearly interpolated between samples).
    fn rising_crossing_positions(samples: &[f32]) -> Vec<f64> {
        samples
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0] < 0.0 && pair[1] >= 0.0)
            .map(|(frame_index, pair)| frame_index as f64 + f64::from(pair[0]) / f64::from(pair[0] - pair[1]))
            .collect()
    }

    /// Average fundamental across whole cycles, in Hz.
    fn mean_frequency(samples: &[f32]) -> f64 {
        let crossing_positions = rising_crossing_positions(samples);
        let cycle_count = crossing_positions.len() - 1;
        f64::from(SAMPLE_RATE) * cycle_count as f64 / (crossing_positions[cycle_count] - crossing_positions[0])
    }

    /// Cycle-by-cycle frequency, in Hz.
    fn per_cycle_frequencies(samples: &[f32]) -> Vec<f64> {
        rising_crossing_positions(samples)
            .windows(2)
            .map(|pair| f64::from(SAMPLE_RATE) / (pair[1] - pair[0]))
            .collect()
    }

    fn render_held_a4(parameters: &[f32], frame_count: usize) -> Vec<f32> {
        let mut synth = Headcount::new(SAMPLE_RATE);
        let (output_left, _, _) = run(&mut synth, parameters, frame_count, &[note_on(0, 69)]);
        output_left
    }

    #[test]
    fn pitch_bend_moves_the_fundamental_by_the_bend_range() {
        let centred = sine_ish_parameters();
        let centred_frequency = mean_frequency(&render_held_a4(&centred, 48_000)[24_000..]);
        assert!((centred_frequency - 440.0).abs() < 0.2, "centred at {centred_frequency} Hz");

        let mut bent_up = sine_ish_parameters();
        bent_up[PITCH_BEND] = 100.0;
        bent_up[BEND_RANGE] = 2.0;
        let bent_up_frequency = mean_frequency(&render_held_a4(&bent_up, 48_000)[24_000..]);
        let expected_ratio = 2.0f64.powf(2.0 / 12.0);
        assert!(
            (bent_up_frequency / centred_frequency - expected_ratio).abs() < 1.0e-3,
            "+2 st bend: {bent_up_frequency} Hz, ratio {} vs {expected_ratio}",
            bent_up_frequency / centred_frequency
        );

        // Full bend down with a 12-semitone range is an octave, on both oscillators.
        let mut bent_down = sine_ish_parameters();
        bent_down[OSCILLATOR_MIX] = 100.0;
        bent_down[OSCILLATOR_TWO_DETUNE] = 0.0;
        bent_down[PITCH_BEND] = -100.0;
        bent_down[BEND_RANGE] = 12.0;
        let bent_down_frequency = mean_frequency(&render_held_a4(&bent_down, 48_000)[24_000..]);
        assert!((bent_down_frequency - 220.0).abs() < 0.2, "octave down: {bent_down_frequency} Hz");
    }

    #[test]
    fn a_stepped_bend_glides_instead_of_zippering() {
        let mut parameters = sine_ish_parameters();
        let mut synth = Headcount::new(SAMPLE_RATE);
        run(&mut synth, &parameters, 480, &[note_on(0, 69)]);
        assert_eq!(synth.performance_controllers.smoothed_bend_semitones, 0.0);

        // The host jumps the wheel to full in one step; the oscillators follow over milliseconds.
        parameters[PITCH_BEND] = 100.0;
        run(&mut synth, &parameters, 48, &[]);
        let after_one_millisecond = synth.performance_controllers.smoothed_bend_semitones;
        assert!(
            after_one_millisecond > 0.05 && after_one_millisecond < 1.0,
            "{after_one_millisecond} st after 1 ms"
        );
        run(&mut synth, &parameters, 4_800, &[]);
        let after_a_hundred_milliseconds = synth.performance_controllers.smoothed_bend_semitones;
        assert!(
            (after_a_hundred_milliseconds - 2.0).abs() < 1.0e-4,
            "{after_a_hundred_milliseconds} st after 100 ms"
        );
    }

    #[test]
    fn the_mod_wheel_dials_in_vibrato() {
        let mut parameters = sine_ish_parameters();
        parameters[VIBRATO_DEPTH] = 30.0;

        parameters[MOD_WHEEL] = 0.0;
        let steady = per_cycle_frequencies(&render_held_a4(&parameters, 96_000)[24_000..]);
        let steady_spread = steady.iter().cloned().fold(f64::MIN, f64::max) - steady.iter().cloned().fold(f64::MAX, f64::min);
        assert!(steady_spread < 0.5, "mod wheel at 0 should not wobble: {steady_spread} Hz spread");

        parameters[MOD_WHEEL] = 100.0;
        let output = render_held_a4(&parameters, 96_000);
        let wobbling = per_cycle_frequencies(&output[24_000..]);
        let highest = wobbling.iter().cloned().fold(f64::MIN, f64::max);
        let lowest = wobbling.iter().cloned().fold(f64::MAX, f64::min);
        // ±30 cents around 440 Hz is 432.4 to 447.7 Hz.
        let expected_spread = 440.0 * (2.0f64.powf(30.0 / 1200.0) - 2.0f64.powf(-30.0 / 1200.0));
        assert!(
            (highest - lowest) > 0.85 * expected_spread && (highest - lowest) < 1.15 * expected_spread,
            "vibrato spread {} Hz, expected about {expected_spread} Hz",
            highest - lowest
        );
        // Vibrato swings around the note rather than detuning it: 1.5 s holds 8.25 cycles of 5.5 Hz.
        let vibrato_centre = mean_frequency(&output[24_000..]);
        assert!((vibrato_centre - 440.0).abs() < 1.5, "vibrato centred at {vibrato_centre} Hz");
        // And it really is about 5.5 Hz: count how often the frequency crosses the centre upward.
        let upward_passes = wobbling.windows(2).filter(|pair| pair[0] < 440.0 && pair[1] >= 440.0).count();
        assert!((7..=10).contains(&upward_passes), "{upward_passes} vibrato cycles in 1.5 s");
    }

    #[test]
    fn performance_controllers_never_misbehave_or_leave_denormals() {
        let mut parameters = default_parameters();
        parameters[RESONANCE] = 100.0;
        parameters[CUTOFF] = 18_000.0;
        parameters[PITCH_BEND] = 100.0;
        parameters[BEND_RANGE] = 24.0;
        parameters[MOD_WHEEL] = 100.0;
        parameters[VIBRATO_DEPTH] = 100.0;
        parameters[RELEASE] = 20.0;
        let mut synth = Headcount::new(SAMPLE_RATE);
        let (held_left, held_right, _) = run(&mut synth, &parameters, 24_000, &[note_on(0, 24), note_on(0, 96), note_on(0, 127)]);
        // Let go of both wheels and every key: the glide back and the tail must settle to exact zeros.
        parameters[PITCH_BEND] = 0.0;
        parameters[MOD_WHEEL] = 0.0;
        let (released_left, released_right, voices_left) = run(&mut synth, &parameters, 48_000, &[note_off(0, 24), note_off(0, 96), note_off(0, 127)]);
        assert!(held_left
            .iter()
            .chain(&held_right)
            .chain(&released_left)
            .chain(&released_right)
            .all(|sample| sample.is_finite() && sample.abs() < 4.0));
        assert_eq!(voices_left, 0.0);
        assert!(released_left[24_000..].iter().chain(&released_right[24_000..]).all(|sample| *sample == 0.0));
        let performance_controllers = synth.performance_controllers;
        for smoothed_value in [performance_controllers.smoothed_bend_semitones, performance_controllers.smoothed_vibrato_cents] {
            assert_eq!(smoothed_value, 0.0, "the glide lands exactly, never in sub-normal territory");
        }
    }

    #[test]
    fn controllers_route_to_their_parameters_and_ids_stay_put() {
        assert_eq!(
            DESCRIPTION.parameter_for_midi_controller(MidiController::PitchBend.vst3_controller_number()),
            Some(15)
        );
        assert_eq!(
            DESCRIPTION.parameter_for_midi_controller(MidiController::ModWheel.vst3_controller_number()),
            Some(16)
        );
        assert_eq!(
            DESCRIPTION.parameter_for_midi_controller(MidiController::ChannelAftertouch.vst3_controller_number()),
            None
        );
        // Parameter ids are forever: index n is id n, and the meter keeps 14.
        for (parameter_index, parameter) in PARAMETERS.iter().enumerate() {
            assert_eq!(parameter.identifier as usize, parameter_index, "{}", parameter.title);
        }
        assert!(PARAMETERS[VOICES_EMPLOYED_METER].read_only);
        assert!((PARAMETERS[PITCH_BEND].default_normalized() - 0.5).abs() < 1.0e-12, "bend rests at the centre");
    }

    #[test]
    fn a_project_saved_before_the_wheels_existed_keeps_its_settings() {
        use synergy_substrate::state::{decode_state, encode_state};
        // The 15-parameter Headcount saved a darker patch; today's Headcount reads it back.
        let legacy_parameters = &PARAMETERS[..=VOICES_EMPLOYED_METER];
        let mut legacy_values: Vec<f64> = legacy_parameters.iter().map(|parameter| parameter.default_normalized()).collect();
        legacy_values[CUTOFF] = 0.2;
        legacy_values[RELEASE] = 0.9;
        let decoded_values = decode_state(&encode_state(legacy_parameters, &legacy_values)).expect("an older Headcount chunk");
        assert!(decoded_values.contains(&(PARAMETERS[CUTOFF].identifier, 0.2)));
        assert!(decoded_values.contains(&(PARAMETERS[RELEASE].identifier, 0.9)));
        // The new ids are absent, so the substrate leaves them at their defaults (centred bend, no vibrato).
        for new_index in [PITCH_BEND, MOD_WHEEL, BEND_RANGE, VIBRATO_DEPTH] {
            assert!(decoded_values.iter().all(|(identifier, _)| *identifier != PARAMETERS[new_index].identifier));
        }
    }
}
