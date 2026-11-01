//! Small DSP building blocks shared by the plugins: decibels, smoothing,
//! denormal hygiene, a zero-delay-feedback state-variable filter, a
//! band-limited oscillator step, RBJ biquads and a 4x oversampler for
//! anything that clips.

/// Values below this are flushed to zero so filters never crawl into denormals.
pub const DENORMAL_FLOOR: f32 = 1.0e-20;

#[inline]
pub fn flush_denormal(sample_value: f32) -> f32 {
    if sample_value.abs() < DENORMAL_FLOOR {
        0.0
    } else {
        sample_value
    }
}

#[inline]
pub fn decibels_to_gain(decibels: f32) -> f32 {
    10.0f32.powf(decibels / 20.0)
}

#[inline]
pub fn gain_to_decibels(gain: f32) -> f32 {
    20.0 * gain.max(1.0e-9).log10()
}

/// One-pole smoothing coefficient reaching ~63 % of a step in `time_milliseconds`.
#[inline]
pub fn smoothing_coefficient(time_milliseconds: f32, sample_rate: f32) -> f32 {
    if time_milliseconds <= 0.0 {
        return 0.0;
    }
    (-1.0 / (time_milliseconds * 0.001 * sample_rate)).exp()
}

/// Topology-preserving (zero-delay feedback) state-variable filter. Stable at
/// any cutoff below Nyquist, unlike the textbook Chamberlin form.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZeroDelayStateVariableFilter {
    first_integrator_state: f32,
    second_integrator_state: f32,
}

/// The three simultaneous outputs of [`ZeroDelayStateVariableFilter::process`].
#[derive(Clone, Copy, Debug)]
pub struct FilterOutputs {
    pub lowpass: f32,
    pub bandpass: f32,
    pub highpass: f32,
}

impl ZeroDelayStateVariableFilter {
    pub const fn new() -> Self {
        Self {
            first_integrator_state: 0.0,
            second_integrator_state: 0.0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// `resonance` 0..1 maps to damping 2..0.05 (self-oscillation stays just out of reach).
    #[inline]
    pub fn process(&mut self, input_sample: f32, cutoff_hz: f32, resonance: f32, sample_rate: f32) -> FilterOutputs {
        let safe_cutoff_hz = cutoff_hz.clamp(10.0, sample_rate * 0.49);
        let warped_frequency = (std::f32::consts::PI * safe_cutoff_hz / sample_rate).tan();
        let damping = 2.0 - 1.95 * resonance.clamp(0.0, 1.0);
        let input_gain = 1.0 / (1.0 + warped_frequency * (warped_frequency + damping));
        let band_gain = warped_frequency * input_gain;
        let low_gain = warped_frequency * band_gain;

        let input_difference = input_sample - self.second_integrator_state;
        let bandpass = input_gain * self.first_integrator_state + band_gain * input_difference;
        let lowpass = self.second_integrator_state + band_gain * self.first_integrator_state + low_gain * input_difference;
        self.first_integrator_state = flush_denormal(2.0 * bandpass - self.first_integrator_state);
        self.second_integrator_state = flush_denormal(2.0 * lowpass - self.second_integrator_state);
        FilterOutputs {
            lowpass,
            bandpass,
            highpass: input_sample - damping * bandpass - lowpass,
        }
    }
}

/// PolyBLEP residual: subtract from a naive saw/square at each discontinuity to
/// suppress most of the aliasing. `phase` and `phase_increment` are in cycles.
#[inline]
pub fn poly_blep(phase: f32, phase_increment: f32) -> f32 {
    if phase < phase_increment {
        let normalized_position = phase / phase_increment;
        2.0 * normalized_position - normalized_position * normalized_position - 1.0
    } else if phase > 1.0 - phase_increment {
        let normalized_position = (phase - 1.0) / phase_increment;
        normalized_position * normalized_position + 2.0 * normalized_position + 1.0
    } else {
        0.0
    }
}

// ── Biquads ──────────────────────────────────────────────────────────────────

/// Normalized biquad coefficients (a0 = 1), RBJ Audio EQ Cookbook designs, in f64 so
/// low corners at oversampled rates keep their precision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiquadCoefficients {
    feedforward_current: f64,
    feedforward_previous: f64,
    feedforward_second_previous: f64,
    feedback_previous: f64,
    feedback_second_previous: f64,
}

impl BiquadCoefficients {
    pub const IDENTITY: Self = Self {
        feedforward_current: 1.0,
        feedforward_previous: 0.0,
        feedforward_second_previous: 0.0,
        feedback_previous: 0.0,
        feedback_second_previous: 0.0,
    };

