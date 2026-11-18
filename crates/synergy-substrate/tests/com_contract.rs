//! Drives a substrate plugin through the real VST3 COM interfaces, the way a
//! host does: factory → component → buses → state → process. If this passes,
//! the plugin crates only have their DSP left to get wrong.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use synergy_substrate::automation::AUTOMATION_SUB_BLOCK_FRAMES;
use synergy_substrate::factory::create_factory;
use synergy_substrate::vst3::Steinberg::Vst::*;
use synergy_substrate::vst3::Steinberg::*;
use synergy_substrate::vst3::{Class, ComPtr, ComWrapper, Interface};
use synergy_substrate::{
    MidiController, MidiControllerAssignment, NoteEventKind, ParameterSpecification, PluginDescription, PluginKind, ProcessBlock, SynergyPlugin,
    TransportSnapshot,
};

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
    midi_controller_assignments: &[],
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

/// Version 2 of a contract instrument: version 1 shipped with Level alone, then the
/// performance controllers were appended with fresh ids (and a meter, to prove the
/// substrate never routes a controller onto one).
static SYNTH_PARAMETERS: [ParameterSpecification; 5] = [
    ParameterSpecification::linear(0, "Level", "%", 0.0, 100.0, 50.0),
    ParameterSpecification::linear(1, "Pitch Bend", "%", -100.0, 100.0, 0.0),
    ParameterSpecification::linear(2, "Mod Wheel", "%", 0.0, 100.0, 0.0),
    ParameterSpecification::linear(3, "Brightness", "%", 0.0, 100.0, 100.0),
    ParameterSpecification::linear(4, "Pressure Meter", "%", 0.0, 100.0, 0.0).as_meter(),
];

static SYNTH_MIDI_ASSIGNMENTS: [MidiControllerAssignment; 4] = [
    MidiControllerAssignment::new(MidiController::PitchBend, 1),
    MidiControllerAssignment::new(MidiController::ModWheel, 2),
    MidiControllerAssignment::new(MidiController::ControlChange(74), 3),
    // Deliberately misdeclared: a meter is read-only, so the substrate must refuse to route onto it.
    MidiControllerAssignment::new(MidiController::ChannelAftertouch, 4),
];

static SYNTH_DESCRIPTION: PluginDescription = PluginDescription {
    name: "Contract Synth",
    class_identifier: [0x55555555, 0x66666666, 0x77777777, 0x88888888],
    kind: PluginKind::Instrument,
    sub_categories: "Instrument|Synth",
    version: "9.9.9",
    parameters: &SYNTH_PARAMETERS,
    sidechain_input: false,
    note_input: true,
    tail_seconds: 0.0,
    midi_controller_assignments: &SYNTH_MIDI_ASSIGNMENTS,
};

/// Silence with opinions: only its description and controller routes are under test.
struct ContractSynth;

impl SynergyPlugin for ContractSynth {
    fn description() -> &'static PluginDescription {
        &SYNTH_DESCRIPTION
    }
    fn new(_sample_rate: f32) -> Self {
        ContractSynth
    }
    fn reset(&mut self) {}
    fn process(&mut self, block: ProcessBlock<'_>) {
        block.output_left.fill(0.0);
        block.output_right.fill(0.0);
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
    create_component_of::<ContractGain>()
}

unsafe fn create_component_of<P: SynergyPlugin>() -> ComPtr<IComponent> {
    let factory = ComPtr::from_raw(create_factory::<P>()).expect("factory");
    let mut instance: *mut c_void = std::ptr::null_mut();
    let [first_word, second_word, third_word, fourth_word] = P::description().class_identifier;
    let class_identifier = synergy_substrate::vst3::uid(first_word, second_word, third_word, fourth_word);
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

// ---------------------------------------------------------------------------------------------
// Automation: long host blocks are split into sub-blocks that follow the ramps.
// ---------------------------------------------------------------------------------------------

/// Counts heap allocations per thread, so a test can assert the audio path makes none.
struct AllocationAuditor;

thread_local! {
    static ALLOCATIONS_ON_THIS_THREAD: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    let _ = ALLOCATIONS_ON_THIS_THREAD.try_with(|allocation_count| allocation_count.set(allocation_count.get() + 1));
}

fn allocations_on_this_thread() -> usize {
    ALLOCATIONS_ON_THIS_THREAD.with(Cell::get)
}

// SAFETY: forwards every call to the system allocator unchanged; the counter is a const-initialised
// thread-local Cell, which neither allocates nor has a destructor.
unsafe impl GlobalAlloc for AllocationAuditor {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        System.realloc(pointer, layout, new_size)
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout)
    }
}

#[global_allocator]
static ALLOCATION_AUDITOR: AllocationAuditor = AllocationAuditor;

const PROBE_RAMP: usize = 0;
const PROBE_STEADY: usize = 1;
const PROBE_SIGNED_METER: usize = 2;

static PROBE_PARAMETERS: [ParameterSpecification; 3] = [
    ParameterSpecification::linear(0, "Ramp", "x", 0.0, 1.0, 0.0),
    ParameterSpecification::linear(1, "Steady", "x", 0.0, 1.0, 0.5),
    ParameterSpecification::linear(2, "Signed Meter", "x", -8.0, 8.0, 0.0).as_meter(),
];

