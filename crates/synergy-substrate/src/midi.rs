//! MIDI controller governance. VST3 never delivers pitch bend, the mod wheel or
//! other continuous controllers as events: the host asks the edit controller
//! (`IMidiMapping::getMidiControllerAssignment`) which parameter each controller
//! should drive, then sends the controller as an ordinary parameter change.
//!
//! A plugin declares those assignments here, in plain Rust, and the substrate
//! answers the host. Plugin crates never see a VST3 controller number.

/// A MIDI controller a plugin can ask the host to route to one of its parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiController {
    /// The pitch bend wheel. The host sends it centred at normalized 0.5.
    PitchBend,
    /// CC 1, the modulation wheel. The host sends 0..127 as normalized 0..1.
    ModWheel,
    /// Channel pressure (mono aftertouch).
    ChannelAftertouch,
    /// Any other control change, 0..=127 (CC 1 is [`MidiController::ModWheel`]).
    ControlChange(u8),
}

impl MidiController {
    /// The VST3 `CtrlNumber` the host asks about (`Steinberg::Vst::ControllerNumbers_`).
    pub const fn vst3_controller_number(self) -> i16 {
        match self {
            MidiController::PitchBend => vst3::Steinberg::Vst::ControllerNumbers_::kPitchBend as i16,
            MidiController::ModWheel => vst3::Steinberg::Vst::ControllerNumbers_::kCtrlModWheel as i16,
            MidiController::ChannelAftertouch => vst3::Steinberg::Vst::ControllerNumbers_::kAfterTouch as i16,
            // An out-of-range CC maps to a number no host ever asks about, so it simply never routes.
            MidiController::ControlChange(control_change_number) if control_change_number <= 127 => control_change_number as i16,
            MidiController::ControlChange(_) => -1,
        }
    }
}

/// One controller → parameter route, declared in a [`crate::PluginDescription`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiControllerAssignment {
    pub controller: MidiController,
    /// The VST3 parameter id the controller drives. It must be a writable (non-meter) parameter.
    pub parameter_identifier: u32,
}

impl MidiControllerAssignment {
    pub const fn new(controller: MidiController, parameter_identifier: u32) -> Self {
        Self {
            controller,
            parameter_identifier,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controllers_speak_the_vst3_numbering() {
        assert_eq!(MidiController::PitchBend.vst3_controller_number(), 129);
        assert_eq!(MidiController::ModWheel.vst3_controller_number(), 1);
        assert_eq!(MidiController::ChannelAftertouch.vst3_controller_number(), 128);
        assert_eq!(MidiController::ControlChange(74).vst3_controller_number(), 74);
        assert_eq!(
            MidiController::ControlChange(1).vst3_controller_number(),
            MidiController::ModWheel.vst3_controller_number()
        );
        assert_eq!(MidiController::ControlChange(200).vst3_controller_number(), -1);
    }
}
