//! Drives a substrate plugin through the real VST3 COM interfaces, the way a
//! host does: factory → component → buses → state → process. If this passes,
//! the plugin crates only have their DSP left to get wrong.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use synergy_substrate::factory::create_factory;
use synergy_substrate::vst3::Steinberg::Vst::*;
use synergy_substrate::vst3::Steinberg::*;
use synergy_substrate::vst3::{Class, ComPtr, ComWrapper, Interface};
use synergy_substrate::{ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin};

static PARAMETERS: [ParameterSpecification; 2] = [
    ParameterSpecification::linear(5, "Gain", "x", 0.0, 2.0, 1.0),
    ParameterSpecification::linear(9, "Level", "x", 0.0, 2.0, 0.0).as_meter(),
];

static DESCRIPTION: PluginDescription = PluginDescription {
    name: "Contract Gain",
    class_identifier: [0x11111111, 0x22222222, 0x33333333, 0x44444444],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Tools",
    version: "9.9.9",
    parameters: &PARAMETERS,
    sidechain_input: true,
    note_input: false,
    tail_seconds: 0.0,
};

/// Multiplies the main input by Gain, adds the sidechain, meters the peak.
struct ContractGain;

impl SynergyPlugin for ContractGain {
    fn description() -> &'static PluginDescription {
        &DESCRIPTION
    }
    fn new(_sample_rate: f32) -> Self {
        ContractGain
    }
    fn reset(&mut self) {}
    fn process(&mut self, block: ProcessBlock<'_>) {
        let gain = block.parameters[0];
        let mut peak = 0.0f32;
        for frame_index in 0..block.frames() {
            let (sidechain_left, sidechain_right) = block.sidechain.map_or((0.0, 0.0), |(left, right)| (left[frame_index], right[frame_index]));
            block.output_left[frame_index] = block.input_left[frame_index] * gain + sidechain_left;
            block.output_right[frame_index] = block.input_right[frame_index] * gain + sidechain_right;
            peak = peak.max(block.output_left[frame_index].abs());
        }
        block.meters[1] = Some(peak);
    }
}

/// An in-memory IBStream, as hosts provide for getState/setState.
#[derive(Default)]
struct MemoryStream {
    bytes: RefCell<Vec<u8>>,
    position: Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, byte_count: int32, bytes_read: *mut int32) -> tresult {
        let bytes = self.bytes.borrow();
        let available = bytes.len() - self.position.get();
        let copy_count = available.min(byte_count.max(0) as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr().add(self.position.get()), buffer.cast::<u8>(), copy_count);
        self.position.set(self.position.get() + copy_count);
        if !bytes_read.is_null() {
            *bytes_read = copy_count as int32;
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, byte_count: int32, bytes_written: *mut int32) -> tresult {
        let incoming = std::slice::from_raw_parts(buffer.cast::<u8>(), byte_count.max(0) as usize);
        self.bytes.borrow_mut().extend_from_slice(incoming);
        if !bytes_written.is_null() {
            *bytes_written = byte_count;
        }
        kResultOk
    }
    unsafe fn seek(&self, position: int64, _mode: int32, result: *mut int64) -> tresult {
        self.position.set(position.max(0) as usize);
        if !result.is_null() {
            *result = position;
        }
        kResultOk
    }
    unsafe fn tell(&self, position: *mut int64) -> tresult {
        *position = self.position.get() as int64;
        kResultOk
    }
}

fn class_id() -> TUID {
    synergy_substrate::vst3::uid(0x11111111, 0x22222222, 0x33333333, 0x44444444)
}

unsafe fn create_component() -> ComPtr<IComponent> {
    let factory = ComPtr::from_raw(create_factory::<ContractGain>()).expect("factory");
    let mut instance: *mut c_void = std::ptr::null_mut();
    let class_identifier = class_id();
    let interface_identifier = IComponent::IID;
    let result = factory.createInstance(class_identifier.as_ptr(), interface_identifier.as_ptr() as FIDString, &mut instance);
    assert_eq!(result, kResultOk);
    ComPtr::from_raw(instance as *mut IComponent).expect("component")
}

#[test]
fn the_factory_describes_the_class_like_slaw_expects() {
    unsafe {
        let factory = ComPtr::from_raw(create_factory::<ContractGain>()).expect("factory");
        let factory_two = factory.cast::<IPluginFactory2>().expect("IPluginFactory2");
        let mut class_info: PClassInfo2 = std::mem::zeroed();
        assert_eq!(factory_two.countClasses(), 1);
        assert_eq!(factory_two.getClassInfo2(0, &mut class_info), kResultOk);
        let text = |characters: &[std::ffi::c_char]| std::ffi::CStr::from_ptr(characters.as_ptr()).to_string_lossy().into_owned();
        assert_eq!(text(&class_info.category), "Audio Module Class");
        assert_eq!(text(&class_info.name), "Contract Gain");
        assert_eq!(text(&class_info.subCategories), "Fx|Tools");
        assert_eq!(text(&class_info.vendor), "slopstack labs");
        assert_eq!(class_info.cid, class_id());
        assert!(factory.cast::<IPluginFactory3>().is_some());

        let mut wrong_class = class_id();
        wrong_class[0] ^= 1;
        let mut instance: *mut c_void = std::ptr::null_mut();
        assert_ne!(
            factory.createInstance(wrong_class.as_ptr(), IComponent::IID.as_ptr() as FIDString, &mut instance),
            kResultOk
        );
        assert!(instance.is_null());
    }
}