static PROBE_DESCRIPTION: PluginDescription = PluginDescription {
    name: "Contract Probe",
    class_identifier: [0x55555555, 0x66666666, 0x77777777, 0x88888888],
    kind: PluginKind::Effect,
    sub_categories: "Fx|Tools",
    version: "9.9.9",
    parameters: &PROBE_PARAMETERS,
    sidechain_input: false,
    note_input: true,
    tail_seconds: 0.0,
    midi_controller_assignments: &[],
};

/// What the probe saw in one `process` call.
#[derive(Clone, Copy, Debug)]
struct ProbeCall {
    frames: usize,
    ramp: f32,
    steady: f32,
    transport: TransportSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ProbeEvent {
    call_index: usize,
    sample_offset: usize,
    pitch: u8,
}

thread_local! {
    static PROBE_CALLS: RefCell<Vec<ProbeCall>> = const { RefCell::new(Vec::new()) };
    static PROBE_EVENTS: RefCell<Vec<ProbeEvent>> = const { RefCell::new(Vec::new()) };
}

/// Clears the probe's log and reserves room, so logging never allocates inside `process`.
fn reset_probe_log() {
    PROBE_CALLS.with(|calls| {
        let mut calls = calls.borrow_mut();
        calls.clear();
        calls.reserve(1_024);
    });
    PROBE_EVENTS.with(|events| {
        let mut events = events.borrow_mut();
        events.clear();
        events.reserve(1_024);
    });
}

fn probe_calls() -> Vec<ProbeCall> {
    PROBE_CALLS.with(|calls| calls.borrow().clone())
}

fn probe_events() -> Vec<ProbeEvent> {
    PROBE_EVENTS.with(|events| events.borrow().clone())
}

/// Logs every call (frames, parameters, events, transport), writes Ramp to the left output and
/// Steady to the right, and meters −4 × Ramp, so the block meter must pick the largest magnitude.
struct ContractProbe;

impl SynergyPlugin for ContractProbe {
    fn description() -> &'static PluginDescription {
        &PROBE_DESCRIPTION
    }
    fn new(_sample_rate: f32) -> Self {
        ContractProbe
    }
    fn reset(&mut self) {}
    fn process(&mut self, block: ProcessBlock<'_>) {
        let call_index = PROBE_CALLS.with(|calls| {
            let mut calls = calls.borrow_mut();
            calls.push(ProbeCall {
                frames: block.frames(),
                ramp: block.parameters[PROBE_RAMP],
                steady: block.parameters[PROBE_STEADY],
                transport: block.transport,
            });
            calls.len() - 1
        });
        PROBE_EVENTS.with(|events| {
            let mut events = events.borrow_mut();
            for note_event in block.events {
                let pitch = match note_event.kind {
                    NoteEventKind::On { pitch, .. } | NoteEventKind::Off { pitch } => pitch,
                };
                events.push(ProbeEvent {
                    call_index,
                    sample_offset: note_event.sample_offset,
                    pitch,
                });
            }
        });
        block.output_left.fill(block.parameters[PROBE_RAMP]);
        block.output_right.fill(block.parameters[PROBE_STEADY]);
        block.meters[PROBE_SIGNED_METER] = Some(-4.0 * block.parameters[PROBE_RAMP]);
    }
}

/// A host's automation queue for one parameter.
struct HostValueQueue {
    parameter_identifier: ParamID,
    points: Vec<(int32, ParamValue)>,
}

impl Class for HostValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for HostValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.parameter_identifier
    }
    unsafe fn getPointCount(&self) -> int32 {
        self.points.len() as int32
    }
    unsafe fn getPoint(&self, index: int32, sample_offset: *mut int32, value: *mut ParamValue) -> tresult {
        let Some((point_offset, point_value)) = usize::try_from(index).ok().and_then(|index| self.points.get(index)) else {
            return kInvalidArgument;
        };
        *sample_offset = *point_offset;
        *value = *point_value;
        kResultOk
    }
    unsafe fn addPoint(&self, _sample_offset: int32, _value: ParamValue, _index: *mut int32) -> tresult {
        kResultFalse
    }
}

/// A host's IParameterChanges: one queue per automated parameter.
struct HostParameterChanges {
    queues: Vec<ComPtr<IParamValueQueue>>,
}

impl Class for HostParameterChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for HostParameterChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.queues.get(index))
            .map_or(std::ptr::null_mut(), |queue| queue.as_ptr())
    }
    unsafe fn addParameterData(&self, _identifier: *const ParamID, _index: *mut int32) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

fn host_parameter_changes(queues: &[(ParamID, &[(int32, ParamValue)])]) -> ComPtr<IParameterChanges> {
    let queues = queues
        .iter()
        .map(|(parameter_identifier, points)| {
            ComWrapper::new(HostValueQueue {
                parameter_identifier: *parameter_identifier,
                points: points.to_vec(),
            })
            .to_com_ptr::<IParamValueQueue>()
            .unwrap()
        })
        .collect();
    ComWrapper::new(HostParameterChanges { queues }).to_com_ptr::<IParameterChanges>().unwrap()
}

/// A host's event list of note-ons.
struct HostEventList {
    events: Vec<Event>,
}

