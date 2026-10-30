//! Parameter governance: how a knob's plain value (dB, ms, Hz) maps onto the
//! 0..1 normalized value every VST3 host speaks.

/// How a parameter's plain range is spread across the normalized 0..1 range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParameterTaper {
    /// Plain value moves linearly with the knob.
    Linear,
    /// Plain value moves exponentially (frequencies, times); `minimum` must be > 0.
    Logarithmic,
    /// Whole-number steps from `minimum` to `maximum`, labelled by `value_labels` when given.
    Stepped,
}

/// One automatable (or read-only) parameter, described once and used by the
/// controller, the processor, the state chunk and the tests.
#[derive(Clone, Copy, Debug)]
pub struct ParameterSpecification {
    /// The VST3 parameter id. Stable forever: saved projects and automation refer to it.
    pub identifier: u32,
    pub title: &'static str,
    pub short_title: &'static str,
    pub units: &'static str,
    pub minimum: f64,
    pub maximum: f64,
    pub default_plain: f64,
    pub taper: ParameterTaper,
    /// Labels for [`ParameterTaper::Stepped`] parameters, one per step, in order.
    pub value_labels: &'static [&'static str],
    /// Reported to the host as an output (meters): never automated, never saved.
    pub read_only: bool,
}

impl ParameterSpecification {
    /// A continuous parameter with a linear taper.
    pub const fn linear(identifier: u32, title: &'static str, units: &'static str, minimum: f64, maximum: f64, default_plain: f64) -> Self {
        Self {
            identifier,
            title,
            short_title: title,
            units,
            minimum,
            maximum,
            default_plain,
            taper: ParameterTaper::Linear,
            value_labels: &[],
            read_only: false,
        }
    }

    /// A continuous parameter with a logarithmic taper (times, frequencies, ratios).
    pub const fn logarithmic(identifier: u32, title: &'static str, units: &'static str, minimum: f64, maximum: f64, default_plain: f64) -> Self {
        Self {
            taper: ParameterTaper::Logarithmic,
            ..Self::linear(identifier, title, units, minimum, maximum, default_plain)
        }
    }

