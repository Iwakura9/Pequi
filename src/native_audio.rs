//! Small native owner for the stereo `pw_filter` graph.
//!
//! `pipewire-rs` 0.10 exposes the regular PipeWire objects but does not expose
//! a safe wrapper for the filter convenience API.  The FFI below mirrors the
//! public declarations in `/usr/include/pipewire-0.3/pipewire/filter.h`.
//! Keeping this boundary here makes the rest of the audio engine independent of
//! C layout details. The filter keeps its graph stable while control updates
//! move through a bounded lock-free queue into a preallocated stereo processor.

use crate::dsp::{FilterBank, StereoProcessor};
use crate::preset::Preset;
use crate::rt_queue::{PushResult, SpscQueue};
use anyhow::{bail, Context, Result};
use pipewire as pw;
use std::ffi::{c_char, c_void, CString};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHANNELS: usize = 2;
const FILTER_FLAGS_RT_PROCESS: u32 = 1 << 2;
const FILTER_PORT_FLAG_MAP_BUFFERS: u32 = 1;
const DEFAULT_SAMPLE_RATE_HZ: f64 = 48_000.0;

/// Number of pending control updates retained by the native audio owner.
pub const ENGINE_UPDATE_QUEUE_CAPACITY: usize = 8;

/// A validated, precomputed control update sent to the realtime owner.
#[derive(Debug, Clone, Copy)]
pub struct EngineUpdate {
    pub bank: FilterBank,
    pub revision: u64,
    pub bypassed: bool,
}

impl EngineUpdate {
    pub const fn new(bank: FilterBank, revision: u64, bypassed: bool) -> Self {
        Self {
            bank,
            revision,
            bypassed,
        }
    }
}

/// The fixed-capacity queue used for engine updates.
pub type EngineUpdateQueue = SpscQueue<EngineUpdate, ENGINE_UPDATE_QUEUE_CAPACITY>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitReceipt {
    pub revision: u64,
    pub coalesced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    StaleRevision { revision: u64, latest: u64 },
    QueueFull { revision: u64 },
}

/// A control-side producer for the native callback's bounded queue.
///
/// The queue is SPSC, so this handle is deliberately not cloneable. It can be
/// moved between control components while retaining one logical producer.
pub struct NativeEngineHandle {
    queue: Arc<EngineUpdateQueue>,
    accepted_revision: Arc<AtomicU64>,
}

impl NativeEngineHandle {
    pub fn new() -> Self {
        Self::with_revision(0)
    }

    fn with_queue(queue: Arc<EngineUpdateQueue>, revision: u64) -> Self {
        Self {
            queue,
            accepted_revision: Arc::new(AtomicU64::new(revision)),
        }
    }

    fn with_revision(revision: u64) -> Self {
        Self::with_queue(Arc::new(EngineUpdateQueue::new()), revision)
    }

    /// Submit an already-prepared bank without waiting for the callback.
    pub fn submit(&self, update: EngineUpdate) -> std::result::Result<SubmitReceipt, SubmitError> {
        let latest = self.accepted_revision.load(Ordering::Acquire);
        if update.revision <= latest {
            return Err(SubmitError::StaleRevision {
                revision: update.revision,
                latest,
            });
        }

        let result = self.queue.push(update);
        match result {
            PushResult::Queued | PushResult::Coalesced => {
                self.accepted_revision
                    .store(update.revision, Ordering::Release);
                Ok(SubmitReceipt {
                    revision: update.revision,
                    coalesced: result == PushResult::Coalesced,
                })
            }
            PushResult::Full => Err(SubmitError::QueueFull {
                revision: update.revision,
            }),
        }
    }

    pub fn submit_update(
        &self,
        update: EngineUpdate,
    ) -> std::result::Result<SubmitReceipt, SubmitError> {
        self.submit(update)
    }

    /// Build coefficients on the control path and submit one update.
    pub fn apply_preset(
        &self,
        preset: &Preset,
        sample_rate: f64,
        revision: u64,
        bypassed: bool,
    ) -> Result<SubmitReceipt> {
        let bank = FilterBank::new(preset, sample_rate)?;
        self.submit(EngineUpdate::new(bank, revision, bypassed))
            .map_err(|error| anyhow::anyhow!("could not submit engine update: {error:?}"))
    }