impl Class for HostEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for HostEventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.events.len() as int32
    }
    unsafe fn getEvent(&self, index: int32, event: *mut Event) -> tresult {
        match usize::try_from(index).ok().and_then(|index| self.events.get(index)) {
            Some(host_event) => {
                *event = *host_event;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, _event: *mut Event) -> tresult {
        kResultFalse
    }
}

fn host_note_ons(note_ons: &[(int32, i16)]) -> ComPtr<IEventList> {
    let events = note_ons
        .iter()
        .map(|(sample_offset, pitch)| {
            // SAFETY: Event is a plain C struct; all-zero is a valid (empty) value.
            let mut host_event: Event = unsafe { std::mem::zeroed() };
            host_event.sampleOffset = *sample_offset;
            host_event.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
            host_event.__field0.noteOn = NoteOnEvent {
                channel: 0,
                pitch: *pitch,
                tuning: 0.0,
                velocity: 1.0,
                length: 0,
                noteId: -1,
            };
            host_event
        })
        .collect();
    ComWrapper::new(HostEventList { events }).to_com_ptr::<IEventList>().unwrap()
}

/// A playing transport as a host reports it at a block's first frame.
fn host_process_context(tempo_bpm: f64, project_quarter_notes: f64, bar_start_quarter_notes: f64, time_signature: (i32, i32)) -> ProcessContext {
    use ProcessContext_::StatesAndFlags_::*;
    // SAFETY: ProcessContext is a plain C struct; all-zero is a valid (empty) value.
    let mut context: ProcessContext = unsafe { std::mem::zeroed() };
    context.state = (kPlaying | kTempoValid | kProjectTimeMusicValid | kBarPositionValid | kTimeSigValid) as uint32;
    context.sampleRate = 48_000.0;
    context.tempo = tempo_bpm;
    context.projectTimeMusic = project_quarter_notes;
    context.barPositionMusic = bar_start_quarter_notes;
    context.timeSigNumerator = time_signature.0;
    context.timeSigDenominator = time_signature.1;
    context
}

/// Everything a host passes with one block besides the audio.
struct HostBlockContext<'a> {
    parameter_changes: Option<&'a ComPtr<IParameterChanges>>,
    input_events: Option<&'a ComPtr<IEventList>>,
    process_context: Option<&'a mut ProcessContext>,
}

const NO_HOST_CONTEXT: HostBlockContext<'static> = HostBlockContext {
    parameter_changes: None,
    input_events: None,
    process_context: None,
};

/// Sets a component up for 48 kHz stereo with room for `maximum_block` frames and activates it.
unsafe fn prepared_processor(component: &ComPtr<IComponent>, maximum_block: int32) -> ComPtr<IAudioProcessor> {
    let processor = component.cast::<IAudioProcessor>().unwrap();
    let mut setup = ProcessSetup {
        processMode: 0,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
        maxSamplesPerBlock: maximum_block,
        sampleRate: 48_000.0,
    };
    assert_eq!(processor.setupProcessing(&mut setup), kResultOk);
    assert_eq!(component.setActive(1), kResultOk);
    processor
}

/// One process call with separate stereo input and output buffers (the main bus only).
unsafe fn run_host_block(
    processor: &ComPtr<IAudioProcessor>,
    input: [&mut [f32]; 2],
    output: [&mut [f32]; 2],
    host_block_context: HostBlockContext<'_>,
) -> tresult {
    let frame_count = output[0].len();
    let [input_left, input_right] = input;
    let [output_left, output_right] = output;
    let mut input_channels = [input_left.as_mut_ptr(), input_right.as_mut_ptr()];
    let mut output_channels = [output_left.as_mut_ptr(), output_right.as_mut_ptr()];
    let mut inputs: [AudioBusBuffers; 1] = std::mem::zeroed();
    inputs[0].numChannels = 2;
    inputs[0].__field0.channelBuffers32 = input_channels.as_mut_ptr();
    let mut outputs: [AudioBusBuffers; 1] = std::mem::zeroed();
    outputs[0].numChannels = 2;
    outputs[0].__field0.channelBuffers32 = output_channels.as_mut_ptr();
    let mut data: ProcessData = std::mem::zeroed();
    data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
    data.numSamples = frame_count as int32;
    data.numInputs = 1;
    data.numOutputs = 1;
    data.inputs = inputs.as_mut_ptr();
    data.outputs = outputs.as_mut_ptr();
    data.inputParameterChanges = host_block_context.parameter_changes.map_or(std::ptr::null_mut(), |changes| changes.as_ptr());
    data.inputEvents = host_block_context.input_events.map_or(std::ptr::null_mut(), |events| events.as_ptr());
    data.processContext = host_block_context
        .process_context
        .map_or(std::ptr::null_mut(), |context| context as *mut ProcessContext);
    processor.process(&mut data)
}

/// The first frame of each logged call, from the frame counts.
fn call_start_frames(calls: &[ProbeCall]) -> Vec<usize> {
    calls
        .iter()
        .scan(0, |next_start, call| {
            let call_start = *next_start;
            *next_start += call.frames;
            Some(call_start)
        })
        .collect()
}

/// VST3 ramp semantics, independently: linear between points, from `previous` before the first.
fn reference_ramp_at(previous_normalized_value: f64, points: &[(int32, ParamValue)], frame_index: usize) -> f64 {
    let mut ramp_start = (0usize, previous_normalized_value);
    for (point_offset, point_value) in points {
        let point_offset = *point_offset as usize;
        if frame_index < point_offset {
            let progress = (frame_index - ramp_start.0) as f64 / (point_offset - ramp_start.0) as f64;
            return ramp_start.1 + (point_value - ramp_start.1) * progress;
        }
        ramp_start = (point_offset, *point_value);
    }
    ramp_start.1
}