    /// A list parameter: one step per label, plain values 0, 1, 2, …
    pub const fn list(identifier: u32, title: &'static str, value_labels: &'static [&'static str], default_index: usize) -> Self {
        Self {
            identifier,
            title,
            short_title: title,
            units: "",
            minimum: 0.0,
            maximum: (value_labels.len() - 1) as f64,
            default_plain: default_index as f64,
            taper: ParameterTaper::Stepped,
            value_labels,
            read_only: false,
        }
    }

    /// Whole-number steps from `minimum` to `maximum` without labels (voice counts, semitones).
    pub const fn stepped(identifier: u32, title: &'static str, units: &'static str, minimum: f64, maximum: f64, default_plain: f64) -> Self {
        Self {
            taper: ParameterTaper::Stepped,
            ..Self::linear(identifier, title, units, minimum, maximum, default_plain)
        }
    }

    /// Marks the parameter as a read-only output (a meter).
    pub const fn as_meter(self) -> Self {
        Self { read_only: true, ..self }
    }

    /// Number of discrete steps the host should offer: 0 for continuous parameters.
    pub fn step_count(&self) -> i32 {
        match self.taper {
            ParameterTaper::Stepped => (self.maximum - self.minimum).round().max(1.0) as i32,
            _ => 0,
        }
    }

    pub fn plain_from_normalized(&self, normalized_value: f64) -> f64 {
        let clamped_normalized = if normalized_value.is_finite() {
            normalized_value.clamp(0.0, 1.0)
        } else {
            self.default_normalized()
        };
        match self.taper {
            ParameterTaper::Linear => self.minimum + clamped_normalized * (self.maximum - self.minimum),
            ParameterTaper::Logarithmic => self.minimum * (self.maximum / self.minimum).powf(clamped_normalized),
            ParameterTaper::Stepped => {
                let step_count = self.step_count() as f64;
                self.minimum + (clamped_normalized * step_count).round().min(step_count)
            }
        }
    }

    pub fn normalized_from_plain(&self, plain_value: f64) -> f64 {
        if !plain_value.is_finite() {
            return self.default_normalized();
        }
        let clamped_plain = plain_value.clamp(self.minimum, self.maximum);
        let normalized_value = match self.taper {
            ParameterTaper::Linear => (clamped_plain - self.minimum) / (self.maximum - self.minimum),
            ParameterTaper::Logarithmic => (clamped_plain / self.minimum).ln() / (self.maximum / self.minimum).ln(),
            ParameterTaper::Stepped => (clamped_plain.round() - self.minimum) / self.step_count() as f64,
        };
        normalized_value.clamp(0.0, 1.0)
    }

    pub fn default_normalized(&self) -> f64 {
        self.normalized_from_plain(self.default_plain)
    }

    /// The text a host shows for a plain value: "-12.0 dB", "4.0:1", "Saw".
    pub fn format_plain(&self, plain_value: f64) -> String {
        if self.taper == ParameterTaper::Stepped {
            let step_index = (plain_value - self.minimum).round().max(0.0) as usize;
            if let Some(label) = self.value_labels.get(step_index) {
                return (*label).to_string();
            }
            return format!("{}", plain_value.round() as i64);
        }
        let magnitude = plain_value.abs();
        let digits = if magnitude >= 1000.0 {
            0
        } else if magnitude >= 10.0 {
            1
        } else {
            2
        };
        format!("{plain_value:.digits$}")
    }

    /// Parses host-typed text back into a plain value: a number, or a list label.
    pub fn parse_plain(&self, typed_text: &str) -> Option<f64> {
        let trimmed_text = typed_text.trim();
        if self.taper == ParameterTaper::Stepped {
            if let Some(label_index) = self.value_labels.iter().position(|label| label.eq_ignore_ascii_case(trimmed_text)) {
                return Some(self.minimum + label_index as f64);
            }
        }
        let numeric_prefix: String = trimmed_text
            .chars()
            .take_while(|character| character.is_ascii_digit() || matches!(character, '-' | '+' | '.' | 'e' | 'E'))
            .collect();
        numeric_prefix.parse::<f64>().ok().filter(|parsed_value| parsed_value.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logarithmic_taper_round_trips_and_hits_its_ends() {
        let cutoff = ParameterSpecification::logarithmic(0, "Cutoff", "Hz", 20.0, 20000.0, 1000.0);
        assert!((cutoff.plain_from_normalized(0.0) - 20.0).abs() < 1e-9);
        assert!((cutoff.plain_from_normalized(1.0) - 20000.0).abs() < 1e-6);
        assert!((cutoff.plain_from_normalized(0.5) - 632.4555).abs() < 1e-3);
        for plain_value in [20.0, 440.0, 1000.0, 19999.0] {
            let round_tripped = cutoff.plain_from_normalized(cutoff.normalized_from_plain(plain_value));
            assert!((round_tripped - plain_value).abs() / plain_value < 1e-9);
        }
    }

    #[test]
    fn list_parameters_snap_to_their_labels() {
        let waveform = ParameterSpecification::list(1, "Wave", &["Saw", "Square", "Triangle"], 1);
        assert_eq!(waveform.step_count(), 2);
        assert_eq!(waveform.plain_from_normalized(0.2), 0.0);
        assert_eq!(waveform.plain_from_normalized(0.6), 1.0);
        assert_eq!(waveform.format_plain(2.0), "Triangle");
        assert_eq!(waveform.parse_plain("square"), Some(1.0));
        assert!((waveform.default_normalized() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn hostile_values_fall_back_to_the_default() {
        let gain = ParameterSpecification::linear(2, "Gain", "dB", -24.0, 24.0, 0.0);
        assert_eq!(gain.plain_from_normalized(f64::NAN), 0.0);
        assert_eq!(gain.normalized_from_plain(f64::INFINITY), 0.5);
        assert_eq!(gain.parse_plain("-6.5 dB"), Some(-6.5));
        assert_eq!(gain.parse_plain("synergy"), None);
    }
}
