//! # synergy-substrate
//!
//! The one place in slaw-vsts that speaks VST3. Every plugin in this repository
//! describes itself (name, class id, buses, parameters) and implements
//! [`SynergyPlugin`] for its DSP; the substrate does the rest: the factory, a
//! single-component processor + edit controller, bus negotiation, parameter
//! automation in and meters out, note events, host tempo and position, and a
//! state chunk so hosts can save and restore every knob.
//!
//! Plugins never see a raw pointer. All `unsafe` lives in [`component`] and
//! [`factory`], next to the comments that justify it.

pub mod component;
pub mod dsp;
pub mod factory;
pub mod parameters;
pub mod state;
mod strings;

pub use parameters::{ParameterSpecification, ParameterTaper};
pub use vst3;

/// Vendor string every plugin reports. slaw shows it next to the plugin name.
pub const VENDOR: &str = "slopstack labs";
pub const VENDOR_URL: &str = "https://github.com/slopstack-labs/slaw-vsts";
pub const VENDOR_EMAIL: &str = "synergy@slopstack.invalid";

/// Whether a plugin is an audio effect or a note-driven instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginKind {
    /// Stereo audio in, stereo audio out.
    Effect,
    /// Note events in, stereo audio out.
    Instrument,
}

/// Everything a host learns about a plugin before it makes a sound.
#[derive(Debug)]
pub struct PluginDescription {
    pub name: &'static str,
    /// The four 32-bit words of the VST3 class id. Never change them: projects refer to them.
    pub class_identifier: [u32; 4],
    pub kind: PluginKind,
    /// VST3 sub-categories, `|`-separated, e.g. `"Fx|Dynamics"` or `"Instrument|Synth"`.
    /// slaw treats any class whose sub-categories contain `Instrument` as an instrument.
    pub sub_categories: &'static str,
    pub version: &'static str,
    pub parameters: &'static [ParameterSpecification],
    /// Adds a stereo auxiliary input bus named "Sidechain" (effects only).
    pub sidechain_input: bool,
    /// Adds a note event input bus to an effect (instruments always have one).
    pub note_input: bool,
    /// How long the plugin keeps ringing after its input stops, in seconds.
    pub tail_seconds: f64,
}

impl PluginDescription {
    pub fn parameter_index(&self, identifier: u32) -> Option<usize> {
        self.parameters.iter().position(|parameter| parameter.identifier == identifier)
    }

    pub fn has_note_input(&self) -> bool {
        self.kind == PluginKind::Instrument || self.note_input
    }
}

/// A note event, already sorted by `sample_offset` within the block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteEvent {
    pub sample_offset: usize,
    pub kind: NoteEventKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NoteEventKind {
    On { pitch: u8, velocity: f32 },
    Off { pitch: u8 },
}

/// The host's transport, as far as it told us. Every field is optional because
/// hosts are free to leave any of them out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TransportSnapshot {
    pub tempo_bpm: Option<f64>,
    /// Musical position of the block's first sample, in quarter notes from the project start.
    pub project_quarter_notes: Option<f64>,
    /// Quarter-note position of the bar the block starts in.
    pub bar_start_quarter_notes: Option<f64>,
    pub time_signature: Option<(i32, i32)>,
    pub playing: bool,
}

/// One block of work handed to [`SynergyPlugin::process`].
pub struct ProcessBlock<'a> {
    pub sample_rate: f32,
    /// Main input (effects). Silence for instruments.
    pub input_left: &'a [f32],
    pub input_right: &'a [f32],
    /// The sidechain bus, when the plugin declared one and the host feeds it.
    pub sidechain: Option<(&'a [f32], &'a [f32])>,
    pub output_left: &'a mut [f32],
    pub output_right: &'a mut [f32],
    /// Plain parameter values, indexed like [`PluginDescription::parameters`].
    pub parameters: &'a [f32],
    /// Where read-only parameters (meters) report their plain value, same indexing.
    pub meters: &'a mut [Option<f32>],
    pub events: &'a [NoteEvent],
    pub transport: TransportSnapshot,
}

impl ProcessBlock<'_> {
    pub fn frames(&self) -> usize {
        self.output_left.len()
    }
}

/// The DSP half of a plugin. Implementations are plain Rust: no pointers, no COM.
pub trait SynergyPlugin: Send + 'static {
    fn description() -> &'static PluginDescription;

    /// Creates the processor for a sample rate. May allocate; never called on the audio thread.
    fn new(sample_rate: f32) -> Self;

    /// Clears delay lines, envelopes and voices (transport restarts, activation).
    fn reset(&mut self);

    /// Renders one block. Must not allocate, lock or block.
    fn process(&mut self, block: ProcessBlock<'_>);
}

/// Exports the VST3 entry points for one plugin type from a `cdylib` crate.
///
/// ```ignore
/// synergy_substrate::export_synergy_plugin!(KpiCompressor);
/// ```
#[macro_export]
macro_rules! export_synergy_plugin {
    ($plugin_type:ty) => {
        #[no_mangle]
        pub extern "system" fn GetPluginFactory() -> *mut $crate::vst3::Steinberg::IPluginFactory {
            $crate::factory::create_factory::<$plugin_type>()
        }

        #[cfg(target_os = "linux")]
        #[no_mangle]
        pub extern "system" fn ModuleEntry(_library_handle: *mut ::std::ffi::c_void) -> bool {
            true
        }

        #[cfg(target_os = "linux")]
        #[no_mangle]
        pub extern "system" fn ModuleExit() -> bool {
            true
        }

        #[cfg(target_os = "windows")]
        #[no_mangle]
        pub extern "system" fn InitDll() -> bool {
            true
        }

        #[cfg(target_os = "windows")]
        #[no_mangle]
        pub extern "system" fn ExitDll() -> bool {
            true
        }

        #[cfg(target_os = "macos")]
        #[no_mangle]
        pub extern "system" fn BundleEntry(_bundle_reference: *mut ::std::ffi::c_void) -> bool {
            true
        }

        #[cfg(target_os = "macos")]
        #[no_mangle]
        pub extern "system" fn BundleExit() -> bool {
            true
        }
    };
}