#[test]
fn a_ramp_across_a_long_block_arrives_as_interpolated_sub_blocks() {
    unsafe {
        let component = create_component_of::<ContractProbe>();
        let processor = prepared_processor(&component, 512);
        let controller = component.cast::<IEditController>().unwrap();
        controller.setParamNormalized(0, 0.25);
        reset_probe_log();

        // One point at the end: the value ramps from the previous 0.25 to 1.0 over the block.
        let ramp_points: &[(int32, ParamValue)] = &[(511, 1.0)];
        let parameter_changes = host_parameter_changes(&[(0, ramp_points)]);
        let (mut input_left, mut input_right) = ([0.0f32; 512], [0.0f32; 512]);
        let (mut output_left, mut output_right) = ([9.0f32; 512], [9.0f32; 512]);
        let host_block_context = HostBlockContext {
            parameter_changes: Some(&parameter_changes),
            ..NO_HOST_CONTEXT
        };
        assert_eq!(
            run_host_block(
                &processor,
                [&mut input_left, &mut input_right],
                [&mut output_left, &mut output_right],
                host_block_context
            ),
            kResultOk
        );

        let calls = probe_calls();
        assert_eq!(calls.len(), 512 / AUTOMATION_SUB_BLOCK_FRAMES);
        for (call, call_start) in calls.iter().zip(call_start_frames(&calls)) {
            assert_eq!(call.frames, AUTOMATION_SUB_BLOCK_FRAMES);
            let expected_ramp = reference_ramp_at(0.25, ramp_points, call_start) as f32;
            assert!(
                (call.ramp - expected_ramp).abs() < 1e-6,
                "sub-block at {call_start}: {} vs {expected_ramp}",
                call.ramp
            );
            assert_eq!(call.steady, 0.5, "parameters without automation hold still");
            // The audio is the staircase: each sub-block rendered on its own value.
            assert!(output_left[call_start..call_start + call.frames].iter().all(|sample| *sample == call.ramp));
        }
        assert!(calls.windows(2).all(|pair| pair[1].ramp > pair[0].ramp), "stepped upwards");
        assert!(output_right.iter().all(|sample| *sample == 0.5));
        // The controller reports the queue's last point, as before.
        assert_eq!(controller.getParamNormalized(0), 1.0);
        // The meter (−4 × Ramp) keeps the largest magnitude across sub-blocks: the last, most negative one.
        let deepest_reading = -4.0 * calls.last().unwrap().ramp;
        let expected_meter_normalized = PROBE_PARAMETERS[PROBE_SIGNED_METER].normalized_from_plain(f64::from(deepest_reading));
        assert!((controller.getParamNormalized(2) - expected_meter_normalized).abs() < 1e-6);

        // The next block ramps from where this one ended; points mid-block become exact split points.
        reset_probe_log();
        let shaped_points: &[(int32, ParamValue)] = &[(80, 0.0), (300, 0.5)];
        let parameter_changes = host_parameter_changes(&[(0, shaped_points)]);
        let host_block_context = HostBlockContext {
            parameter_changes: Some(&parameter_changes),
            ..NO_HOST_CONTEXT
        };
        run_host_block(
            &processor,
            [&mut input_left, &mut input_right],
            [&mut output_left, &mut output_right],
            host_block_context,
        );
        let calls = probe_calls();
        let call_starts = call_start_frames(&calls);
        assert!(call_starts.contains(&80) && call_starts.contains(&300), "split on the points: {call_starts:?}");
        assert!(calls.iter().all(|call| call.frames <= AUTOMATION_SUB_BLOCK_FRAMES));
        assert_eq!(calls.iter().map(|call| call.frames).sum::<usize>(), 512);
        for (call, call_start) in calls.iter().zip(&call_starts) {
            let expected_ramp = reference_ramp_at(1.0, shaped_points, *call_start) as f32;
            assert!(
                (call.ramp - expected_ramp).abs() < 1e-6,
                "sub-block at {call_start}: {} vs {expected_ramp}",
                call.ramp
            );
        }
        assert_eq!(calls[call_starts.iter().position(|start| *start == 80).unwrap()].ramp, 0.0);
        assert_eq!(calls[call_starts.iter().position(|start| *start == 300).unwrap()].ramp, 0.5);
        assert_eq!(controller.getParamNormalized(0), 0.5);
    }
}

