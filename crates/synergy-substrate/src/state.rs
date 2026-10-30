//! The state chunk hosts save with a project: a magic tag, a format version,
//! then every non-meter parameter as (id, normalized value). Unknown ids are
//! skipped on load, so adding parameters never breaks old projects.

use crate::ParameterSpecification;

const STATE_MAGIC: &[u8; 8] = b"SYNERGY1";
const STATE_FORMAT_VERSION: u32 = 1;

/// Serializes the normalized values of every savable parameter.
pub fn encode_state(parameters: &[ParameterSpecification], normalized_values: &[f64]) -> Vec<u8> {
    let savable_parameters: Vec<(u32, f64)> = parameters
        .iter()
        .zip(normalized_values)
        .filter(|(parameter, _)| !parameter.read_only)
        .map(|(parameter, normalized_value)| (parameter.identifier, *normalized_value))
        .collect();
    let mut encoded_state = Vec::with_capacity(16 + savable_parameters.len() * 12);
    encoded_state.extend_from_slice(STATE_MAGIC);
    encoded_state.extend_from_slice(&STATE_FORMAT_VERSION.to_le_bytes());
    encoded_state.extend_from_slice(&(savable_parameters.len() as u32).to_le_bytes());
    for (identifier, normalized_value) in savable_parameters {
        encoded_state.extend_from_slice(&identifier.to_le_bytes());
        encoded_state.extend_from_slice(&normalized_value.to_le_bytes());
    }
    encoded_state
}

/// Parses a state chunk into (id, normalized value) pairs. Returns `None` for
/// anything that is not one of ours, so a host's stray chunk is ignored.
pub fn decode_state(encoded_state: &[u8]) -> Option<Vec<(u32, f64)>> {
    let header = encoded_state.get(..16)?;
    if &header[..8] != STATE_MAGIC {
        return None;
    }
    let format_version = u32::from_le_bytes(header[8..12].try_into().ok()?);
    if format_version != STATE_FORMAT_VERSION {
        return None;
    }
    let declared_count = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
    let entries = &encoded_state[16..];
    let available_count = entries.len() / 12;
    let mut decoded_values = Vec::with_capacity(declared_count.min(available_count));
    for entry in entries.chunks_exact(12).take(declared_count) {
        let identifier = u32::from_le_bytes(entry[..4].try_into().ok()?);
        let normalized_value = f64::from_le_bytes(entry[4..].try_into().ok()?);
        if normalized_value.is_finite() {
            decoded_values.push((identifier, normalized_value.clamp(0.0, 1.0)));
        }
    }
    Some(decoded_values)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMETERS: &[ParameterSpecification] = &[
        ParameterSpecification::linear(0, "Threshold", "dB", -60.0, 0.0, -18.0),
        ParameterSpecification::linear(7, "Gain reduction", "dB", 0.0, 48.0, 0.0).as_meter(),
        ParameterSpecification::logarithmic(3, "Attack", "ms", 0.1, 100.0, 10.0),
    ];

    #[test]
    fn state_round_trips_and_skips_meters() {
        let encoded_state = encode_state(PARAMETERS, &[0.25, 0.9, 0.75]);
        assert_eq!(decode_state(&encoded_state), Some(vec![(0, 0.25), (3, 0.75)]));
    }

    #[test]
    fn foreign_or_truncated_chunks_are_rejected_gracefully() {
        assert_eq!(decode_state(b"not ours at all, sorry"), None);
        let mut encoded_state = encode_state(PARAMETERS, &[0.25, 0.9, 0.75]);
        encoded_state.truncate(encoded_state.len() - 5);
        assert_eq!(decode_state(&encoded_state), Some(vec![(0, 0.25)]));
    }
}