    fn normalized(raw_coefficients: [f64; 6]) -> Self {
        let [b0, b1, b2, a0, a1, a2] = raw_coefficients;
        Self {
            feedforward_current: b0 / a0,
            feedforward_previous: b1 / a0,
            feedforward_second_previous: b2 / a0,
            feedback_previous: a1 / a0,
            feedback_second_previous: a2 / a0,
        }
    }

    /// Cosine, sine and the bandwidth term alpha of a design frequency (clamped below Nyquist).
    fn design_terms(frequency_hz: f64, quality: f64, sample_rate: f64) -> (f64, f64, f64) {
        let safe_frequency_hz = frequency_hz.clamp(5.0, sample_rate * 0.49);
        let angular_frequency = 2.0 * std::f64::consts::PI * safe_frequency_hz / sample_rate;
        (
            angular_frequency.cos(),
            angular_frequency.sin(),
            angular_frequency.sin() / (2.0 * quality.max(0.05)),
        )
    }

    pub fn lowpass(frequency_hz: f64, quality: f64, sample_rate: f64) -> Self {
        let (cosine, _, alpha) = Self::design_terms(frequency_hz, quality, sample_rate);
        Self::normalized([
            (1.0 - cosine) / 2.0,
            1.0 - cosine,
            (1.0 - cosine) / 2.0,
            1.0 + alpha,
            -2.0 * cosine,
            1.0 - alpha,
        ])
    }

    pub fn highpass(frequency_hz: f64, quality: f64, sample_rate: f64) -> Self {
        let (cosine, _, alpha) = Self::design_terms(frequency_hz, quality, sample_rate);
        Self::normalized([
            (1.0 + cosine) / 2.0,
            -(1.0 + cosine),
            (1.0 + cosine) / 2.0,
            1.0 + alpha,
            -2.0 * cosine,
            1.0 - alpha,
        ])
    }

    pub fn peaking(frequency_hz: f64, quality: f64, gain_decibels: f64, sample_rate: f64) -> Self {
        let (cosine, _, alpha) = Self::design_terms(frequency_hz, quality, sample_rate);
        let amplitude = 10.0f64.powf(gain_decibels / 40.0);
        Self::normalized([
            1.0 + alpha * amplitude,
            -2.0 * cosine,
            1.0 - alpha * amplitude,
            1.0 + alpha / amplitude,
            -2.0 * cosine,
            1.0 - alpha / amplitude,
        ])
    }

    /// Shelf with slope 1: `high` picks the high shelf, otherwise the low shelf.
    pub fn shelf(frequency_hz: f64, gain_decibels: f64, sample_rate: f64, high: bool) -> Self {
        let (cosine, sine, _) = Self::design_terms(frequency_hz, 1.0, sample_rate);
        let amplitude = 10.0f64.powf(gain_decibels / 40.0);
        let shelf_alpha = sine / 2.0 * std::f64::consts::SQRT_2;
        let root_term = 2.0 * amplitude.sqrt() * shelf_alpha;
        let (plus, minus) = (amplitude + 1.0, amplitude - 1.0);
        if high {
            Self::normalized([
                amplitude * (plus + minus * cosine + root_term),
                -2.0 * amplitude * (minus + plus * cosine),
                amplitude * (plus + minus * cosine - root_term),
                plus - minus * cosine + root_term,
                2.0 * (minus - plus * cosine),
                plus - minus * cosine - root_term,
            ])
        } else {
            Self::normalized([
                amplitude * (plus - minus * cosine + root_term),
                2.0 * amplitude * (minus - plus * cosine),
                amplitude * (plus - minus * cosine - root_term),
                plus + minus * cosine + root_term,
                -2.0 * (minus + plus * cosine),
                plus + minus * cosine - root_term,
            ])
        }
    }