#[test]
fn a_block_without_automation_is_one_call() {
    unsafe {
        let component = create_component_of::<ContractProbe>();
        let processor = prepared_processor(&component, 2_048);
        let controller = component.cast::<IEditController>().unwrap();
        let (mut input_left, mut input_right) = (vec![0.0f32; 2_048], vec![0.0f32; 2_048]);
        let (mut output_left, mut output_right) = (vec![0.0f32; 2_048], vec![0.0f32; 2_048]);

        // No IParameterChanges at all, an empty one, and one that only names a read-only meter.
        let empty_changes = host_parameter_changes(&[]);
        let meter_only_changes = host_parameter_changes(&[(2, &[(0, 1.0), (100, 0.0)])]);
        for parameter_changes in [None, Some(&empty_changes), Some(&meter_only_changes)] {
            reset_probe_log();
            let host_block_context = HostBlockContext {
                parameter_changes,
                ..NO_HOST_CONTEXT
            };
            run_host_block(
                &processor,
                [&mut input_left, &mut input_right],
                [&mut output_left, &mut output_right],
                host_block_context,
            );
            let calls = probe_calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].frames, 2_048);
            assert_eq!(calls[0].ramp, 0.0);
            // One call, one meter reading: −4 × 0 = 0.
            assert!((controller.getParamNormalized(2) - 0.5).abs() < 1e-9);
        }
    }
}

#[test]
fn note_events_land_on_their_absolute_frame_after_splitting() {
    unsafe {
        let component = create_component_of::<ContractProbe>();
        let processor = prepared_processor(&component, 512);
        reset_probe_log();
        let parameter_changes = host_parameter_changes(&[(0, &[(0, 0.0), (511, 1.0)])]);
        // Out of order on purpose (the substrate sorts), on and either side of sub-block edges.
        let host_offsets: [(int32, i16); 7] = [(100, 64), (0, 60), (31, 61), (32, 62), (33, 63), (480, 65), (511, 66)];
        let input_events = host_note_ons(&host_offsets);
        let (mut input_left, mut input_right) = ([0.0f32; 512], [0.0f32; 512]);
        let (mut output_left, mut output_right) = ([0.0f32; 512], [0.0f32; 512]);
        let host_block_context = HostBlockContext {
            parameter_changes: Some(&parameter_changes),
            input_events: Some(&input_events),
            process_context: None,
        };
        run_host_block(
            &processor,
            [&mut input_left, &mut input_right],
            [&mut output_left, &mut output_right],
            host_block_context,
        );

        let calls = probe_calls();
        assert!(calls.len() > 1, "the block was split");
        let call_starts = call_start_frames(&calls);
        let mut delivered: Vec<(usize, u8)> = probe_events()
            .iter()
            .map(|probe_event| {
                assert!(probe_event.sample_offset < calls[probe_event.call_index].frames, "offset inside its sub-block");
                (call_starts[probe_event.call_index] + probe_event.sample_offset, probe_event.pitch)
            })
            .collect();
        delivered.sort();
        let mut expected: Vec<(usize, u8)> = host_offsets.iter().map(|(offset, pitch)| (*offset as usize, *pitch as u8)).collect();
        expected.sort();
        assert_eq!(delivered, expected);
    }
}

#[test]
fn transport_advances_across_sub_blocks_and_crosses_the_bar_line() {
    unsafe {
        let component = create_component_of::<ContractProbe>();
        let processor = prepared_processor(&component, 2_048);
        reset_probe_log();
        // 120 BPM at 48 kHz: 24 000 frames per quarter note. 3/4 from 0; the bar line at 3.0 is
        // 0.04 quarter notes (960 frames, a sub-block edge) after the block starts.
        let mut process_context = host_process_context(120.0, 2.96, 0.0, (3, 4));
        let parameter_changes = host_parameter_changes(&[(0, &[(0, 0.0), (2_047, 1.0)])]);
        let (mut input_left, mut input_right) = (vec![0.0f32; 2_048], vec![0.0f32; 2_048]);
        let (mut output_left, mut output_right) = (vec![0.0f32; 2_048], vec![0.0f32; 2_048]);
        let host_block_context = HostBlockContext {
            parameter_changes: Some(&parameter_changes),
            input_events: None,
            process_context: Some(&mut process_context),
        };
        run_host_block(
            &processor,
            [&mut input_left, &mut input_right],
            [&mut output_left, &mut output_right],
            host_block_context,
        );

        let calls = probe_calls();
        assert_eq!(calls.len(), 2_048 / AUTOMATION_SUB_BLOCK_FRAMES);
        let mut bar_line_seen = false;
        for (call, call_start) in calls.iter().zip(call_start_frames(&calls)) {
            let transport = call.transport;
            let expected_position = 2.96 + call_start as f64 / 24_000.0;
            let position = transport.project_quarter_notes.expect("position known");
            assert!(
                (position - expected_position).abs() < 1e-12,
                "at {call_start}: {position} vs {expected_position}"
            );
            let expected_bar_start = if call_start >= 960 { 3.0 } else { 0.0 };
            assert_eq!(transport.bar_start_quarter_notes, Some(expected_bar_start), "bar at frame {call_start}");
            assert_eq!(transport.tempo_bpm, Some(120.0));
            assert_eq!(transport.time_signature, Some((3, 4)));
            assert!(transport.playing);
            bar_line_seen |= call_start == 960;
        }
        assert!(bar_line_seen);
        // The first sub-block is told exactly what the host said.
        assert_eq!(calls[0].transport.project_quarter_notes, Some(2.96));
    }
}

