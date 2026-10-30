//! The single-component VST3 object: one COM class that is the processor
//! (IComponent + IAudioProcessor) and its own edit controller (IEditController),
//! so parameters live in exactly one place and never need syncing between halves.
//!
//! Every `unsafe fn` here is a VST3 interface method the host calls with raw
//! pointers. The rules each one relies on are noted where pointers are touched.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComRef};

use crate::state::{decode_state, encode_state};
use crate::strings::{copy_to_char16, read_char16};
use crate::{NoteEvent, NoteEventKind, PluginKind, ProcessBlock, SynergyPlugin, TransportSnapshot};

/// Upper bound for state chunks we are willing to read from a host stream.
const MAXIMUM_STATE_BYTES: usize = 1 << 20;
const DEFAULT_SAMPLE_RATE: f32 = 44_100.0;
const DEFAULT_MAXIMUM_BLOCK: usize = 4096;
const SIDECHAIN_BUS_NAME: &str = "Sidechain";

/// Audio-thread working memory, allocated in setupProcessing and reused for every block.
struct ProcessingState<P: SynergyPlugin> {
    plugin: P,
    sample_rate: f32,
    input_left: Vec<f32>,
    input_right: Vec<f32>,
    sidechain_left: Vec<f32>,
    sidechain_right: Vec<f32>,
    plain_parameters: Vec<f32>,
    meters: Vec<Option<f32>>,
    events: Vec<NoteEvent>,
}

impl<P: SynergyPlugin> ProcessingState<P> {
    fn new(sample_rate: f32, maximum_block: usize) -> Self {
        let parameter_count = P::description().parameters.len();
        Self {
            plugin: P::new(sample_rate),
            sample_rate,
            input_left: vec![0.0; maximum_block],
            input_right: vec![0.0; maximum_block],
            sidechain_left: vec![0.0; maximum_block],
            sidechain_right: vec![0.0; maximum_block],
            plain_parameters: vec![0.0; parameter_count],
            meters: vec![None; parameter_count],
            events: Vec::with_capacity(1024),
        }
    }

    fn ensure_capacity(&mut self, frame_count: usize) {
        if self.input_left.len() < frame_count {
            // Only when a host exceeds the block size it announced; correctness over purity.
            for buffer in [&mut self.input_left, &mut self.input_right, &mut self.sidechain_left, &mut self.sidechain_right] {
                buffer.resize(frame_count, 0.0);
            }
        }
    }
}

/// The COM object for plugin type `P`.
pub struct PluginComponent<P: SynergyPlugin> {
    /// Normalized parameter values, shared by the controller (UI thread) and the processor (audio thread).
    normalized_values: Box<[AtomicU64]>,
    processing: Mutex<ProcessingState<P>>,
    sidechain_bus_active: AtomicBool,
    _plugin: PhantomData<P>,
}

impl<P: SynergyPlugin> Class for PluginComponent<P> {
    type Interfaces = (IComponent, IAudioProcessor, IEditController, IProcessContextRequirements);
}

impl<P: SynergyPlugin> Default for PluginComponent<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: SynergyPlugin> PluginComponent<P> {
    pub fn new() -> Self {
        let description = P::description();
        Self {
            normalized_values: description
                .parameters
                .iter()
                .map(|parameter| AtomicU64::new(parameter.default_normalized().to_bits()))
                .collect(),
            processing: Mutex::new(ProcessingState::new(DEFAULT_SAMPLE_RATE, DEFAULT_MAXIMUM_BLOCK)),
            sidechain_bus_active: AtomicBool::new(false),
            _plugin: PhantomData,
        }
    }

    fn normalized_value(&self, parameter_index: usize) -> f64 {
        f64::from_bits(self.normalized_values[parameter_index].load(Ordering::Relaxed))
    }