    pub fn accepted_revision(&self) -> u64 {
        self.accepted_revision.load(Ordering::Acquire)
    }

    /// Obtain a queue reference for constructing a realtime test owner. The
    /// callback itself never clones this `Arc`.
    pub fn queue(&self) -> Arc<EngineUpdateQueue> {
        self.queue.clone()
    }
}

impl Default for NativeEngineHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Realtime-owned processor state and its pending update queue.
pub struct ProcessorState {
    processor: StereoProcessor,
    queue: Arc<EngineUpdateQueue>,
    active_revision: u64,
}

impl ProcessorState {
    pub fn new(
        bank: FilterBank,
        queue: Arc<EngineUpdateQueue>,
        active_revision: u64,
        bypassed: bool,
    ) -> Self {
        let mut processor = StereoProcessor::from_bank(bank);
        processor.set_bypassed(bypassed);
        Self {
            processor,
            queue,
            active_revision,
        }
    }

    #[inline]
    pub fn process_buffers(&mut self, left: &mut [f32], right: &mut [f32]) {
        process_buffers(
            &mut self.processor,
            &self.queue,
            &mut self.active_revision,
            left,
            right,
        );
    }

    pub fn active_revision(&self) -> u64 {
        self.active_revision
    }

    pub fn processor(&self) -> &StereoProcessor {
        &self.processor
    }
}

/// Apply queued updates and process one planar block without allocation,
/// locking, validation, logging, or I/O.
#[inline]
pub fn process_buffers(
    processor: &mut StereoProcessor,
    queue: &EngineUpdateQueue,
    active_revision: &mut u64,
    left: &mut [f32],
    right: &mut [f32],
) {
    let mut newest = None;
    while let Some(update) = queue.pop() {
        if update.revision > *active_revision
            && newest
                .map(|candidate: EngineUpdate| update.revision > candidate.revision)
                .unwrap_or(true)
        {
            newest = Some(update);
        }
    }

    if let Some(update) = newest {
        processor.replace_bank(update.bank);
        processor.set_bypassed(update.bypassed);
        *active_revision = update.revision;
    }
    processor.process_planar(left, right);
}

type PwFilter = pw::sys::pw_filter;
type PwBuffer = pw::sys::pw_buffer;
type PwProperties = pw::sys::pw_properties;
type PwCore = pw::sys::pw_core;
type PwLoop = pw::sys::pw_loop;
type SpaPod = pw::spa::sys::spa_pod;
type SpaHook = pw::spa::sys::spa_hook;
type SpaCommand = pw::spa::sys::spa_command;
type SpaDict = pw::spa::sys::spa_dict;
type SpaEvent = pw::spa::sys::spa_event;
type SpaIoPosition = pw::spa::sys::spa_io_position;

#[repr(C)]
struct PwFilterEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, i32, i32, *const c_char)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const SpaPod)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void, *mut SpaIoPosition)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const SpaCommand)>,
}

// These declarations are copied from filter.h.  The ownership rules are
// encoded by NativePwFilter below: PipeWire owns properties after the create
// calls, and the filter remains alive before its callback state is dropped.
unsafe extern "C" {
    fn pw_filter_new_simple(
        loop_: *mut PwLoop,
        name: *const c_char,
        props: *mut PwProperties,
        events: *const PwFilterEvents,
        data: *mut c_void,
    ) -> *mut PwFilter;
    fn pw_filter_destroy(filter: *mut PwFilter);
    fn pw_filter_connect(
        filter: *mut PwFilter,
        flags: u32,
        params: *const *const SpaPod,
        n_params: u32,
    ) -> i32;
    fn pw_filter_get_node_id(filter: *mut PwFilter) -> u32;
    fn pw_filter_add_port(
        filter: *mut PwFilter,
        direction: i32,
        flags: u32,
        port_data_size: usize,
        props: *mut PwProperties,
        params: *const *const SpaPod,
        n_params: u32,
    ) -> *mut c_void;
    fn pw_filter_get_dsp_buffer(port_data: *mut c_void, n_samples: u32) -> *mut c_void;
}

// Values from pipewire/port.h.  They are stable C enum values, and spelling
// them here avoids importing a second generated bindings crate.
const DIRECTION_INPUT: i32 = 0;
const DIRECTION_OUTPUT: i32 = 1;