#[test]
fn the_audio_path_does_not_allocate() {
    unsafe {
        let component = create_component_of::<ContractProbe>();
        let processor = prepared_processor(&component, 2_048);
        let parameter_changes = host_parameter_changes(&[(0, &[(0, 0.0), (700, 1.0), (2_047, 0.0)]), (1, &[(1_000, 0.0)])]);
        let input_events = host_note_ons(&[(5, 60), (1_500, 62), (40, 61)]);
        let mut process_context = host_process_context(128.0, 7.5, 6.0, (7, 8));
        let (mut input_left, mut input_right) = (vec![0.1f32; 2_048], vec![0.1f32; 2_048]);
        let (mut output_left, mut output_right) = (vec![0.0f32; 2_048], vec![0.0f32; 2_048]);

        // A busy automated block, then a plain one, each measured from the host's side of the call.
        reset_probe_log();
        let allocations_before = allocations_on_this_thread();
        let host_block_context = HostBlockContext {
            parameter_changes: Some(&parameter_changes),
            input_events: Some(&input_events),
            process_context: Some(&mut process_context),
        };
        let result = run_host_block(
            &processor,
            [&mut input_left, &mut input_right],
            [&mut output_left, &mut output_right],
            host_block_context,
        );
        let automated_allocations = allocations_on_this_thread() - allocations_before;
        assert_eq!(result, kResultOk);
        assert!(probe_calls().len() > 60, "the automated block was split");
        assert_eq!(automated_allocations, 0, "allocations while processing an automated block");

        let allocations_before = allocations_on_this_thread();
        run_host_block(
            &processor,
            [&mut input_left, &mut input_right],
            [&mut output_left, &mut output_right],
            NO_HOST_CONTEXT,
        );
        assert_eq!(
            allocations_on_this_thread() - allocations_before,
            0,
            "allocations while processing a plain block"
        );

        // The auditor itself works: a plain Vec allocation is counted.
        let allocations_before = allocations_on_this_thread();
        let witness = std::hint::black_box(vec![1u8; 16]);
        assert!(allocations_on_this_thread() > allocations_before);
        drop(witness);
    }
}

#[test]
fn a_host_flooding_one_queue_degrades_to_fewer_points_not_allocations() {
    unsafe {
        let component = create_component_of::<ContractProbe>();
        let processor = prepared_processor(&component, 512);
        let controller = component.cast::<IEditController>().unwrap();
        // A point on every frame, ramping 0 → 1: far more than the preallocated lane holds.
        let flood: Vec<(int32, ParamValue)> = (0..512).map(|frame_index| (frame_index, f64::from(frame_index) / 511.0)).collect();
        let parameter_changes = host_parameter_changes(&[(0, &flood)]);
        let (mut input_left, mut input_right) = ([0.0f32; 512], [0.0f32; 512]);
        let (mut output_left, mut output_right) = ([0.0f32; 512], [0.0f32; 512]);
        reset_probe_log();
        let allocations_before = allocations_on_this_thread();
        let host_block_context = HostBlockContext {
            parameter_changes: Some(&parameter_changes),
            ..NO_HOST_CONTEXT
        };
        run_host_block(
            &processor,
            [&mut input_left, &mut input_right],
            [&mut output_left, &mut output_right],
            host_block_context,
        );
        assert_eq!(allocations_on_this_thread() - allocations_before, 0);
        let calls = probe_calls();
        for (call, call_start) in calls.iter().zip(call_start_frames(&calls)) {
            // Thinned points still lie on the same straight line, so the values stay exact.
            assert!((call.ramp - call_start as f32 / 511.0).abs() < 1e-6, "at {call_start}: {}", call.ramp);
        }
        assert_eq!(controller.getParamNormalized(0), 1.0, "the true last point always survives thinning");
    }
}

// ---------------------------------------------------------------------------------------------
// Executive Override: its tempo pump and MIDI trigger must not notice the splitting.
// ---------------------------------------------------------------------------------------------

const EXECUTIVE_TEMPO_BPM: f64 = 137.0;
const EXECUTIVE_START_QUARTER_NOTES: f64 = 2.8;
const EXECUTIVE_BAR_QUARTER_NOTES: f64 = 3.0;
const EXECUTIVE_TOTAL_FRAMES: usize = 8_192;
const EXECUTIVE_LONG_BLOCK: usize = 2_048;

struct ExecutiveRendering {
    output_left: Vec<f32>,
    /// The Gain Reduction meter (normalized) after each long block's worth of frames.
    gain_reduction_per_long_block: Vec<f64>,
}