    fn store_normalized_value(&self, parameter_index: usize, normalized_value: f64) {
        if normalized_value.is_finite() {
            self.normalized_values[parameter_index].store(normalized_value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    fn snapshot_normalized_values(&self) -> Vec<f64> {
        (0..self.normalized_values.len())
            .map(|parameter_index| self.normalized_value(parameter_index))
            .collect()
    }

    fn apply_decoded_state(&self, decoded_values: &[(u32, f64)]) {
        let description = P::description();
        for (identifier, normalized_value) in decoded_values {
            if let Some(parameter_index) = description.parameter_index(*identifier) {
                if !description.parameters[parameter_index].read_only {
                    self.store_normalized_value(parameter_index, *normalized_value);
                }
            }
        }
    }

    fn audio_input_bus_count() -> i32 {
        let description = P::description();
        match description.kind {
            PluginKind::Effect => 1 + i32::from(description.sidechain_input),
            PluginKind::Instrument => 0,
        }
    }

    /// Reads a whole IBStream into memory.
    ///
    /// # Safety
    /// `stream` must be null or a valid IBStream for the duration of the call.
    unsafe fn read_stream(stream: *mut IBStream) -> Option<Vec<u8>> {
        let stream = ComRef::from_raw(stream)?;
        let mut collected_bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut bytes_read: int32 = 0;
            let read_result = stream.read(chunk.as_mut_ptr().cast(), chunk.len() as int32, &mut bytes_read);
            if read_result != kResultOk || bytes_read <= 0 {
                break;
            }
            collected_bytes.extend_from_slice(&chunk[..bytes_read as usize]);
            if collected_bytes.len() > MAXIMUM_STATE_BYTES {
                return None;
            }
        }
        Some(collected_bytes)
    }

    /// # Safety
    /// `stream` must be null or a valid IBStream for the duration of the call.
    unsafe fn write_stream(stream: *mut IBStream, bytes: &[u8]) -> tresult {
        let Some(stream) = ComRef::from_raw(stream) else {
            return kInvalidArgument;
        };
        let mut bytes_written: int32 = 0;
        // VST3 streams take a mutable pointer but only read from it during write().
        stream.write(bytes.as_ptr() as *mut _, bytes.len() as int32, &mut bytes_written)
    }
}

impl<P: SynergyPlugin> IPluginBaseTrait for PluginComponent<P> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl<P: SynergyPlugin> IComponentTrait for PluginComponent<P> {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        // Single component: hosts query IEditController on this object instead.
        kNotImplemented
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media_type: MediaType, direction: BusDirection) -> int32 {
        let description = P::description();
        match (media_type as MediaTypes, direction as BusDirections) {
            (MediaTypes_::kAudio, BusDirections_::kInput) => Self::audio_input_bus_count(),
            (MediaTypes_::kAudio, BusDirections_::kOutput) => 1,
            (MediaTypes_::kEvent, BusDirections_::kInput) => i32::from(description.has_note_input()),
            _ => 0,
        }
    }

    unsafe fn getBusInfo(&self, media_type: MediaType, direction: BusDirection, index: int32, bus: *mut BusInfo) -> tresult {
        if bus.is_null() || index < 0 || index >= self.getBusCount(media_type, direction) {
            return kInvalidArgument;
        }
        // SAFETY: non-null and owned by the host for the duration of the call.
        let bus = &mut *bus;
        bus.mediaType = media_type;
        bus.direction = direction;
        bus.flags = BusInfo_::BusFlags_::kDefaultActive as uint32;
        bus.busType = BusTypes_::kMain as BusType;
        match (media_type as MediaTypes, direction as BusDirections, index) {
            (MediaTypes_::kAudio, BusDirections_::kInput, 0) => {
                bus.channelCount = 2;
                copy_to_char16("Input", &mut bus.name);
            }
            (MediaTypes_::kAudio, BusDirections_::kInput, _) => {
                // The sidechain: an auxiliary stereo bus the host may leave switched off.
                bus.channelCount = 2;
                bus.busType = BusTypes_::kAux as BusType;
                bus.flags = 0;
                copy_to_char16(SIDECHAIN_BUS_NAME, &mut bus.name);
            }
            (MediaTypes_::kAudio, _, _) => {
                bus.channelCount = 2;
                copy_to_char16("Output", &mut bus.name);
            }
            _ => {
                bus.channelCount = 16;
                copy_to_char16("Note Input", &mut bus.name);
            }
        }
        kResultOk
    }

    unsafe fn getRoutingInfo(&self, _input_info: *mut RoutingInfo, _output_info: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(&self, media_type: MediaType, direction: BusDirection, index: int32, state: TBool) -> tresult {
        if index < 0 || index >= self.getBusCount(media_type, direction) {
            return kInvalidArgument;
        }
        if media_type as MediaTypes == MediaTypes_::kAudio && direction as BusDirections == BusDirections_::kInput && index == 1 {
            self.sidechain_bus_active.store(state != 0, Ordering::Relaxed);
        }
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state != 0 {
            if let Ok(mut processing) = self.processing.lock() {
                processing.plugin.reset();
            }
        }
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        match Self::read_stream(state).as_deref().and_then(decode_state) {
            Some(decoded_values) => {
                self.apply_decoded_state(&decoded_values);
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let encoded_state = encode_state(P::description().parameters, &self.snapshot_normalized_values());
        Self::write_stream(state, &encoded_state)
    }
}

impl<P: SynergyPlugin> IAudioProcessorTrait for PluginComponent<P> {
    unsafe fn setBusArrangements(&self, inputs: *mut SpeakerArrangement, input_count: int32, outputs: *mut SpeakerArrangement, output_count: int32) -> tresult {
        if input_count != Self::audio_input_bus_count() || output_count != 1 || outputs.is_null() {
            return kResultFalse;
        }
        // SAFETY: the host passes arrays of the counts it gave us.
        let output_arrangement = *outputs;
        let input_arrangements = if input_count > 0 && !inputs.is_null() {
            std::slice::from_raw_parts(inputs, input_count as usize)
        } else {
            &[]
        };
        let main_input_is_stereo = input_arrangements.first().is_none_or(|arrangement| *arrangement == SpeakerArr::kStereo);
        // The sidechain accepts stereo or mono; mono is folded to both channels in process().
        let sidechain_is_supported = input_arrangements
            .get(1)
            .is_none_or(|arrangement| *arrangement == SpeakerArr::kStereo || *arrangement == SpeakerArr::kMono);
        if output_arrangement == SpeakerArr::kStereo && main_input_is_stereo && sidechain_is_supported {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn getBusArrangement(&self, direction: BusDirection, index: int32, arrangement: *mut SpeakerArrangement) -> tresult {
        if arrangement.is_null() || index < 0 || index >= self.getBusCount(MediaTypes_::kAudio as MediaType, direction) {
            return kInvalidArgument;
        }
        *arrangement = SpeakerArr::kStereo;
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, symbolic_sample_size: int32) -> tresult {
        if symbolic_sample_size as SymbolicSampleSizes == SymbolicSampleSizes_::kSample32 {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getLatencySamples(&self) -> uint32 {
        0
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let Some(setup) = setup.as_ref() else {
            return kInvalidArgument;
        };
        let sample_rate = if setup.sampleRate.is_finite() && setup.sampleRate > 0.0 {
            setup.sampleRate as f32
        } else {
            DEFAULT_SAMPLE_RATE
        };
        let maximum_block = usize::try_from(setup.maxSamplesPerBlock)
            .ok()
            .filter(|block| *block > 0)
            .unwrap_or(DEFAULT_MAXIMUM_BLOCK);
        match self.processing.lock() {
            Ok(mut processing) => {
                *processing = ProcessingState::new(sample_rate, maximum_block);
                kResultOk
            }
            Err(_) => kInternalError,
        }
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let Some(data) = data.as_mut() else {
            return kInvalidArgument;
        };
        let description = P::description();

        // Automation in: apply the last point of each queue (blocks are short; slaw sends 64-frame chunks).
        if let Some(parameter_changes) = ComRef::from_raw(data.inputParameterChanges) {
            for queue_index in 0..parameter_changes.getParameterCount() {
                let Some(value_queue) = ComRef::from_raw(parameter_changes.getParameterData(queue_index)) else {
                    continue;
                };
                let point_count = value_queue.getPointCount();
                let Some(parameter_index) = description.parameter_index(value_queue.getParameterId()) else {
                    continue;
                };
                if point_count <= 0 || description.parameters[parameter_index].read_only {
                    continue;
                }
                let mut sample_offset: int32 = 0;
                let mut normalized_value: ParamValue = 0.0;
                if value_queue.getPoint(point_count - 1, &mut sample_offset, &mut normalized_value) == kResultOk {
                    self.store_normalized_value(parameter_index, normalized_value);
                }
            }
        }

        let frame_count = usize::try_from(data.numSamples).unwrap_or(0);
        if frame_count == 0 || data.numOutputs < 1 || data.outputs.is_null() {
            // A parameter-flush call with no audio.
            return kResultOk;
        }
        if data.symbolicSampleSize as SymbolicSampleSizes != SymbolicSampleSizes_::kSample32 {
            return kResultFalse;
        }

        // SAFETY: the host owns `outputs[0]` with `numChannels` valid channel pointers of `numSamples` frames.
        let output_bus = &mut *data.outputs;
        let output_channel_count = usize::try_from(output_bus.numChannels).unwrap_or(0);
        let output_channels = output_bus.__field0.channelBuffers32;
        if output_channel_count == 0 || output_channels.is_null() {
            return kResultOk;
        }
        output_bus.silenceFlags = 0;

        // Never block the audio thread: if setupProcessing holds the lock, emit silence this block.
        let Ok(mut processing) = self.processing.try_lock() else {
            for channel_index in 0..output_channel_count {
                let channel_pointer = *output_channels.add(channel_index);
                if !channel_pointer.is_null() {
                    std::slice::from_raw_parts_mut(channel_pointer, frame_count).fill(0.0);
                }
            }
            return kResultOk;
        };
        processing.ensure_capacity(frame_count);
        let processing = &mut *processing;

        // Inputs are copied first: hosts may process in place (input and output share memory),
        // and Rust must never hold a shared and a mutable view of the same samples.
        let main_input_present = Self::copy_input_bus(data, 0, frame_count, &mut processing.input_left, &mut processing.input_right);
        if !main_input_present {
            processing.input_left[..frame_count].fill(0.0);
            processing.input_right[..frame_count].fill(0.0);
        }
        let sidechain_present =
            description.sidechain_input && Self::copy_input_bus(data, 1, frame_count, &mut processing.sidechain_left, &mut processing.sidechain_right);

        processing.events.clear();
        if description.has_note_input() {
            if let Some(event_list) = ComRef::from_raw(data.inputEvents) {
                for event_index in 0..event_list.getEventCount() {
                    let mut host_event: Event = std::mem::zeroed();
                    if event_list.getEvent(event_index, &mut host_event) != kResultOk {
                        continue;
                    }
                    let sample_offset = usize::try_from(host_event.sampleOffset).unwrap_or(0).min(frame_count - 1);
                    // SAFETY: the union member read matches the event type the host stamped on it.
                    let note_event = match host_event.r#type as Event_::EventTypes {
                        Event_::EventTypes_::kNoteOnEvent => {
                            let note_on = host_event.__field0.noteOn;
                            let pitch = u8::try_from(note_on.pitch.clamp(0, 127)).unwrap_or(60);
                            // VST3 sends velocity-0 note-ons as note-offs in spirit.
                            if note_on.velocity <= 0.0 {
                                NoteEventKind::Off { pitch }
                            } else {
                                NoteEventKind::On {
                                    pitch,
                                    velocity: note_on.velocity.clamp(0.0, 1.0),
                                }
                            }
                        }
                        Event_::EventTypes_::kNoteOffEvent => {
                            let note_off = host_event.__field0.noteOff;
                            NoteEventKind::Off {
                                pitch: u8::try_from(note_off.pitch.clamp(0, 127)).unwrap_or(60),
                            }
                        }
                        _ => continue,
                    };
                    processing.events.push(NoteEvent {
                        sample_offset,
                        kind: note_event,
                    });
                }
                processing.events.sort_by_key(|note_event| note_event.sample_offset);
            }
        }

        let transport = Self::read_transport(data.processContext);
        for (parameter_index, parameter) in description.parameters.iter().enumerate() {
            processing.plain_parameters[parameter_index] = parameter.plain_from_normalized(self.normalized_value(parameter_index)) as f32;
        }
        processing.meters.fill(None);

        let output_left_pointer = *output_channels;
        let output_right_pointer = if output_channel_count > 1 {
            *output_channels.add(1)
        } else {
            std::ptr::null_mut()
        };
        if output_left_pointer.is_null() {
            return kResultOk;
        }
        // SAFETY: distinct host channel buffers of `frame_count` frames; inputs were copied away above.
        let output_left = std::slice::from_raw_parts_mut(output_left_pointer, frame_count);
        let mut mono_fallback_right = [0.0f32; 0];
        let output_right: &mut [f32] = if output_right_pointer.is_null() || output_right_pointer == output_left_pointer {
            &mut mono_fallback_right
        } else {
            std::slice::from_raw_parts_mut(output_right_pointer, frame_count)
        };
        let stereo_output = !output_right.is_empty();
        if !stereo_output {
            // A mono output: render the right channel into the sidechain-right scratch and discard it.
            processing.plugin.process(ProcessBlock {
                sample_rate: processing.sample_rate,
                input_left: &processing.input_left[..frame_count],
                input_right: &processing.input_right[..frame_count],
                sidechain: sidechain_present.then_some((&processing.sidechain_left[..frame_count], &processing.sidechain_left[..frame_count])),
                output_left,
                output_right: &mut processing.sidechain_right[..frame_count],
                parameters: &processing.plain_parameters,
                meters: &mut processing.meters,
                events: &processing.events,
                transport,
            });
        } else {
            processing.plugin.process(ProcessBlock {
                sample_rate: processing.sample_rate,
                input_left: &processing.input_left[..frame_count],
                input_right: &processing.input_right[..frame_count],
                sidechain: sidechain_present.then_some((&processing.sidechain_left[..frame_count], &processing.sidechain_right[..frame_count])),
                output_left,
                output_right,
                parameters: &processing.plain_parameters,
                meters: &mut processing.meters,
                events: &processing.events,
                transport,
            });
        }

        // Meters out: store for getParamNormalized and report to the host if it listens.
        let output_parameter_changes = ComRef::from_raw(data.outputParameterChanges);
        for (parameter_index, meter_value) in processing.meters.iter().enumerate() {
            let Some(meter_plain) = meter_value else {
                continue;
            };
            let parameter = &description.parameters[parameter_index];
            let meter_normalized = parameter.normalized_from_plain(f64::from(*meter_plain));
            self.store_normalized_value(parameter_index, meter_normalized);
            if let Some(output_parameter_changes) = &output_parameter_changes {
                let mut queue_index: int32 = 0;
                if let Some(value_queue) = ComRef::from_raw(output_parameter_changes.addParameterData(&parameter.identifier, &mut queue_index)) {
                    let mut point_index: int32 = 0;
                    value_queue.addPoint(0, meter_normalized, &mut point_index);
                }
            }
        }
        kResultOk
    }

    unsafe fn getTailSamples(&self) -> uint32 {
        let sample_rate = self.processing.lock().map(|processing| processing.sample_rate).unwrap_or(DEFAULT_SAMPLE_RATE);
        (P::description().tail_seconds * f64::from(sample_rate)) as uint32
    }
}

impl<P: SynergyPlugin> PluginComponent<P> {
    /// Copies an audio input bus into scratch buffers. Returns false when the host gave us no
    /// usable buffers for it (an inactive sidechain, for one), so the caller treats it as silence.
    ///
    /// # Safety
    /// `data` must be the host's ProcessData for the current call.
    unsafe fn copy_input_bus(data: &ProcessData, bus_index: usize, frame_count: usize, left: &mut [f32], right: &mut [f32]) -> bool {
        if data.inputs.is_null() || usize::try_from(data.numInputs).unwrap_or(0) <= bus_index {
            return false;
        }
        let input_bus = &*data.inputs.add(bus_index);
        let channel_count = usize::try_from(input_bus.numChannels).unwrap_or(0);
        let channels = input_bus.__field0.channelBuffers32;
        if channel_count == 0 || channels.is_null() {
            return false;
        }
        let left_pointer = *channels;
        if left_pointer.is_null() {
            return false;
        }
        left[..frame_count].copy_from_slice(std::slice::from_raw_parts(left_pointer, frame_count));
        let right_pointer = if channel_count > 1 { *channels.add(1) } else { std::ptr::null_mut() };
        if right_pointer.is_null() {
            right[..frame_count].copy_from_slice(&left[..frame_count]);
        } else {
            right[..frame_count].copy_from_slice(std::slice::from_raw_parts(right_pointer, frame_count));
        }
        true
    }

    /// # Safety
    /// `context` must be null or the host's ProcessContext for the current call.
    unsafe fn read_transport(context: *mut ProcessContext) -> TransportSnapshot {
        let Some(context) = context.as_ref() else {
            return TransportSnapshot::default();
        };
        let state_flags = context.state;
        let has_flag = |flag: ProcessContext_::StatesAndFlags| state_flags & flag != 0;
        TransportSnapshot {
            tempo_bpm: (has_flag(ProcessContext_::StatesAndFlags_::kTempoValid) && context.tempo > 0.0).then_some(context.tempo),
            project_quarter_notes: has_flag(ProcessContext_::StatesAndFlags_::kProjectTimeMusicValid).then_some(context.projectTimeMusic),
            bar_start_quarter_notes: has_flag(ProcessContext_::StatesAndFlags_::kBarPositionValid).then_some(context.barPositionMusic),
            time_signature: (has_flag(ProcessContext_::StatesAndFlags_::kTimeSigValid) && context.timeSigNumerator > 0 && context.timeSigDenominator > 0)
                .then_some((context.timeSigNumerator, context.timeSigDenominator)),
            playing: has_flag(ProcessContext_::StatesAndFlags_::kPlaying),
        }
    }
}

impl<P: SynergyPlugin> IProcessContextRequirementsTrait for PluginComponent<P> {
    unsafe fn getProcessContextRequirements(&self) -> uint32 {
        use IProcessContextRequirements_::Flags_::*;
        (kNeedTempo | kNeedProjectTimeMusic | kNeedBarPositionMusic | kNeedTimeSignature | kNeedTransportState) as uint32
    }
}

impl<P: SynergyPlugin> IEditControllerTrait for PluginComponent<P> {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        // Same object, same values: accept the chunk anyway for hosts that send it.
        if let Some(decoded_values) = Self::read_stream(state).as_deref().and_then(decode_state) {
            self.apply_decoded_state(&decoded_values);
        }
        kResultOk
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> int32 {
        P::description().parameters.len() as int32
    }

    unsafe fn getParameterInfo(&self, parameter_index: int32, info: *mut ParameterInfo) -> tresult {
        let parameters = P::description().parameters;
        let (Some(parameter), Some(info)) = (usize::try_from(parameter_index).ok().and_then(|index| parameters.get(index)), info.as_mut()) else {
            return kInvalidArgument;
        };
        info.id = parameter.identifier;
        copy_to_char16(parameter.title, &mut info.title);
        copy_to_char16(parameter.short_title, &mut info.shortTitle);
        copy_to_char16(parameter.units, &mut info.units);
        info.stepCount = parameter.step_count();
        info.defaultNormalizedValue = parameter.default_normalized();
        info.unitId = 0;
        info.flags = if parameter.read_only {
            ParameterInfo_::ParameterFlags_::kIsReadOnly as int32
        } else if parameter.taper == crate::ParameterTaper::Stepped && !parameter.value_labels.is_empty() {
            (ParameterInfo_::ParameterFlags_::kCanAutomate | ParameterInfo_::ParameterFlags_::kIsList) as int32
        } else {
            ParameterInfo_::ParameterFlags_::kCanAutomate as int32
        };
        kResultOk
    }

    unsafe fn getParamStringByValue(&self, identifier: ParamID, normalized_value: ParamValue, string: *mut String128) -> tresult {
        let description = P::description();
        let (Some(parameter_index), Some(string)) = (description.parameter_index(identifier), string.as_mut()) else {
            return kInvalidArgument;
        };
        let parameter = &description.parameters[parameter_index];
        copy_to_char16(&parameter.format_plain(parameter.plain_from_normalized(normalized_value)), string);
        kResultOk
    }

    unsafe fn getParamValueByString(&self, identifier: ParamID, string: *mut TChar, normalized_value: *mut ParamValue) -> tresult {
        let description = P::description();
        let Some(parameter_index) = description.parameter_index(identifier) else {
            return kInvalidArgument;
        };
        let parameter = &description.parameters[parameter_index];
        match (
            read_char16(string).and_then(|typed_text| parameter.parse_plain(&typed_text)),
            normalized_value.as_mut(),
        ) {
            (Some(plain_value), Some(normalized_value)) => {
                *normalized_value = parameter.normalized_from_plain(plain_value);
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn normalizedParamToPlain(&self, identifier: ParamID, normalized_value: ParamValue) -> ParamValue {
        let description = P::description();
        description
            .parameter_index(identifier)
            .map(|parameter_index| description.parameters[parameter_index].plain_from_normalized(normalized_value))
            .unwrap_or(0.0)
    }

    unsafe fn plainParamToNormalized(&self, identifier: ParamID, plain_value: ParamValue) -> ParamValue {
        let description = P::description();
        description
            .parameter_index(identifier)
            .map(|parameter_index| description.parameters[parameter_index].normalized_from_plain(plain_value))
            .unwrap_or(0.0)
    }

    unsafe fn getParamNormalized(&self, identifier: ParamID) -> ParamValue {
        P::description()
            .parameter_index(identifier)
            .map(|parameter_index| self.normalized_value(parameter_index))
            .unwrap_or(0.0)
    }

    unsafe fn setParamNormalized(&self, identifier: ParamID, normalized_value: ParamValue) -> tresult {
        match P::description().parameter_index(identifier) {
            Some(parameter_index) => {
                self.store_normalized_value(parameter_index, normalized_value);
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        // No editor of our own, so nothing ever edits from the plugin side.
        kResultOk
    }

    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        // Hosts show a generic knob panel; slaw's is excellent and on-brand.
        std::ptr::null_mut()
    }
}
