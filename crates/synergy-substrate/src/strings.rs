//! Copying Rust strings into the fixed-size C and UTF-16 arrays VST3 structs use.

use std::ffi::c_char;

use vst3::Steinberg::char16;

/// Copies `source` into a NUL-terminated 8-bit buffer, truncating if needed.
pub fn copy_to_char8(source: &str, destination: &mut [c_char]) {
    let Some(last_index) = destination.len().checked_sub(1) else {
        return;
    };
    let mut written_count = 0;
    for (source_byte, destination_byte) in source.bytes().filter(|byte| *byte != 0).zip(destination[..last_index].iter_mut()) {
        *destination_byte = source_byte as c_char;
        written_count += 1;
    }
    destination[written_count] = 0;
}

/// Copies `source` into a NUL-terminated UTF-16 buffer, truncating if needed.
pub fn copy_to_char16(source: &str, destination: &mut [char16]) {
    let Some(last_index) = destination.len().checked_sub(1) else {
        return;
    };
    let mut written_count = 0;
    for (source_unit, destination_unit) in source.encode_utf16().zip(destination[..last_index].iter_mut()) {
        *destination_unit = source_unit as char16;
        written_count += 1;
    }
    destination[written_count] = 0;
}

/// Reads a NUL-terminated UTF-16 string the host handed us.
///
/// # Safety
/// `source` must point to a NUL-terminated UTF-16 string (VST3 guarantees this for `TChar*`).
pub unsafe fn read_char16(source: *const char16) -> Option<String> {
    if source.is_null() {
        return None;
    }
    let mut unit_count = 0usize;
    // Bounded: VST3 strings in these calls are String128 at most.
    while unit_count < 512 && *source.add(unit_count) != 0 {
        unit_count += 1;
    }
    let units = std::slice::from_raw_parts(source, unit_count);
    String::from_utf16(units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_truncate_and_terminate() {
        let mut narrow = [1 as c_char; 6];
        copy_to_char8("synergy", &mut narrow);
        assert_eq!(narrow.map(|value| value as u8), *b"syner\0");

        let mut wide = [1 as char16; 4];
        copy_to_char16("KPI", &mut wide);
        assert_eq!(wide, [b'K' as char16, b'P' as char16, b'I' as char16, 0]);
        let round_tripped = unsafe { read_char16(wide.as_ptr()) };
        assert_eq!(round_tripped.as_deref(), Some("KPI"));
    }
}