/// Renders Executive Override in `host_block_frames` blocks with a host-correct transport (3/4 at
/// 137 BPM from 2.8 quarter notes, so a bar line falls mid-render). With `automate_mix`, each
/// block carries a constant Mix automation queue, which changes nothing but forces the split.
unsafe fn render_executive_override(mode: f64, rate: f64, host_block_frames: usize, automate_mix: bool, note_on_frames: &[usize]) -> ExecutiveRendering {
    use executive_override::{ExecutiveOverride, GAIN_REDUCTION_METER, MIX, MODE, RATE};
    let component = create_component_of::<ExecutiveOverride>();
    let processor = prepared_processor(&component, EXECUTIVE_LONG_BLOCK as int32);
    let controller = component.cast::<IEditController>().unwrap();
    let parameters = ExecutiveOverride::description().parameters;
    let identifier = |parameter_index: usize| parameters[parameter_index].identifier;
    controller.setParamNormalized(identifier(MODE), controller.plainParamToNormalized(identifier(MODE), mode));
    controller.setParamNormalized(identifier(RATE), controller.plainParamToNormalized(identifier(RATE), rate));
    let mix_normalized = controller.getParamNormalized(identifier(MIX));

    let mut rendering = ExecutiveRendering {
        output_left: Vec::with_capacity(EXECUTIVE_TOTAL_FRAMES),
        gain_reduction_per_long_block: Vec::new(),
    };
    let mut deepest_in_long_block = 0.0f64;
    for block_start in (0..EXECUTIVE_TOTAL_FRAMES).step_by(host_block_frames) {
        // What a host says at this block's first frame.
        let project_quarter_notes = EXECUTIVE_START_QUARTER_NOTES + block_start as f64 * EXECUTIVE_TEMPO_BPM / 60.0 / 48_000.0;
        let bar_start_quarter_notes = (project_quarter_notes / EXECUTIVE_BAR_QUARTER_NOTES).floor() * EXECUTIVE_BAR_QUARTER_NOTES;
        let mut process_context = host_process_context(EXECUTIVE_TEMPO_BPM, project_quarter_notes, bar_start_quarter_notes, (3, 4));
        let constant_mix: [(int32, ParamValue); 2] = [(0, mix_normalized), (host_block_frames as int32 - 1, mix_normalized)];
        let parameter_changes = host_parameter_changes(&[(identifier(MIX), &constant_mix)]);
        let block_note_ons: Vec<(int32, i16)> = note_on_frames
            .iter()
            .filter(|frame| (block_start..block_start + host_block_frames).contains(frame))
            .map(|frame| ((frame - block_start) as int32, 36))
            .collect();
        let input_events = host_note_ons(&block_note_ons);
        let (mut input_left, mut input_right) = (vec![0.5f32; host_block_frames], vec![0.5f32; host_block_frames]);
        let (mut output_left, mut output_right) = (vec![0.0f32; host_block_frames], vec![0.0f32; host_block_frames]);
        let host_block_context = HostBlockContext {
            parameter_changes: automate_mix.then_some(&parameter_changes),
            input_events: Some(&input_events),
            process_context: Some(&mut process_context),
        };
        assert_eq!(
            run_host_block(
                &processor,
                [&mut input_left, &mut input_right],
                [&mut output_left, &mut output_right],
                host_block_context
            ),
            kResultOk
        );
        rendering.output_left.extend_from_slice(&output_left);
        deepest_in_long_block = deepest_in_long_block.max(controller.getParamNormalized(identifier(GAIN_REDUCTION_METER)));
        if (block_start + host_block_frames) % EXECUTIVE_LONG_BLOCK == 0 {
            rendering.gain_reduction_per_long_block.push(deepest_in_long_block);
            deepest_in_long_block = 0.0;
        }
    }
    rendering
}

fn assert_renderings_match(split: &ExecutiveRendering, short_blocks: &ExecutiveRendering, what: &str) {
    let largest_difference = split
        .output_left
        .iter()
        .zip(&short_blocks.output_left)
        .map(|(split_sample, short_sample)| (split_sample - short_sample).abs())
        .fold(0.0f32, f32::max);
    assert!(
        largest_difference < 1e-5,
        "{what}: split long blocks differ from short host blocks by {largest_difference}"
    );
    for (split_meter, short_meter) in split.gain_reduction_per_long_block.iter().zip(&short_blocks.gain_reduction_per_long_block) {
        assert!((split_meter - short_meter).abs() < 1e-6, "{what}: meter {split_meter} vs {short_meter}");
    }
}

#[test]
fn executive_override_tempo_pump_survives_splitting_across_a_bar_line() {
    unsafe {
        // Half-note pump in 3/4: only a transport that moves to the new bar ducks on its downbeat.
        let split = render_executive_override(executive_override::MODE_TEMPO_PUMP.into(), 2.0, EXECUTIVE_LONG_BLOCK, true, &[]);
        let short_blocks = render_executive_override(executive_override::MODE_TEMPO_PUMP.into(), 2.0, AUTOMATION_SUB_BLOCK_FRAMES, false, &[]);
        assert_renderings_match(&split, &short_blocks, "tempo pump");

        // The downbeat at 3.0 quarter notes (frame ≈ 4 204) really ducks, and the meter saw it.
        let downbeat_frame = ((EXECUTIVE_BAR_QUARTER_NOTES - EXECUTIVE_START_QUARTER_NOTES) * 60.0 / EXECUTIVE_TEMPO_BPM * 48_000.0).ceil() as usize;
        let ducked_decibels = 20.0 * (split.output_left[downbeat_frame + 200] / 0.5).log10();
        assert!(ducked_decibels < -11.0, "downbeat duck: {ducked_decibels} dB");
        assert!(split.gain_reduction_per_long_block[2] > 0.2);
    }
}

#[test]
fn executive_override_midi_trigger_survives_splitting() {
    unsafe {
        let note_on_frames = [31, 32, 1_000, 2_047, 2_048, 5_000];
        let split = render_executive_override(executive_override::MODE_MIDI_TRIGGER.into(), 0.0, EXECUTIVE_LONG_BLOCK, true, &note_on_frames);
        let short_blocks = render_executive_override(
            executive_override::MODE_MIDI_TRIGGER.into(),
            0.0,
            AUTOMATION_SUB_BLOCK_FRAMES,
            false,
            &note_on_frames,
        );
        assert_renderings_match(&split, &short_blocks, "MIDI trigger");
        let ducked_decibels = 20.0 * (split.output_left[5_200] / 0.5).log10();
        assert!(ducked_decibels < -11.0, "note-on duck: {ducked_decibels} dB");
    }
}