struct FilterState {
    inputs: [*mut c_void; CHANNELS],
    outputs: [*mut c_void; CHANNELS],
    processor: ProcessorState,
    effective_rate_hz: AtomicU32,
    process_calls: AtomicU64,
}

impl FilterState {
    fn new(bank: FilterBank, queue: Arc<EngineUpdateQueue>) -> Self {
        Self {
            inputs: [ptr::null_mut(); CHANNELS],
            outputs: [ptr::null_mut(); CHANNELS],
            processor: ProcessorState::new(bank, queue, 0, false),
            effective_rate_hz: AtomicU32::new(0),
            process_calls: AtomicU64::new(0),
        }
    }
}

// The state is allocated once before PipeWire can invoke process().  It is
// never moved, and all values read from the audio callback are either immutable
// pointers or atomics.  NativePwFilter itself is intentionally !Send below;
// the callbacks are only installed on its owning PipeWire loop.
unsafe impl Send for FilterState {}
unsafe impl Sync for FilterState {}

unsafe extern "C" fn process_callback(data: *mut c_void, position: *mut SpaIoPosition) {
    // SAFETY: PipeWire calls this with the exact pointer supplied to
    // pw_filter_new_simple.  The owner keeps the Box<FilterState> alive until
    // pw_filter_destroy has returned, so this callback cannot outlive state.
    let Some(state) = (data as *mut FilterState).as_mut() else {
        return;
    };
    if position.is_null() {
        return;
    }

    // SAFETY: position is provided by PipeWire for this process cycle.  The
    // duration is the number of samples in each mono DSP port.
    let position = unsafe { &*position };
    let duration = position.clock.duration;
    if duration == 0 || duration > u32::MAX as u64 {
        return;
    }
    let n_samples = duration as u32;

    let rate = position.clock.rate;
    if rate.num > 0 && rate.denom > 0 {
        state
            .effective_rate_hz
            .store(rate.denom / rate.num, Ordering::Relaxed);
    }

    let mut outputs = [ptr::null_mut(); CHANNELS];
    let mut inputs = [ptr::null_mut(); CHANNELS];
    for channel in 0..CHANNELS {
        // SAFETY: Port pointers are returned by pw_filter_add_port and remain
        // valid for the life of the filter. These functions are RT-safe by the
        // PipeWire filter API contract; they only access preallocated buffers.
        outputs[channel] =
            unsafe { pw_filter_get_dsp_buffer(state.outputs[channel], n_samples).cast::<f32>() };
        inputs[channel] =
            unsafe { pw_filter_get_dsp_buffer(state.inputs[channel], n_samples).cast::<f32>() };
    }

    // Keep the update drain and processor state in one extracted function so
    // the same callback contract is testable without a PipeWire graph.
    if outputs[0].is_null() && outputs[1].is_null() {
        state.processor.process_buffers(&mut [], &mut []);
    } else if !outputs[0].is_null() && !outputs[1].is_null() {
        for channel in 0..CHANNELS {
            // SAFETY: each mapped DSP port contains n_samples f32 values.
            if inputs[channel].is_null() {
                unsafe {
                    ptr::write_bytes(
                        outputs[channel].cast::<u8>(),
                        0,
                        n_samples as usize * std::mem::size_of::<f32>(),
                    )
                };
            } else {
                unsafe {
                    ptr::copy(
                        inputs[channel].cast::<u8>(),
                        outputs[channel].cast::<u8>(),
                        n_samples as usize * std::mem::size_of::<f32>(),
                    )
                };
            }
        }
        // SAFETY: output pointers are distinct stereo port buffers and each
        // has the declared n_samples f32 elements.
        let left = unsafe { std::slice::from_raw_parts_mut(outputs[0], n_samples as usize) };
        let right = unsafe { std::slice::from_raw_parts_mut(outputs[1], n_samples as usize) };
        state.processor.process_buffers(left, right);
    } else {
        // A partially disconnected graph still drains updates and processes
        // the connected channel. The missing channel is represented as zero;
        // its output is discarded when no output port exists.
        state.processor.process_buffers(&mut [], &mut []);
        for index in 0..n_samples as usize {
            let left = if inputs[0].is_null() {
                0.0
            } else {
                unsafe { inputs[0].add(index).read() }
            };
            let right = if inputs[1].is_null() {
                0.0
            } else {
                unsafe { inputs[1].add(index).read() }
            };
            let (left, right) = state.processor.processor.process_frame_f32(left, right);
            if !outputs[0].is_null() {
                unsafe { outputs[0].add(index).write(left) };
            }
            if !outputs[1].is_null() {
                unsafe { outputs[1].add(index).write(right) };
            }
        }
    }
    state.process_calls.fetch_add(1, Ordering::Relaxed);
}