#[test]
fn one_object_is_component_processor_and_controller() {
    unsafe {
        let component = create_component();
        assert!(component.cast::<IAudioProcessor>().is_some());
        let controller = component.cast::<IEditController>().expect("single component exposes IEditController");
        assert_eq!(controller.getParameterCount(), 2);
        let mut info: ParameterInfo = std::mem::zeroed();
        assert_eq!(controller.getParameterInfo(1, &mut info), kResultOk);
        assert_eq!(info.id, 9);
        assert_ne!(info.flags & ParameterInfo_::ParameterFlags_::kIsReadOnly as int32, 0);

        // Buses: stereo main in, stereo aux "Sidechain" (inactive by default), stereo out.
        assert_eq!(
            component.getBusCount(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection),
            2
        );
        let mut bus: BusInfo = std::mem::zeroed();
        assert_eq!(
            component.getBusInfo(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection, 1, &mut bus),
            kResultOk
        );
        assert_eq!(bus.busType, BusTypes_::kAux as BusType);
        assert_eq!(bus.channelCount, 2);
        assert_eq!(bus.flags, 0);
        let name: String = String::from_utf16_lossy(&bus.name.iter().take_while(|unit| **unit != 0).copied().collect::<Vec<_>>());
        assert_eq!(name, "Sidechain");
        assert_eq!(
            component.getBusCount(MediaTypes_::kEvent as MediaType, BusDirections_::kInput as BusDirection),
            0
        );
    }
}

#[test]
fn state_round_trips_through_a_host_stream() {
    unsafe {
        let original = create_component();
        let original_controller = original.cast::<IEditController>().unwrap();
        assert_eq!(original_controller.setParamNormalized(5, 0.25), kResultOk);

        let stream = ComWrapper::new(MemoryStream::default());
        let stream_pointer = stream.to_com_ptr::<IBStream>().unwrap();
        assert_eq!(original.getState(stream_pointer.as_ptr()), kResultOk);

        let restored = create_component();
        let restored_controller = restored.cast::<IEditController>().unwrap();
        assert!((restored_controller.getParamNormalized(5) - 0.5).abs() < 1e-12, "default before restore");
        stream.position.set(0);
        assert_eq!(restored.setState(stream_pointer.as_ptr()), kResultOk);
        assert!((restored_controller.getParamNormalized(5) - 0.25).abs() < 1e-12);

        // A foreign chunk is refused without touching anything.
        let foreign = ComWrapper::new(MemoryStream::default());
        foreign.bytes.borrow_mut().extend_from_slice(b"definitely not synergy");
        assert_ne!(restored.setState(foreign.to_com_ptr::<IBStream>().unwrap().as_ptr()), kResultOk);
        assert!((restored_controller.getParamNormalized(5) - 0.25).abs() < 1e-12);
    }
}

#[test]
fn process_handles_in_place_buffers_and_an_absent_sidechain() {
    unsafe {
        let component = create_component();
        let processor = component.cast::<IAudioProcessor>().unwrap();
        let controller = component.cast::<IEditController>().unwrap();
        let mut setup = ProcessSetup {
            processMode: 0,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: 64,
            sampleRate: 48_000.0,
        };
        assert_eq!(processor.setupProcessing(&mut setup), kResultOk);
        assert_eq!(component.setActive(1), kResultOk);
        controller.setParamNormalized(5, 0.25); // gain 0.5

        // In-place: input and output share the same channel memory, as many hosts do.
        let mut left = [1.0f32; 64];
        let mut right = [-1.0f32; 64];
        let mut channels = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut main_bus: AudioBusBuffers = std::mem::zeroed();
        main_bus.numChannels = 2;
        main_bus.__field0.channelBuffers32 = channels.as_mut_ptr();
        // The host leaves the sidechain bus inactive: zero channels, null buffers.
        let mut inputs = [main_bus, std::mem::zeroed()];
        let mut outputs = [main_bus];
        let mut data: ProcessData = std::mem::zeroed();
        data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
        data.numSamples = 64;
        data.numInputs = 2;
        data.numOutputs = 1;
        data.inputs = inputs.as_mut_ptr();
        data.outputs = outputs.as_mut_ptr();
        assert_eq!(processor.process(&mut data), kResultOk);
        assert!(left.iter().all(|sample| (*sample - 0.5).abs() < 1e-6));
        assert!(right.iter().all(|sample| (*sample + 0.5).abs() < 1e-6));
        // The meter (read-only id 9) reports the block peak back through getParamNormalized.
        assert!((controller.getParamNormalized(9) - 0.25).abs() < 1e-6);

        // Zero-frame calls (parameter flushes) are fine too.
        data.numSamples = 0;
        assert_eq!(processor.process(&mut data), kResultOk);
    }
}