/// Asks the controller which parameter a MIDI controller drives, the way a host does.
unsafe fn midi_assignment(midi_mapping: &ComPtr<IMidiMapping>, bus_index: int32, channel: int16, controller_number: CtrlNumber) -> Option<ParamID> {
    let mut parameter_identifier: ParamID = ParamID::MAX;
    match midi_mapping.getMidiControllerAssignment(bus_index, channel, controller_number, &mut parameter_identifier) {
        result if result == kResultTrue => Some(parameter_identifier),
        result => {
            assert_eq!(result, kResultFalse, "unmapped controllers answer kResultFalse");
            assert_eq!(parameter_identifier, ParamID::MAX, "an unmapped answer must not write the id");
            None
        }
    }
}

#[test]
fn midi_controllers_map_to_their_declared_parameters() {
    use ControllerNumbers_::*;
    unsafe {
        let component = create_component_of::<ContractSynth>();
        // Hosts ask the edit controller, which is the same object.
        let controller = component.cast::<IEditController>().expect("IEditController");
        let midi_mapping = controller.cast::<IMidiMapping>().expect("the controller exposes IMidiMapping");
        assert_eq!(
            component.getBusCount(MediaTypes_::kEvent as MediaType, BusDirections_::kInput as BusDirection),
            1
        );

        for channel in [0, 9, 15] {
            assert_eq!(
                midi_assignment(&midi_mapping, 0, channel, kPitchBend as CtrlNumber),
                Some(1),
                "pitch bend, channel {channel}"
            );
            assert_eq!(
                midi_assignment(&midi_mapping, 0, channel, kCtrlModWheel as CtrlNumber),
                Some(2),
                "mod wheel, channel {channel}"
            );
            assert_eq!(midi_assignment(&midi_mapping, 0, channel, 74), Some(3), "CC 74, channel {channel}");
        }
        // Undeclared controllers, a meter target, and a bus that does not exist all stay unmapped.
        assert_eq!(midi_assignment(&midi_mapping, 0, 0, kCtrlVolume as CtrlNumber), None);
        assert_eq!(midi_assignment(&midi_mapping, 0, 0, kCtrlExpression as CtrlNumber), None);
        assert_eq!(
            midi_assignment(&midi_mapping, 0, 0, kAfterTouch as CtrlNumber),
            None,
            "never routes onto a read-only meter"
        );
        assert_eq!(midi_assignment(&midi_mapping, 1, 0, kPitchBend as CtrlNumber), None);
        assert_eq!(midi_assignment(&midi_mapping, -1, 0, kPitchBend as CtrlNumber), None);

        // A mapped controller with nowhere to write the answer is refused, not dereferenced.
        assert_eq!(
            midi_mapping.getMidiControllerAssignment(0, 0, kPitchBend as CtrlNumber, std::ptr::null_mut()),
            kInvalidArgument
        );

        // The mapped parameters are ordinary automatable knobs the host can see.
        let mut info: ParameterInfo = std::mem::zeroed();
        assert_eq!(controller.getParameterInfo(1, &mut info), kResultOk);
        assert_eq!(info.id, 1);
        assert_ne!(info.flags & ParameterInfo_::ParameterFlags_::kCanAutomate as int32, 0);
        assert!((info.defaultNormalizedValue - 0.5).abs() < 1e-12, "pitch bend rests at the centre");
    }
}

#[test]
fn plugins_without_mappings_or_notes_map_nothing() {
    use ControllerNumbers_::*;
    unsafe {
        let component = create_component();
        let midi_mapping = component.cast::<IMidiMapping>().expect("every substrate class answers IMidiMapping");
        for controller_number in [kPitchBend, kCtrlModWheel, kAfterTouch, 0, 7, 74, 127] {
            for bus_index in [0, 1] {
                assert_eq!(midi_assignment(&midi_mapping, bus_index, 0, controller_number as CtrlNumber), None);
            }
        }
    }
}

#[test]
fn a_chunk_saved_before_the_controllers_existed_still_loads() {
    unsafe {
        // Version 1 of Contract Synth saved Level alone.
        let legacy_chunk = synergy_substrate::state::encode_state(&SYNTH_PARAMETERS[..1], &[0.8]);
        let stream = ComWrapper::new(MemoryStream::default());
        stream.bytes.borrow_mut().extend_from_slice(&legacy_chunk);

        let component = create_component_of::<ContractSynth>();
        let controller = component.cast::<IEditController>().unwrap();
        controller.setParamNormalized(2, 0.75);
        assert_eq!(component.setState(stream.to_com_ptr::<IBStream>().unwrap().as_ptr()), kResultOk);
        assert!((controller.getParamNormalized(0) - 0.8).abs() < 1e-12, "the saved Level survives");
        assert!((controller.getParamNormalized(1) - 0.5).abs() < 1e-12, "Pitch Bend keeps its centre default");
        assert!((controller.getParamNormalized(3) - 1.0).abs() < 1e-12, "Brightness keeps its default");
        assert!(
            (controller.getParamNormalized(2) - 0.75).abs() < 1e-12,
            "parameters the old chunk never mentioned are left alone"
        );
    }
}