    /// Linear magnitude response at `frequency_hz`.
    pub fn magnitude_at(&self, frequency_hz: f64, sample_rate: f64) -> f64 {
        let angular_frequency = 2.0 * std::f64::consts::PI * frequency_hz / sample_rate;
        let (first_cosine, first_sine) = (angular_frequency.cos(), angular_frequency.sin());
        let (second_cosine, second_sine) = ((2.0 * angular_frequency).cos(), (2.0 * angular_frequency).sin());
        let numerator_real = self.feedforward_current + self.feedforward_previous * first_cosine + self.feedforward_second_previous * second_cosine;
        let numerator_imaginary = -(self.feedforward_previous * first_sine + self.feedforward_second_previous * second_sine);
        let denominator_real = 1.0 + self.feedback_previous * first_cosine + self.feedback_second_previous * second_cosine;
        let denominator_imaginary = -(self.feedback_previous * first_sine + self.feedback_second_previous * second_sine);
        numerator_real.hypot(numerator_imaginary) / denominator_real.hypot(denominator_imaginary)
    }
}

/// Transposed direct form II state; flushed so tails reach exact silence.
#[derive(Clone, Copy, Debug, Default)]
pub struct BiquadState {
    first_state: f64,
    second_state: f64,
}

impl BiquadState {
    #[inline]
    pub fn process(&mut self, coefficients: &BiquadCoefficients, input_sample: f64) -> f64 {
        let output_sample = coefficients.feedforward_current * input_sample + self.first_state;
        self.first_state = flush_state(coefficients.feedforward_previous * input_sample - coefficients.feedback_previous * output_sample + self.second_state);
        self.second_state = flush_state(coefficients.feedforward_second_previous * input_sample - coefficients.feedback_second_previous * output_sample);
        output_sample
    }
}

#[inline]
fn flush_state(state_value: f64) -> f64 {
    if state_value.abs() < 1.0e-24 {
        0.0
    } else {
        state_value
    }
}

// ── Oversampling ─────────────────────────────────────────────────────────────

/// Nonlinear stages run at this multiple of the host rate, so clipping harmonics above
/// Nyquist are filtered away instead of folding back as aliasing.
pub const OVERSAMPLING_FACTOR: usize = 4;

/// Quality factors of the four biquads that make an 8th-order Butterworth lowpass.
const BUTTERWORTH_EIGHTH_ORDER_QUALITIES: [f64; 4] = [0.509_795_6, 0.601_344_9, 0.899_976_2, 2.562_915_4];

/// The anti-imaging and anti-aliasing filter for [`FourTimesOversampler`]: an 8th-order
/// Butterworth at 0.45 of the host rate (at most 20 kHz), designed at the oversampled rate.
pub fn oversampling_filter(sample_rate: f64) -> [BiquadCoefficients; 4] {
    let cutoff_hz = (sample_rate * 0.45).min(20_000.0);
    let oversampled_rate = sample_rate * OVERSAMPLING_FACTOR as f64;
    BUTTERWORTH_EIGHTH_ORDER_QUALITIES.map(|quality| BiquadCoefficients::lowpass(cutoff_hz, quality, oversampled_rate))
}

/// Runs a nonlinearity at four times the host rate: zero-stuff, filter the images, apply the
/// nonlinearity, filter what it created above the host's Nyquist, keep every fourth sample.
#[derive(Clone, Copy, Debug, Default)]
pub struct FourTimesOversampler {
    upsampling: [BiquadState; 4],
    downsampling: [BiquadState; 4],
}

impl FourTimesOversampler {
    /// One host-rate sample through `nonlinearity`, which is called four times at the oversampled rate.
    #[inline]
    pub fn process(&mut self, filter: &[BiquadCoefficients; 4], input_sample: f64, mut nonlinearity: impl FnMut(f64) -> f64) -> f64 {
        let mut decimated_sample = 0.0;
        for oversample_index in 0..OVERSAMPLING_FACTOR {
            // Zero-stuffing scaled by the factor keeps unity gain once the images are filtered out.
            let mut oversampled_sample = if oversample_index == 0 {
                input_sample * OVERSAMPLING_FACTOR as f64
            } else {
                0.0
            };
            for (state, coefficients) in self.upsampling.iter_mut().zip(filter.iter()) {
                oversampled_sample = state.process(coefficients, oversampled_sample);
            }
            let mut downsampled_sample = nonlinearity(oversampled_sample);
            for (state, coefficients) in self.downsampling.iter_mut().zip(filter.iter()) {
                downsampled_sample = state.process(coefficients, downsampled_sample);
            }
            if oversample_index == OVERSAMPLING_FACTOR - 1 {
                decimated_sample = downsampled_sample;
            }
        }
        decimated_sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biquad_designs_hit_their_marks() {
        let lowpass = BiquadCoefficients::lowpass(1_000.0, std::f64::consts::FRAC_1_SQRT_2, 48_000.0);
        assert!((lowpass.magnitude_at(1_000.0, 48_000.0) - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        assert!(lowpass.magnitude_at(100.0, 48_000.0) > 0.99);
        let peak = BiquadCoefficients::peaking(750.0, 0.8, 6.0, 48_000.0);
        assert!((20.0 * peak.magnitude_at(750.0, 48_000.0).log10() - 6.0).abs() < 1e-3);
        let low_shelf = BiquadCoefficients::shelf(120.0, -9.0, 48_000.0, false);
        assert!((20.0 * low_shelf.magnitude_at(20.0, 48_000.0).log10() + 9.0).abs() < 0.5);
        assert!(low_shelf.magnitude_at(5_000.0, 48_000.0) > 0.98);
        let high_shelf = BiquadCoefficients::shelf(2_800.0, 9.0, 48_000.0, true);
        assert!((20.0 * high_shelf.magnitude_at(15_000.0, 48_000.0).log10() - 9.0).abs() < 0.6);
    }

    #[test]
    fn the_oversampler_is_transparent_for_a_linear_stage() {
        let filter = oversampling_filter(48_000.0);
        let mut oversampler = FourTimesOversampler::default();
        let mut peak_output = 0.0f64;
        for sample_index in 0..48_000 {
            let input_sample = (2.0 * std::f64::consts::PI * 1_000.0 * sample_index as f64 / 48_000.0).sin();
            let output_sample = oversampler.process(&filter, input_sample, |oversampled_sample| oversampled_sample);
            if sample_index > 24_000 {
                peak_output = peak_output.max(output_sample.abs());
            }
        }
        assert!((peak_output - 1.0).abs() < 0.01, "1 kHz through the oversampler peaks at {peak_output}");
    }

    #[test]
    fn decibel_conversions_agree() {
        assert!((decibels_to_gain(-6.0) - 0.501_187).abs() < 1e-5);
        assert!((gain_to_decibels(decibels_to_gain(-18.0)) + 18.0).abs() < 1e-4);
    }

    #[test]
    fn zero_delay_filter_stays_stable_at_extreme_settings() {
        let mut filter = ZeroDelayStateVariableFilter::default();
        let sample_rate = 44_100.0;
        let mut peak_output = 0.0f32;
        for sample_index in 0..44_100 {
            let input_sample = (sample_index as f32 * 0.07).sin();
            let outputs = filter.process(input_sample, 21_000.0, 1.0, sample_rate);
            assert!(outputs.lowpass.is_finite());
            peak_output = peak_output.max(outputs.lowpass.abs());
        }
        assert!(peak_output < 50.0, "peak {peak_output}");
    }

    #[test]
    fn lowpass_passes_lows_and_cuts_highs() {
        let sample_rate = 48_000.0;
        let measure = |frequency_hz: f32| {
            let mut filter = ZeroDelayStateVariableFilter::default();
            let mut peak_output = 0.0f32;
            for sample_index in 0..48_000 {
                let input_sample = (2.0 * std::f32::consts::PI * frequency_hz * sample_index as f32 / sample_rate).sin();
                let outputs = filter.process(input_sample, 1_000.0, 0.0, sample_rate);
                if sample_index > 24_000 {
                    peak_output = peak_output.max(outputs.lowpass.abs());
                }
            }
            peak_output
        };
        assert!(measure(100.0) > 0.95);
        assert!(measure(10_000.0) < 0.05);
    }
}