static FILTER_EVENTS: PwFilterEvents = PwFilterEvents {
    version: 1,
    destroy: None,
    state_changed: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(process_callback),
    drained: None,
    command: None,
};

fn port_properties(name: &str, channel: &str) -> Result<*mut PwProperties> {
    let mut props = pw::properties::PropertiesBox::new();
    props.insert("format.dsp", "32 bit float mono audio");
    props.insert("port.name", name);
    props.insert("port.alias", name);
    props.insert("audio.channel", channel);
    Ok(props.into_raw())
}

/// A connected, stable, duplex stereo PipeWire filter.
///
/// This owner is deliberately tied to the creating thread.  Its main loop and
/// PipeWire handles are local objects, and the only realtime callback is the
/// fixed-size passthrough above.
pub struct NativePwFilter {
    main_loop: pw::main_loop::MainLoopRc,
    filter: NonNull<PwFilter>,
    state: Box<FilterState>,
    engine: NativeEngineHandle,
    node_id: u32,
    // Rc is !Send and documents that the loop owner must remain single-threaded.
    _not_send: PhantomData<Rc<()>>,
}

impl NativePwFilter {
    /// Create the stable `peq` Audio/Sink node with input/output FL and FR ports.
    pub fn new() -> Result<Self> {
        let preset = Preset::new("native-neutral");
        Self::new_with_preset(&preset, DEFAULT_SAMPLE_RATE_HZ)
    }

    /// Create the filter with a bank prepared for the target stream rate.
    pub fn new_with_preset(preset: &Preset, sample_rate: f64) -> Result<Self> {
        let bank = FilterBank::new(preset, sample_rate)?;
        Self::new_with_bank(bank)
    }

