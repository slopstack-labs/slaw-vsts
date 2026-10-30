//! Small DSP building blocks shared by the plugins: decibels, smoothing,
//! denormal hygiene, a zero-delay-feedback state-variable filter and a
//! band-limited oscillator step.

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

#[cfg(test)]
mod tests {
    use super::*;

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