    fn new_with_bank(bank: FilterBank) -> Result<Self> {
        pw::init();
        let main_loop =
            pw::main_loop::MainLoopRc::new(None).context("creating native PipeWire main loop")?;
        let queue = Arc::new(EngineUpdateQueue::new());
        let engine = NativeEngineHandle::with_queue(queue.clone(), 0);
        let mut state = Box::new(FilterState::new(bank, queue));
        let state_ptr = (&mut *state) as *mut FilterState as *mut c_void;
        let name = CString::new("peq").context("building native filter name")?;
        let mut filter_props = pw::properties::PropertiesBox::new();
        filter_props.insert("node.name", "peq");
        filter_props.insert("node.description", "peq Audio/Sink");
        filter_props.insert("media.class", "Audio/Sink");
        filter_props.insert("media.type", "Audio");
        filter_props.insert("media.category", "Filter");
        filter_props.insert("media.role", "DSP");
        filter_props.insert("node.autoconnect", "false");
        filter_props.insert("node.always-process", "true");

        // SAFETY: main_loop.loop_() remains alive in this owner. PipeWire takes
        // ownership of filter_props after this call, and stores the static event
        // table plus state_ptr until pw_filter_destroy.
        let raw_filter = unsafe {
            pw_filter_new_simple(
                main_loop.loop_().as_raw_ptr(),
                name.as_ptr(),
                filter_props.into_raw(),
                &FILTER_EVENTS,
                state_ptr,
            )
        };
        let Some(filter) = NonNull::new(raw_filter) else {
            bail!("pw_filter_new_simple returned NULL")
        };

        let channels = [
            ("FL", "input_FL", "output_FL"),
            ("FR", "input_FR", "output_FR"),
        ];
        let mut input_ports = [ptr::null_mut(); CHANNELS];
        let mut output_ports = [ptr::null_mut(); CHANNELS];
        for (index, (channel, input_name, output_name)) in channels.into_iter().enumerate() {
            let input_props = port_properties(input_name, channel)?;
            // SAFETY: filter is valid and takes ownership of input_props. The
            // port has no format POD because its DSP format is fixed by props.
            let input = unsafe {
                pw_filter_add_port(
                    filter.as_ptr(),
                    DIRECTION_INPUT,
                    FILTER_PORT_FLAG_MAP_BUFFERS,
                    0,
                    input_props,
                    ptr::null(),
                    0,
                )
            };
            if input.is_null() {
                // SAFETY: no callback can be active before connect; filter is
                // still owned locally and can be destroyed here.
                unsafe { pw_filter_destroy(filter.as_ptr()) };
                bail!("pw_filter_add_port failed for {input_name}");
            }
            input_ports[index] = input;

            let output_props = port_properties(output_name, channel)?;
            // SAFETY: same ownership and validity conditions as input above.
            let output = unsafe {
                pw_filter_add_port(
                    filter.as_ptr(),
                    DIRECTION_OUTPUT,
                    FILTER_PORT_FLAG_MAP_BUFFERS,
                    0,
                    output_props,
                    ptr::null(),
                    0,
                )
            };
            if output.is_null() {
                unsafe { pw_filter_destroy(filter.as_ptr()) };
                bail!("pw_filter_add_port failed for {output_name}");
            }
            output_ports[index] = output;
        }
        state.inputs = input_ports;
        state.outputs = output_ports;

        // SAFETY: all ports and the static callback table are initialized. A
        // null parameter list is explicitly permitted by pw_filter_connect.
        let result =
            unsafe { pw_filter_connect(filter.as_ptr(), FILTER_FLAGS_RT_PROCESS, ptr::null(), 0) };
        if result < 0 {
            unsafe { pw_filter_destroy(filter.as_ptr()) };
            bail!("pw_filter_connect failed with {result}");
        }
        // SAFETY: filter is connected and remains owned by this struct.
        let node_id = unsafe { pw_filter_get_node_id(filter.as_ptr()) };
        Ok(Self {
            main_loop,
            filter,
            state,
            engine,
            node_id,
            _not_send: PhantomData,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Access the control producer for this stable filter node.
    pub fn engine_handle(&self) -> &NativeEngineHandle {
        &self.engine
    }

    /// Move the producer to a control owner when it must outlive a borrow of
    /// this PipeWire owner. The callback keeps its own queue reference.
    pub fn take_engine_handle(&mut self) -> NativeEngineHandle {
        std::mem::take(&mut self.engine)
    }

    /// Revision last applied by the realtime callback.
    pub fn applied_revision(&self) -> u64 {
        self.state.processor.active_revision()
    }

    /// Effective graph sample rate published by the realtime callback.
    pub fn effective_sample_rate(&self) -> Option<u32> {
        match self.state.effective_rate_hz.load(Ordering::Relaxed) {
            0 => None,
            rate => Some(rate),
        }
    }

    pub fn process_calls(&self) -> u64 {
        self.state.process_calls.load(Ordering::Relaxed)
    }

    /// Dispatch one bounded main-loop iteration.
    pub fn iterate(&self, timeout: Duration) -> i32 {
        self.main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(timeout))
    }

    /// Run the owner loop for a bounded duration. Tests and callers should
    /// always use this instead of an unbounded loop while bringing up a graph.
    pub fn run_for(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.iterate(remaining.min(Duration::from_millis(50)));
        }
    }
}

impl Drop for NativePwFilter {
    fn drop(&mut self) {
        // SAFETY: all PipeWire callbacks have finished before the filter is
        // destroyed by the owning loop thread; state remains alive until this
        // call returns. PipeWire frees the properties and ports it owns.
        unsafe { pw_filter_destroy(self.filter.as_ptr()) };
    }
}

struct SourceState {
    outputs: [*mut c_void; CHANNELS],
    frequencies_hz: [f32; CHANNELS],
    phases: [f32; CHANNELS],
    effective_rate_hz: AtomicU32,
    process_calls: AtomicU64,
    frames: AtomicU64,
}

unsafe extern "C" fn source_process_callback(data: *mut c_void, position: *mut SpaIoPosition) {
    // SAFETY: PipeWire passes the same SourceState pointer supplied at filter
    // creation.  The owner keeps it boxed until pw_filter_destroy returns, and
    // the process callback is serialized for one source filter.
    let Some(state) = (data as *mut SourceState).as_mut() else {
        return;
    };
    if position.is_null() {
        return;
    }
    // SAFETY: position is valid for this callback according to filter.h.
    let position = unsafe { &*position };
    let duration = position.clock.duration;
    if duration == 0 || duration > u32::MAX as u64 {
        return;
    }
    let n_samples = duration as usize;
    let rate = position.clock.rate;
    let rate_hz = if rate.num > 0 && rate.denom > 0 {
        rate.denom as f32 / rate.num as f32
    } else {
        48_000.0
    };
    state
        .effective_rate_hz
        .store(rate_hz as u32, Ordering::Relaxed);

    let mut produced = 0usize;
    for channel in 0..CHANNELS {
        // SAFETY: The port is owned by this connected filter and the returned
        // DSP buffer has n_samples f32 elements. The function is RT-safe.
        let output = unsafe {
            pw_filter_get_dsp_buffer(state.outputs[channel], n_samples as u32).cast::<f32>()
        };
        if output.is_null() {
            continue;
        }
        let step = std::f32::consts::TAU * state.frequencies_hz[channel] / rate_hz;
        for index in 0..n_samples {
            // SAFETY: PipeWire allocated n_samples samples for this DSP port.
            unsafe { output.add(index).write(state.phases[channel].sin() * 0.20) };
            state.phases[channel] += step;
            if state.phases[channel] >= std::f32::consts::TAU {
                state.phases[channel] -= std::f32::consts::TAU;
            }
        }
        produced = n_samples;
    }
    state.frames.fetch_add(produced as u64, Ordering::Relaxed);
    state.process_calls.fetch_add(1, Ordering::Relaxed);
}

static SOURCE_EVENTS: PwFilterEvents = PwFilterEvents {
    version: 1,
    destroy: None,
    state_changed: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(source_process_callback),
    drained: None,
    command: None,
};

/// A small independent playback client for the C03 graph proof.
///
/// It is implemented with the same native filter boundary as the sink so the
/// proof can create two real output nodes without a session manager. The shell
/// harness links each source explicitly to `peq` with `pw-link`.
pub struct NativePwSource {
    main_loop: pw::main_loop::MainLoopRc,
    filter: NonNull<PwFilter>,
    state: Box<SourceState>,
    node_id: u32,
    _not_send: PhantomData<Rc<()>>,
}

impl NativePwSource {
    pub fn new(name: &str, left_hz: f32, right_hz: f32) -> Result<Self> {
        pw::init();
        let main_loop =
            pw::main_loop::MainLoopRc::new(None).context("creating source PipeWire main loop")?;
        let mut state = Box::new(SourceState {
            outputs: [ptr::null_mut(); CHANNELS],
            frequencies_hz: [left_hz, right_hz],
            phases: [0.0, 0.0],
            effective_rate_hz: AtomicU32::new(0),
            process_calls: AtomicU64::new(0),
            frames: AtomicU64::new(0),
        });
        let state_ptr = (&mut *state) as *mut SourceState as *mut c_void;
        let filter_name = CString::new(name).context("building source filter name")?;
        let mut filter_props = pw::properties::PropertiesBox::new();
        filter_props.insert("node.name", name);
        filter_props.insert("node.description", name);
        filter_props.insert("media.class", "Stream/Output/Audio");
        filter_props.insert("media.type", "Audio");
        filter_props.insert("media.category", "Playback");
        filter_props.insert("media.role", "Music");
        filter_props.insert("node.autoconnect", "false");
        filter_props.insert("node.always-process", "true");

        // SAFETY: The loop, event table, and state remain alive for the entire
        // connected filter lifetime. PipeWire takes ownership of properties.
        let raw_filter = unsafe {
            pw_filter_new_simple(
                main_loop.loop_().as_raw_ptr(),
                filter_name.as_ptr(),
                filter_props.into_raw(),
                &SOURCE_EVENTS,
                state_ptr,
            )
        };
        let Some(filter) = NonNull::new(raw_filter) else {
            bail!("pw_filter_new_simple returned NULL for source {name}")
        };

        let channels = [("FL", "output_FL"), ("FR", "output_FR")];
        for (index, (channel, port_name)) in channels.into_iter().enumerate() {
            let props = port_properties(port_name, channel)?;
            // SAFETY: filter is valid, takes ownership of props, and the output
            // port remains valid while the filter is connected.
            let output = unsafe {
                pw_filter_add_port(
                    filter.as_ptr(),
                    DIRECTION_OUTPUT,
                    FILTER_PORT_FLAG_MAP_BUFFERS,
                    0,
                    props,
                    ptr::null(),
                    0,
                )
            };
            if output.is_null() {
                unsafe { pw_filter_destroy(filter.as_ptr()) };
                bail!("pw_filter_add_port failed for source {port_name}");
            }
            state.outputs[index] = output;
        }

        // SAFETY: the output ports and static callback table are initialized;
        // no parameter POD is required for fixed DSP ports.
        let result =
            unsafe { pw_filter_connect(filter.as_ptr(), FILTER_FLAGS_RT_PROCESS, ptr::null(), 0) };
        if result < 0 {
            unsafe { pw_filter_destroy(filter.as_ptr()) };
            bail!("pw_filter_connect failed for source {name} with {result}");
        }
        // SAFETY: filter is connected and remains owned by this value.
        let node_id = unsafe { pw_filter_get_node_id(filter.as_ptr()) };
        Ok(Self {
            main_loop,
            filter,
            state,
            node_id,
            _not_send: PhantomData,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn effective_sample_rate(&self) -> Option<u32> {
        match self.state.effective_rate_hz.load(Ordering::Relaxed) {
            0 => None,
            rate => Some(rate),
        }
    }

    pub fn process_calls(&self) -> u64 {
        self.state.process_calls.load(Ordering::Relaxed)
    }

    pub fn frames(&self) -> u64 {
        self.state.frames.load(Ordering::Relaxed)
    }

    pub fn run_for(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.main_loop.loop_().iterate(pw::loop_::Timeout::Finite(
                remaining.min(Duration::from_millis(50)),
            ));
        }
    }
}

impl Drop for NativePwSource {
    fn drop(&mut self) {
        // SAFETY: the owning thread has stopped its loop before drop, so no
        // callback can access state after PipeWire destroys the filter.
        unsafe { pw_filter_destroy(self.filter.as_ptr()) };
    }
}

// Keep these aliases visible to rustdoc users inspecting the FFI boundary and
// prevent accidental drift if PipeWire changes its opaque declarations.
#[allow(dead_code)]
fn _ffi_layout_markers(_: *mut PwCore, _: *mut SpaHook, _: *const SpaDict, _: *const SpaEvent) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::Preset;

    fn neutral_bank() -> FilterBank {
        FilterBank::new(&Preset::new("native-test"), DEFAULT_SAMPLE_RATE_HZ)
            .expect("test preset is valid")
    }

    fn gain_bank(gain_db: f64) -> FilterBank {
        let mut preset = Preset::new("native-test");
        preset.preamp_db = gain_db;
        FilterBank::new(&preset, DEFAULT_SAMPLE_RATE_HZ).expect("test preset is valid")
    }

    #[test]
    fn callback_owner_applies_newest_revision_and_rejects_stale_control() {
        let handle = NativeEngineHandle::new();
        let queue = handle.queue();
        let mut state = ProcessorState::new(neutral_bank(), queue, 0, false);

        handle
            .submit(EngineUpdate::new(gain_bank(3.0), 2, false))
            .expect("revision 2 should be accepted");
        let stale = handle.submit(EngineUpdate::new(gain_bank(1.0), 1, false));
        assert_eq!(
            stale,
            Err(SubmitError::StaleRevision {
                revision: 1,
                latest: 2
            })
        );

        let mut left = [1.0_f32; 8];
        let mut right = [1.0_f32; 8];
        state.process_buffers(&mut left, &mut right);
        assert_eq!(state.active_revision(), 2);
        assert!(left
            .iter()
            .all(|sample| (*sample - 10f32.powf(3.0 / 20.0)).abs() < 1e-6));
        assert_eq!(left, right);
    }

    #[test]
    fn bypass_update_reaches_callback_without_rebuilding_owner() {
        let handle = NativeEngineHandle::new();
        let queue = handle.queue();
        let mut state = ProcessorState::new(gain_bank(6.0), queue, 0, false);
        handle
            .submit(EngineUpdate::new(gain_bank(6.0), 1, true))
            .expect("bypass update should be accepted");

        let mut left = [0.25_f32; 4];
        let mut right = [-0.5_f32; 4];
        state.process_buffers(&mut left, &mut right);
        assert_eq!(state.active_revision(), 1);
        assert_eq!(left, [0.25; 4]);
        assert_eq!(right, [-0.5; 4]);
    }
}
