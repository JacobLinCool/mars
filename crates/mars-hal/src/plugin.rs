//! AudioServerPlugIn COM-style driver implementation.
//!
//! This module implements the real `AudioServerPlugInDriverInterface` vtable that
//! coreaudiod loads.  Config updates arrive via `SetPropertyData` on the custom
//! properties (`kMarsPropertyDesiredState`, etc.) and flow through the existing
//! `DRIVER_STATE` machinery in `crate::lib`.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::coreaudio_types::*;
use crate::shm_backend::{RingSpec, StreamDirection, global_registry, stream_name};
use crate::{
    DRIVER_STATE, applied_state_json, configuration_summary_json,
    perform_device_configuration_change, request_device_configuration_change, runtime_stats_json,
    set_desired_state_json,
};

// ===========================================================================
// Global state
// ===========================================================================

struct MarsDriverPlugin {
    ref_count: AtomicU32,
    host: Mutex<Option<AudioServerPlugInHostRef>>,
    object_registry: Mutex<ObjectRegistry>,
}

// `MarsDriverPlugin` contains atomics, a `Mutex`, and a raw pointer behind `Mutex`.
// The `Mutex` guard ensures exclusive access to the host pointer; the `AtomicU32`
// is inherently `Send + Sync`.  The host pointer is only stored/read under the
// lock and is valid for the plugin's lifetime inside coreaudiod.
unsafe impl Send for MarsDriverPlugin {}
unsafe impl Sync for MarsDriverPlugin {}

#[derive(Debug)]
struct ObjectRegistry {
    next_id: AudioObjectID,
    devices: BTreeMap<String, DeviceObjectInfo>,
}

#[derive(Debug, Clone)]
struct DeviceObjectInfo {
    device_id: AudioObjectID,
    stream_id: AudioObjectID,
    uid: String,
    name: String,
    kind: String,
    channels: u16,
    hidden: bool,
    io_running: bool,
}

impl Default for ObjectRegistry {
    fn default() -> Self {
        Self {
            next_id: 2, // 1 = plugin object
            devices: BTreeMap::new(),
        }
    }
}

impl ObjectRegistry {
    fn allocate_id(&mut self) -> AudioObjectID {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    fn find_device_by_object(&self, object_id: AudioObjectID) -> Option<&DeviceObjectInfo> {
        self.devices.values().find(|d| d.device_id == object_id)
    }

    fn find_device_by_object_mut(
        &mut self,
        object_id: AudioObjectID,
    ) -> Option<&mut DeviceObjectInfo> {
        self.devices.values_mut().find(|d| d.device_id == object_id)
    }

    fn find_device_by_stream(&self, stream_id: AudioObjectID) -> Option<&DeviceObjectInfo> {
        self.devices.values().find(|d| d.stream_id == stream_id)
    }

    fn all_device_ids(&self) -> Vec<AudioObjectID> {
        self.devices.values().map(|d| d.device_id).collect()
    }
}

static PLUGIN: Lazy<MarsDriverPlugin> = Lazy::new(|| MarsDriverPlugin {
    ref_count: AtomicU32::new(1),
    host: Mutex::new(None),
    object_registry: Mutex::new(ObjectRegistry::default()),
});

// ===========================================================================
// COM vtable (static)
// ===========================================================================

static VTABLE: AudioServerPlugInDriverVTable = AudioServerPlugInDriverVTable {
    query_interface: plugin_query_interface,
    add_ref: plugin_add_ref,
    release: plugin_release,
    initialize: plugin_initialize,
    create_device: plugin_create_device,
    destroy_device: plugin_destroy_device,
    add_device_client: plugin_add_device_client,
    remove_device_client: plugin_remove_device_client,
    perform_device_configuration_change: plugin_perform_device_configuration_change,
    abort_device_configuration_change: plugin_abort_device_configuration_change,
    has_property: plugin_has_property,
    is_property_settable: plugin_is_property_settable,
    get_property_data_size: plugin_get_property_data_size,
    get_property_data: plugin_get_property_data,
    set_property_data: plugin_set_property_data,
    start_io: plugin_start_io,
    stop_io: plugin_stop_io,
    get_zero_time_stamp: plugin_get_zero_time_stamp,
    will_do_io_operation: plugin_will_do_io_operation,
    begin_io_operation: plugin_begin_io_operation,
    do_io_operation: plugin_do_io_operation,
    end_io_operation: plugin_end_io_operation,
};

/// Wrapper for a raw pointer that is `Sync + Send`.
///
/// The vtable pointer is to a `static` and lives for the entire process — it is
/// safe to share across threads.
struct SyncVTablePtr(*const AudioServerPlugInDriverVTable);
unsafe impl Sync for SyncVTablePtr {}
unsafe impl Send for SyncVTablePtr {}

static VTABLE_PTR: SyncVTablePtr = SyncVTablePtr(&VTABLE);

// ===========================================================================
// Factory function — the single exported symbol for CoreAudio host
// ===========================================================================

/// CoreAudio host calls this to create the driver.  Returns a COM double-indirection
/// pointer (`**vtable`).
///
/// # Safety
/// Must only be called by the CoreAudio host with valid CFAllocatorRef and CFUUID parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MarsAudioServerPlugInFactory(
    _allocator: *const c_void,
    _requested_type_uuid: *const c_void,
) -> *mut c_void {
    // Force lazy init.
    let _ = &*PLUGIN;
    // Return pointer-to-pointer (COM convention).
    (&VTABLE_PTR.0 as *const *const AudioServerPlugInDriverVTable)
        .cast_mut()
        .cast::<c_void>()
}

// ===========================================================================
// COM / IUnknown
// ===========================================================================

unsafe extern "C" fn plugin_query_interface(
    _driver: *mut c_void,
    iid: REFIID,
    interface: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || interface.is_null() {
        return E_NOINTERFACE;
    }

    // SAFETY: The IID is a 16-byte UUID passed by the host.
    let iid_bytes: &[u8; 16] = unsafe { &*iid.cast::<[u8; 16]>() };

    if *iid_bytes == IID_IUNKNOWN || *iid_bytes == IID_AUDIO_SERVER_PLUGIN_DRIVER {
        PLUGIN.ref_count.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `interface` is non-null, checked above.
        unsafe {
            *interface = (&VTABLE_PTR.0 as *const *const AudioServerPlugInDriverVTable)
                .cast_mut()
                .cast::<c_void>();
        }
        return S_OK;
    }

    E_NOINTERFACE
}

unsafe extern "C" fn plugin_add_ref(_driver: *mut c_void) -> ULONG {
    PLUGIN.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "C" fn plugin_release(_driver: *mut c_void) -> ULONG {
    let prev = PLUGIN.ref_count.fetch_sub(1, Ordering::Relaxed);
    prev.saturating_sub(1)
}

// ===========================================================================
// Lifecycle
// ===========================================================================

unsafe extern "C" fn plugin_initialize(
    _driver: *mut c_void,
    host: AudioServerPlugInHostRef,
) -> OSStatus {
    *PLUGIN.host.lock() = Some(host);
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_create_device(
    _driver: *mut c_void,
    _description: *const c_void,
    _client_info: *const AudioServerPlugInClientInfo,
    _device_object_id: *mut AudioObjectID,
) -> OSStatus {
    K_AUDIO_HARDWARE_UNSUPPORTED_OPERATION_ERROR
}

unsafe extern "C" fn plugin_destroy_device(
    _driver: *mut c_void,
    device_object_id: AudioObjectID,
) -> OSStatus {
    let mut reg = PLUGIN.object_registry.lock();
    let uid_to_remove = reg
        .devices
        .iter()
        .find(|(_, info)| info.device_id == device_object_id)
        .map(|(uid, _)| uid.clone());
    if let Some(uid) = uid_to_remove {
        reg.devices.remove(&uid);
        let _ = global_registry().remove(&stream_name(StreamDirection::Vout, &uid));
        let _ = global_registry().remove(&stream_name(StreamDirection::Vin, &uid));
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_add_device_client(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    _client_info: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_remove_device_client(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    _client_info: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    K_AUDIO_HARDWARE_NO_ERROR
}

// ===========================================================================
// Configuration change
// ===========================================================================

unsafe extern "C" fn plugin_perform_device_configuration_change(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    change_action: u64,
    _change_info: *const c_void,
) -> OSStatus {
    // `change_action` is the generation token from `request_device_configuration_change()`.
    let result = perform_device_configuration_change(change_action);
    if result.is_err() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // Sync object registry with newly applied state.
    sync_object_registry();

    // Notify host that device list may have changed.
    notify_device_list_changed();

    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_abort_device_configuration_change(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    _change_action: u64,
    _change_info: *const c_void,
) -> OSStatus {
    // Clear pending change in DRIVER_STATE.
    let mut state = DRIVER_STATE.lock();
    state.pending_change = None;
    K_AUDIO_HARDWARE_NO_ERROR
}

// ===========================================================================
// Property dispatch helpers
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectType {
    Plugin,
    Device,
    Stream,
}

fn classify_object(object_id: AudioObjectID) -> Option<ObjectType> {
    if object_id == K_AUDIO_OBJECT_PLUGIN_OBJECT {
        return Some(ObjectType::Plugin);
    }
    let reg = PLUGIN.object_registry.lock();
    if reg.find_device_by_object(object_id).is_some() {
        return Some(ObjectType::Device);
    }
    if reg.find_device_by_stream(object_id).is_some() {
        return Some(ObjectType::Stream);
    }
    None
}

fn plugin_has_property_for(selector: UInt32) -> bool {
    matches!(
        selector,
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS
            | K_AUDIO_OBJECT_PROPERTY_CLASS
            | K_AUDIO_OBJECT_PROPERTY_OWNER
            | K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS
            | K_AUDIO_PLUG_IN_PROPERTY_BUNDLE_ID
            | K_AUDIO_PLUG_IN_PROPERTY_DEVICE_LIST
            | K_AUDIO_PLUG_IN_PROPERTY_RESOURCE_BUNDLE
            | K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST
            | K_MARS_PROPERTY_DESIRED_STATE
            | K_MARS_PROPERTY_APPLIED_STATE
            | K_MARS_PROPERTY_RUNTIME_STATS
            | K_MARS_PROPERTY_CONFIG_SUMMARY
    )
}

fn device_has_property_for(selector: UInt32) -> bool {
    matches!(
        selector,
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS
            | K_AUDIO_OBJECT_PROPERTY_CLASS
            | K_AUDIO_OBJECT_PROPERTY_OWNER
            | K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS
            | K_AUDIO_OBJECT_PROPERTY_NAME
            | K_AUDIO_OBJECT_PROPERTY_MANUFACTURER
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_UID
            | K_AUDIO_DEVICE_PROPERTY_MODEL_UID
            | K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_DEVICE
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_SYSTEM_DEVICE
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_HIDDEN
            | K_AUDIO_DEVICE_PROPERTY_LATENCY
            | K_AUDIO_DEVICE_PROPERTY_STREAMS
            | K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE
            | K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES
            | K_AUDIO_DEVICE_PROPERTY_ZERO_TIME_STAMP_PERIOD
            | K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET
            | K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN
            | K_AUDIO_DEVICE_PROPERTY_IS_ALIVE
            | K_AUDIO_DEVICE_PROPERTY_IS_RUNNING
            | K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO
    )
}

fn stream_has_property_for(selector: UInt32) -> bool {
    matches!(
        selector,
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS
            | K_AUDIO_OBJECT_PROPERTY_CLASS
            | K_AUDIO_OBJECT_PROPERTY_OWNER
            | K_AUDIO_STREAM_PROPERTY_DIRECTION
            | K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE
            | K_AUDIO_STREAM_PROPERTY_START_CHANNEL
            | K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT
            | K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT
            | K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS
            | K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS
            | K_AUDIO_STREAM_PROPERTY_LATENCY
            | K_AUDIO_STREAM_PROPERTY_IS_ACTIVE
    )
}

// ===========================================================================
// Property operations
// ===========================================================================

unsafe extern "C" fn plugin_has_property(
    _driver: *mut c_void,
    object_id: AudioObjectID,
    _client_process_id: i32,
    address: *const AudioObjectPropertyAddress,
) -> Boolean {
    if address.is_null() {
        return 0;
    }
    // SAFETY: `address` is non-null, provided by the host.
    let addr = unsafe { &*address };
    let has = match classify_object(object_id) {
        Some(ObjectType::Plugin) => plugin_has_property_for(addr.m_selector),
        Some(ObjectType::Device) => device_has_property_for(addr.m_selector),
        Some(ObjectType::Stream) => stream_has_property_for(addr.m_selector),
        None => false,
    };
    if has { 1 } else { 0 }
}

unsafe extern "C" fn plugin_is_property_settable(
    _driver: *mut c_void,
    object_id: AudioObjectID,
    _client_process_id: i32,
    address: *const AudioObjectPropertyAddress,
    is_settable: *mut Boolean,
) -> OSStatus {
    if address.is_null() || is_settable.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: `address` is non-null, provided by the host.
    let addr = unsafe { &*address };

    let settable = match classify_object(object_id) {
        Some(ObjectType::Plugin) => addr.m_selector == K_MARS_PROPERTY_DESIRED_STATE,
        Some(ObjectType::Device) => addr.m_selector == K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE,
        Some(ObjectType::Stream) => matches!(
            addr.m_selector,
            K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT | K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT
        ),
        None => return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR,
    };

    // SAFETY: `is_settable` is non-null, checked above.
    unsafe { *is_settable = if settable { 1 } else { 0 } };
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_get_property_data_size(
    _driver: *mut c_void,
    object_id: AudioObjectID,
    _client_process_id: i32,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: UInt32,
    _qualifier_data: *const c_void,
    data_size: *mut UInt32,
) -> OSStatus {
    if address.is_null() || data_size.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: `address` is non-null, provided by the host.
    let addr = unsafe { &*address };

    let size = match classify_object(object_id) {
        Some(ObjectType::Plugin) => plugin_property_data_size(addr.m_selector),
        Some(ObjectType::Device) => device_property_data_size(object_id, addr.m_selector),
        Some(ObjectType::Stream) => stream_property_data_size(addr.m_selector),
        None => return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR,
    };

    match size {
        Some(s) => {
            // SAFETY: `data_size` is non-null, checked above.
            unsafe { *data_size = s };
            K_AUDIO_HARDWARE_NO_ERROR
        }
        None => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

unsafe extern "C" fn plugin_get_property_data(
    _driver: *mut c_void,
    object_id: AudioObjectID,
    _client_process_id: i32,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: UInt32,
    _qualifier_data: *const c_void,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    if address.is_null() || out_data_size.is_null() || data.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: `address` is non-null, provided by the host.
    let addr = unsafe { &*address };

    // SAFETY: all pointer arguments have been validated above; callee contracts
    // are satisfied by the host-provided buffer.
    unsafe {
        match classify_object(object_id) {
            Some(ObjectType::Plugin) => {
                plugin_get_property(addr.m_selector, data_size, out_data_size, data)
            }
            Some(ObjectType::Device) => {
                device_get_property(object_id, addr, data_size, out_data_size, data)
            }
            Some(ObjectType::Stream) => {
                stream_get_property(object_id, addr, data_size, out_data_size, data)
            }
            None => K_AUDIO_HARDWARE_BAD_OBJECT_ERROR,
        }
    }
}

unsafe extern "C" fn plugin_set_property_data(
    _driver: *mut c_void,
    object_id: AudioObjectID,
    _client_process_id: i32,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: UInt32,
    _qualifier_data: *const c_void,
    data_size: UInt32,
    data: *const c_void,
) -> OSStatus {
    if address.is_null() || data.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: `address` is non-null, provided by the host.
    let addr = unsafe { &*address };

    match classify_object(object_id) {
        Some(ObjectType::Plugin) => {
            if addr.m_selector == K_MARS_PROPERTY_DESIRED_STATE {
                // SAFETY: `data` is non-null (checked above) and points to `data_size` bytes.
                unsafe { set_desired_state_from_raw(data, data_size) }
            } else {
                K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR
            }
        }
        _ => K_AUDIO_HARDWARE_UNSUPPORTED_OPERATION_ERROR,
    }
}

// ===========================================================================
// IO operations
// ===========================================================================

unsafe extern "C" fn plugin_start_io(
    _driver: *mut c_void,
    device_object_id: AudioObjectID,
) -> OSStatus {
    // Read device info, then drop the registry lock before acquiring DRIVER_STATE
    // to maintain consistent lock ordering (DRIVER_STATE → object_registry) and
    // avoid deadlock with `sync_object_registry`.
    let (uid, channels, is_input) = {
        let reg = PLUGIN.object_registry.lock();
        let Some(dev) = reg.find_device_by_object(device_object_id) else {
            return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR;
        };
        (dev.uid.clone(), dev.channels, dev.kind.contains("input"))
    };

    let spec = {
        let state = DRIVER_STATE.lock();
        RingSpec {
            sample_rate: state.applied_state.sample_rate,
            channels,
            capacity_frames: state.applied_state.buffer_frames.saturating_mul(8),
        }
    };

    let direction = if is_input {
        StreamDirection::Vin
    } else {
        StreamDirection::Vout
    };
    let name = stream_name(direction, &uid);

    if global_registry().create_or_open(&name, spec).is_err() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // Re-acquire registry to set io_running.
    let mut reg = PLUGIN.object_registry.lock();
    if let Some(dev) = reg.find_device_by_object_mut(device_object_id) {
        dev.io_running = true;
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_stop_io(
    _driver: *mut c_void,
    device_object_id: AudioObjectID,
) -> OSStatus {
    let mut reg = PLUGIN.object_registry.lock();
    if let Some(dev) = reg.find_device_by_object_mut(device_object_id) {
        dev.io_running = false;
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_get_zero_time_stamp(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    out_sample_time: *mut Float64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    if out_sample_time.is_null() || out_host_time.is_null() || out_seed.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // Use mach_absolute_time for host time.
    // SAFETY: mach_absolute_time is always safe to call on macOS.
    let host_time = unsafe { mach_absolute_time() };
    // SAFETY: all output pointers verified non-null above.
    unsafe {
        *out_sample_time = 0.0;
        *out_host_time = host_time;
        *out_seed = 1;
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_will_do_io_operation(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    operation_id: UInt32,
    will_do: *mut Boolean,
    is_input: *mut Boolean,
) -> OSStatus {
    if will_do.is_null() || is_input.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: output pointers are non-null.
    unsafe {
        *will_do = if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX
            || operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_READ_INPUT
        {
            1
        } else {
            0
        };
        *is_input = if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_READ_INPUT {
            1
        } else {
            0
        };
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_begin_io_operation(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    _operation_id: UInt32,
    _io_buffer_frame_size: UInt32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_do_io_operation(
    _driver: *mut c_void,
    device_object_id: AudioObjectID,
    _stream_object_id: AudioObjectID,
    _client_id: UInt32,
    operation_id: UInt32,
    io_buffer_frame_size: UInt32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
    io_main_buffer: *mut c_void,
    _io_secondary_buffer: *mut c_void,
) -> OSStatus {
    let reg = PLUGIN.object_registry.lock();
    let Some(dev) = reg.find_device_by_object(device_object_id) else {
        return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR;
    };
    let uid = dev.uid.clone();
    let channels = dev.channels as usize;
    let is_input = dev.kind.contains("input");
    drop(reg);

    let direction = if is_input {
        StreamDirection::Vin
    } else {
        StreamDirection::Vout
    };
    let name = stream_name(direction, &uid);

    let total_samples = io_buffer_frame_size as usize * channels;
    // SAFETY: `io_main_buffer` points to `total_samples` f32 samples provided by the host.
    let buffer: &mut [f32] =
        unsafe { core::slice::from_raw_parts_mut(io_main_buffer.cast::<f32>(), total_samples) };

    let mut underrun_delta = 0_u64;
    let mut overrun_delta = 0_u64;
    let mut xrun_delta = 0_u64;

    if let Some(ring_handle) = global_registry().open(&name) {
        if let Some(mut ring) = ring_handle.try_lock() {
            let before = ring.header().ok();
            if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX {
                // VOut: host wrote mixed audio into buffer -> push to SHM ring.
                if ring.write_interleaved(buffer).is_err() {
                    overrun_delta = overrun_delta.saturating_add(1);
                    xrun_delta = xrun_delta.saturating_add(1);
                }
            } else if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_READ_INPUT {
                // VIn: pull audio from SHM ring -> host reads from buffer.
                if ring.read_interleaved(buffer).is_err() {
                    buffer.fill(0.0);
                    underrun_delta = underrun_delta.saturating_add(1);
                    xrun_delta = xrun_delta.saturating_add(1);
                }
            }
            let after = ring.header().ok();
            if let (Some(before), Some(after)) = (before, after) {
                let overrun = after.overrun_count.saturating_sub(before.overrun_count);
                let underrun = after.underrun_count.saturating_sub(before.underrun_count);
                overrun_delta = overrun_delta.saturating_add(overrun);
                underrun_delta = underrun_delta.saturating_add(underrun);
                xrun_delta = xrun_delta.saturating_add(overrun.saturating_add(underrun));
            }
        } else if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX {
            // Non-blocking policy: drop current frame on contention.
            overrun_delta = overrun_delta.saturating_add(1);
            xrun_delta = xrun_delta.saturating_add(1);
        } else if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_READ_INPUT {
            // Non-blocking policy: return silence on contention.
            buffer.fill(0.0);
            underrun_delta = underrun_delta.saturating_add(1);
            xrun_delta = xrun_delta.saturating_add(1);
        }
    } else if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX {
        // Ring unavailable: drop current frame.
        overrun_delta = overrun_delta.saturating_add(1);
        xrun_delta = xrun_delta.saturating_add(1);
    } else if operation_id == K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_READ_INPUT {
        // Ring unavailable: return silence.
        buffer.fill(0.0);
        underrun_delta = underrun_delta.saturating_add(1);
        xrun_delta = xrun_delta.saturating_add(1);
    }

    {
        let mut state = DRIVER_STATE.lock();
        state.runtime.underrun_count = state.runtime.underrun_count.saturating_add(underrun_delta);
        state.runtime.overrun_count = state.runtime.overrun_count.saturating_add(overrun_delta);
        state.runtime.xrun_count = state.runtime.xrun_count.saturating_add(xrun_delta);
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        state.runtime.last_callback_ns = now_ns;
    }

    K_AUDIO_HARDWARE_NO_ERROR
}

unsafe extern "C" fn plugin_end_io_operation(
    _driver: *mut c_void,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    _operation_id: UInt32,
    _io_buffer_frame_size: UInt32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    K_AUDIO_HARDWARE_NO_ERROR
}

// ===========================================================================
// Property data — Plugin object
// ===========================================================================

fn plugin_property_data_size(selector: UInt32) -> Option<UInt32> {
    Some(match selector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS | K_AUDIO_OBJECT_PROPERTY_CLASS => {
            size_of::<UInt32>() as UInt32
        }
        K_AUDIO_OBJECT_PROPERTY_OWNER => size_of::<AudioObjectID>() as UInt32,
        K_AUDIO_PLUG_IN_PROPERTY_BUNDLE_ID | K_AUDIO_PLUG_IN_PROPERTY_RESOURCE_BUNDLE => {
            size_of::<*const c_void>() as UInt32 // CFStringRef
        }
        K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_PLUG_IN_PROPERTY_DEVICE_LIST => {
            let reg = PLUGIN.object_registry.lock();
            (reg.devices.len() * size_of::<AudioObjectID>()) as UInt32
        }
        K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST => {
            (4 * size_of::<AudioServerPlugInCustomPropertyInfo>()) as UInt32
        }
        K_MARS_PROPERTY_DESIRED_STATE
        | K_MARS_PROPERTY_APPLIED_STATE
        | K_MARS_PROPERTY_RUNTIME_STATS
        | K_MARS_PROPERTY_CONFIG_SUMMARY => {
            // Return a generous upper bound; actual size is written on GetPropertyData.
            64 * 1024
        }
        _ => return None,
    })
}

/// Write plugin property data into the host-provided buffer.
///
/// # Safety
/// `data` must point to a writable buffer of at least `data_size` bytes.
/// `out_data_size` must be a valid pointer.
unsafe fn plugin_get_property(
    selector: UInt32,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    match selector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS => unsafe {
            write_val::<UInt32>(K_AUDIO_OBJECT_CLASS_ID, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_CLASS => unsafe {
            write_val::<UInt32>(K_AUDIO_PLUG_IN_CLASS_ID, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_OWNER => unsafe {
            write_val::<AudioObjectID>(K_AUDIO_OBJECT_SYSTEM_OBJECT, data_size, out_data_size, data)
        },
        K_AUDIO_PLUG_IN_PROPERTY_BUNDLE_ID => unsafe {
            write_cfstring(MARS_DRIVER_BUNDLE_ID, data_size, out_data_size, data)
        },
        K_AUDIO_PLUG_IN_PROPERTY_RESOURCE_BUNDLE => unsafe {
            write_cfstring("", data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_PLUG_IN_PROPERTY_DEVICE_LIST => {
            let reg = PLUGIN.object_registry.lock();
            let ids = reg.all_device_ids();
            let byte_len = ids.len() * size_of::<AudioObjectID>();
            if (data_size as usize) < byte_len {
                return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
            }
            // SAFETY: buffer bounds checked above, ids is a contiguous slice.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    ids.as_ptr(),
                    data.cast::<AudioObjectID>(),
                    ids.len(),
                );
                *out_data_size = byte_len as UInt32;
            }
            K_AUDIO_HARDWARE_NO_ERROR
        }
        K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST => {
            let infos = [
                AudioServerPlugInCustomPropertyInfo {
                    m_selector: K_MARS_PROPERTY_DESIRED_STATE,
                    m_property_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_CFDATA,
                    m_qualifier_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_NONE,
                },
                AudioServerPlugInCustomPropertyInfo {
                    m_selector: K_MARS_PROPERTY_APPLIED_STATE,
                    m_property_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_CFDATA,
                    m_qualifier_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_NONE,
                },
                AudioServerPlugInCustomPropertyInfo {
                    m_selector: K_MARS_PROPERTY_RUNTIME_STATS,
                    m_property_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_CFDATA,
                    m_qualifier_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_NONE,
                },
                AudioServerPlugInCustomPropertyInfo {
                    m_selector: K_MARS_PROPERTY_CONFIG_SUMMARY,
                    m_property_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_CFDATA,
                    m_qualifier_data_type: K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_NONE,
                },
            ];
            let byte_len = size_of_val(&infos);
            if (data_size as usize) < byte_len {
                return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
            }
            // SAFETY: buffer is large enough, infos is repr(C) with known layout.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    infos.as_ptr().cast::<u8>(),
                    data.cast::<u8>(),
                    byte_len,
                );
                *out_data_size = byte_len as UInt32;
            }
            K_AUDIO_HARDWARE_NO_ERROR
        }
        K_MARS_PROPERTY_APPLIED_STATE => {
            let json = match applied_state_json() {
                Ok(j) => j,
                Err(_) => return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
            };
            // SAFETY: caller guarantees buffer is writable for data_size bytes.
            unsafe { write_json_as_cfdata(&json, data_size, out_data_size, data) }
        }
        K_MARS_PROPERTY_RUNTIME_STATS => {
            let json = match runtime_stats_json() {
                Ok(j) => j,
                Err(_) => return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
            };
            unsafe { write_json_as_cfdata(&json, data_size, out_data_size, data) }
        }
        K_MARS_PROPERTY_CONFIG_SUMMARY => {
            let json = match configuration_summary_json() {
                Ok(j) => j,
                Err(_) => return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
            };
            unsafe { write_json_as_cfdata(&json, data_size, out_data_size, data) }
        }
        K_MARS_PROPERTY_DESIRED_STATE => {
            let state = DRIVER_STATE.lock();
            let json = match &state.desired_state {
                Some(ds) => match serde_json::to_string(ds) {
                    Ok(j) => j,
                    Err(_) => return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
                },
                None => "null".to_string(),
            };
            drop(state);
            unsafe { write_json_as_cfdata(&json, data_size, out_data_size, data) }
        }
        _ => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

// ===========================================================================
// Property data — Device object
// ===========================================================================

fn device_property_data_size(object_id: AudioObjectID, selector: UInt32) -> Option<UInt32> {
    Some(match selector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS | K_AUDIO_OBJECT_PROPERTY_CLASS => {
            size_of::<UInt32>() as UInt32
        }
        K_AUDIO_OBJECT_PROPERTY_OWNER => size_of::<AudioObjectID>() as UInt32,
        K_AUDIO_OBJECT_PROPERTY_NAME
        | K_AUDIO_OBJECT_PROPERTY_MANUFACTURER
        | K_AUDIO_DEVICE_PROPERTY_DEVICE_UID
        | K_AUDIO_DEVICE_PROPERTY_MODEL_UID => {
            size_of::<*const c_void>() as UInt32 // CFStringRef
        }
        K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE
        | K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_DEVICE
        | K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_SYSTEM_DEVICE
        | K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_HIDDEN
        | K_AUDIO_DEVICE_PROPERTY_LATENCY
        | K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET
        | K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN
        | K_AUDIO_DEVICE_PROPERTY_IS_ALIVE
        | K_AUDIO_DEVICE_PROPERTY_IS_RUNNING
        | K_AUDIO_DEVICE_PROPERTY_ZERO_TIME_STAMP_PERIOD => size_of::<UInt32>() as UInt32,
        K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE => size_of::<Float64>() as UInt32,
        K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES => {
            size_of::<AudioValueRange>() as UInt32
        }
        K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_DEVICE_PROPERTY_STREAMS => {
            let reg = PLUGIN.object_registry.lock();
            if reg.find_device_by_object(object_id).is_some() {
                size_of::<AudioObjectID>() as UInt32
            } else {
                0
            }
        }
        K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO => {
            (2 * size_of::<UInt32>()) as UInt32
        }
        _ => return None,
    })
}

/// Write device property data.
///
/// # Safety
/// `data` must be writable for `data_size` bytes.
unsafe fn device_get_property(
    object_id: AudioObjectID,
    addr: &AudioObjectPropertyAddress,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    let reg = PLUGIN.object_registry.lock();
    let Some(dev) = reg.find_device_by_object(object_id) else {
        return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR;
    };
    let dev = dev.clone();
    let stream_id = dev.stream_id;
    drop(reg);

    let state = DRIVER_STATE.lock();
    let sample_rate = state.applied_state.sample_rate as f64;
    let buffer_frames = state.applied_state.buffer_frames;
    drop(state);

    match addr.m_selector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS => unsafe {
            write_val::<UInt32>(K_AUDIO_OBJECT_CLASS_ID, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_CLASS => unsafe {
            write_val::<UInt32>(K_AUDIO_DEVICE_CLASS_ID, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_OWNER => unsafe {
            write_val::<AudioObjectID>(K_AUDIO_OBJECT_PLUGIN_OBJECT, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_NAME => unsafe {
            write_cfstring(&dev.name, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_MANUFACTURER => unsafe {
            write_cfstring("MARS", data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_DEVICE_UID => unsafe {
            write_cfstring(&dev.uid, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_MODEL_UID => unsafe {
            write_cfstring("MarsVirtualDevice", data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE => unsafe {
            write_val::<UInt32>(
                K_AUDIO_TRANSPORT_TYPE_VIRTUAL,
                data_size,
                out_data_size,
                data,
            )
        },
        K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_DEVICE
        | K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_SYSTEM_DEVICE => unsafe {
            write_val::<UInt32>(1, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_HIDDEN => unsafe {
            write_val::<UInt32>(
                if dev.hidden { 1 } else { 0 },
                data_size,
                out_data_size,
                data,
            )
        },
        K_AUDIO_DEVICE_PROPERTY_LATENCY | K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET => unsafe {
            write_val::<UInt32>(0, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN => unsafe {
            write_val::<UInt32>(0, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_IS_ALIVE => unsafe {
            write_val::<UInt32>(1, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_IS_RUNNING => unsafe {
            write_val::<UInt32>(
                if dev.io_running { 1 } else { 0 },
                data_size,
                out_data_size,
                data,
            )
        },
        K_AUDIO_DEVICE_PROPERTY_ZERO_TIME_STAMP_PERIOD => unsafe {
            write_val::<UInt32>(buffer_frames, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE => unsafe {
            write_val::<Float64>(sample_rate, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES => {
            let range = AudioValueRange {
                m_minimum: sample_rate,
                m_maximum: sample_rate,
            };
            unsafe { write_val::<AudioValueRange>(range, data_size, out_data_size, data) }
        }
        K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_DEVICE_PROPERTY_STREAMS => unsafe {
            write_val::<AudioObjectID>(stream_id, data_size, out_data_size, data)
        },
        K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO => {
            let channels: [UInt32; 2] = [1, 2];
            let byte_len = size_of_val(&channels);
            if (data_size as usize) < byte_len {
                return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
            }
            // SAFETY: buffer large enough, writing two UInt32s.
            unsafe {
                core::ptr::copy_nonoverlapping(channels.as_ptr(), data.cast::<UInt32>(), 2);
                *out_data_size = byte_len as UInt32;
            }
            K_AUDIO_HARDWARE_NO_ERROR
        }
        _ => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

// ===========================================================================
// Property data — Stream object
// ===========================================================================

fn stream_property_data_size(selector: UInt32) -> Option<UInt32> {
    Some(match selector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS | K_AUDIO_OBJECT_PROPERTY_CLASS => {
            size_of::<UInt32>() as UInt32
        }
        K_AUDIO_OBJECT_PROPERTY_OWNER => size_of::<AudioObjectID>() as UInt32,
        K_AUDIO_STREAM_PROPERTY_DIRECTION
        | K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE
        | K_AUDIO_STREAM_PROPERTY_START_CHANNEL
        | K_AUDIO_STREAM_PROPERTY_LATENCY
        | K_AUDIO_STREAM_PROPERTY_IS_ACTIVE => size_of::<UInt32>() as UInt32,
        K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT | K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT => {
            size_of::<AudioStreamBasicDescription>() as UInt32
        }
        K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS
        | K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS => {
            size_of::<AudioStreamRangedDescription>() as UInt32
        }
        _ => return None,
    })
}

/// Write stream property data.
///
/// # Safety
/// `data` must be writable for `data_size` bytes.
unsafe fn stream_get_property(
    object_id: AudioObjectID,
    addr: &AudioObjectPropertyAddress,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    let reg = PLUGIN.object_registry.lock();
    let Some(dev) = reg.find_device_by_stream(object_id) else {
        return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR;
    };
    let dev = dev.clone();
    drop(reg);

    let state = DRIVER_STATE.lock();
    let sample_rate = state.applied_state.sample_rate as f64;
    drop(state);

    let channels = dev.channels as u32;
    let is_input = dev.kind.contains("input");

    match addr.m_selector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS => unsafe {
            write_val::<UInt32>(K_AUDIO_OBJECT_CLASS_ID, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_CLASS => unsafe {
            write_val::<UInt32>(K_AUDIO_STREAM_CLASS_ID, data_size, out_data_size, data)
        },
        K_AUDIO_OBJECT_PROPERTY_OWNER => unsafe {
            write_val::<AudioObjectID>(dev.device_id, data_size, out_data_size, data)
        },
        K_AUDIO_STREAM_PROPERTY_DIRECTION => {
            let dir: UInt32 = if is_input { 1 } else { 0 };
            unsafe { write_val::<UInt32>(dir, data_size, out_data_size, data) }
        }
        K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE => {
            let term = if is_input {
                K_INPUT_TERMINAL
            } else {
                K_OUTPUT_TERMINAL
            };
            unsafe { write_val::<UInt32>(term, data_size, out_data_size, data) }
        }
        K_AUDIO_STREAM_PROPERTY_START_CHANNEL => unsafe {
            write_val::<UInt32>(1, data_size, out_data_size, data)
        },
        K_AUDIO_STREAM_PROPERTY_LATENCY => unsafe {
            write_val::<UInt32>(0, data_size, out_data_size, data)
        },
        K_AUDIO_STREAM_PROPERTY_IS_ACTIVE => unsafe {
            write_val::<UInt32>(1, data_size, out_data_size, data)
        },
        K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT | K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT => {
            let asbd = AudioStreamBasicDescription::float32_stereo(sample_rate, channels);
            unsafe {
                write_val::<AudioStreamBasicDescription>(asbd, data_size, out_data_size, data)
            }
        }
        K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS
        | K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS => {
            let asbd = AudioStreamBasicDescription::float32_stereo(sample_rate, channels);
            let ranged = AudioStreamRangedDescription {
                m_format: asbd,
                m_sample_rate_range: AudioValueRange {
                    m_minimum: sample_rate,
                    m_maximum: sample_rate,
                },
            };
            unsafe {
                write_val::<AudioStreamRangedDescription>(ranged, data_size, out_data_size, data)
            }
        }
        _ => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

// ===========================================================================
// SetPropertyData — desired state
// ===========================================================================

/// Parse a CFDataRef from the host buffer and stage the desired state.
///
/// The host passes the property value as a pointer to a CFDataRef (the custom
/// property data type is `kAudioServerPlugInCustomPropertyDataTypeCFData`).
///
/// # Safety
/// `data` must point to at least `data_size` readable bytes containing a CFDataRef.
unsafe fn set_desired_state_from_raw(data: *const c_void, data_size: UInt32) -> OSStatus {
    // `data` points to a CFDataRef value (a pointer).
    if (data_size as usize) < size_of::<*const c_void>() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // SAFETY: `data` is non-null (checked by caller) and large enough for a pointer.
    let cf_data: *const c_void = unsafe { core::ptr::read_unaligned(data.cast::<*const c_void>()) };
    if cf_data.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // SAFETY: `cf_data` is a valid CFDataRef provided by the host via SetPropertyData.
    let byte_ptr = unsafe { CFDataGetBytePtr(cf_data) };
    let byte_len = unsafe { CFDataGetLength(cf_data) };
    if byte_ptr.is_null() || byte_len <= 0 {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // SAFETY: CFDataGetBytePtr returns a pointer to `byte_len` contiguous bytes.
    let bytes = unsafe { core::slice::from_raw_parts(byte_ptr, byte_len as usize) };
    let json_str = match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
    };

    if set_desired_state_json(json_str).is_err() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    // Request configuration change and ask host to call PerformDeviceConfigurationChange.
    let generation = match request_device_configuration_change() {
        Ok(g) => g,
        Err(_) => return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
    };

    let host_guard = PLUGIN.host.lock();
    if let Some(host) = *host_guard {
        // SAFETY: `host` is the AudioServerPlugInHostRef provided by coreaudiod.
        let status = unsafe {
            ((*host).request_device_configuration_change)(
                host,
                K_AUDIO_OBJECT_PLUGIN_OBJECT,
                generation,
                core::ptr::null(),
            )
        };
        drop(host_guard);
        if status != K_AUDIO_HARDWARE_NO_ERROR {
            return status;
        }
    } else {
        drop(host_guard);
        // No host — we're probably in test mode.  Perform directly.
        let _ = perform_device_configuration_change(generation);
        sync_object_registry();
    }

    K_AUDIO_HARDWARE_NO_ERROR
}

// ===========================================================================
// Object registry sync
// ===========================================================================

fn sync_object_registry() {
    let state = DRIVER_STATE.lock();
    let applied = &state.applied_state;
    let desired_uids: std::collections::BTreeSet<&str> =
        applied.devices.iter().map(|d| d.uid.as_str()).collect();

    let mut reg = PLUGIN.object_registry.lock();

    // Remove devices no longer in applied state.
    let to_remove: Vec<String> = reg
        .devices
        .keys()
        .filter(|uid| !desired_uids.contains(uid.as_str()))
        .cloned()
        .collect();
    for uid in to_remove {
        reg.devices.remove(&uid);
        let _ = global_registry().remove(&stream_name(StreamDirection::Vout, &uid));
        let _ = global_registry().remove(&stream_name(StreamDirection::Vin, &uid));
    }
    if applied.devices.is_empty() {
        let _ = global_registry().remove_namespace("mars.");
    }

    // Add new devices and refresh metadata for existing ones.
    for device in &applied.devices {
        if !reg.devices.contains_key(&device.uid) {
            let device_id = reg.allocate_id();
            let stream_id = reg.allocate_id();
            reg.devices.insert(
                device.uid.clone(),
                DeviceObjectInfo {
                    device_id,
                    stream_id,
                    uid: device.uid.clone(),
                    name: device.name.clone(),
                    kind: device.kind.clone(),
                    channels: device.channels,
                    hidden: device.hidden,
                    io_running: false,
                },
            );
        } else if let Some(existing) = reg.devices.get_mut(&device.uid) {
            existing.name = device.name.clone();
            existing.kind = device.kind.clone();
            existing.channels = device.channels;
            existing.hidden = device.hidden;
        }
    }
}

fn notify_device_list_changed() {
    let host_guard = PLUGIN.host.lock();
    if let Some(host) = *host_guard {
        let addr = AudioObjectPropertyAddress {
            m_selector: K_AUDIO_PLUG_IN_PROPERTY_DEVICE_LIST,
            m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        // SAFETY: `host` is valid AudioServerPlugInHostRef from coreaudiod.
        unsafe {
            ((*host).properties_changed)(host, K_AUDIO_OBJECT_PLUGIN_OBJECT, 1, &addr);
        }
    }
}

// ===========================================================================
// Helpers — write typed values into host buffers
// ===========================================================================

/// Write a `Copy` value into the host buffer.
///
/// # Safety
/// `data` must be writable for at least `data_size` bytes.
/// `out_data_size` must be a valid pointer.
unsafe fn write_val<T: Copy>(
    val: T,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    let needed = size_of::<T>();
    if (data_size as usize) < needed {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: buffer bounds checked.
    unsafe {
        core::ptr::write_unaligned(data.cast::<T>(), val);
        *out_data_size = needed as UInt32;
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

/// Write a Rust `&str` as a CFStringRef into the host buffer.
///
/// # Safety
/// `data` must be writable for at least `data_size` bytes (enough for a pointer).
/// `out_data_size` must be a valid pointer.
unsafe fn write_cfstring(
    s: &str,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    let needed = size_of::<*const c_void>();
    if (data_size as usize) < needed {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    let cf_str = cfstring_create(s);
    // SAFETY: buffer bounds checked; writing a pointer value.
    unsafe {
        core::ptr::write_unaligned(data.cast::<*const c_void>(), cf_str);
        *out_data_size = needed as UInt32;
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

/// Write JSON bytes as a CFDataRef into the host buffer.
///
/// # Safety
/// Same as `write_cfstring`.
unsafe fn write_json_as_cfdata(
    json: &str,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    data: *mut c_void,
) -> OSStatus {
    let needed = size_of::<*const c_void>();
    if (data_size as usize) < needed {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }

    let cf_data = cfdata_create(json.as_bytes());
    // SAFETY: buffer bounds checked.
    unsafe {
        core::ptr::write_unaligned(data.cast::<*const c_void>(), cf_data);
        *out_data_size = needed as UInt32;
    }
    K_AUDIO_HARDWARE_NO_ERROR
}

// ===========================================================================
// CoreFoundation helpers (linked via build.rs)
// ===========================================================================

unsafe extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> *const c_void;

    fn CFDataCreate(alloc: *const c_void, bytes: *const u8, length: isize) -> *const c_void;

    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;

    fn CFDataGetLength(data: *const c_void) -> isize;

    fn mach_absolute_time() -> u64;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

fn cfstring_create(s: &str) -> *const c_void {
    // SAFETY: FFI call to CoreFoundation with valid UTF-8 bytes.
    unsafe {
        CFStringCreateWithBytes(
            core::ptr::null(),
            s.as_ptr(),
            s.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    }
}

fn cfdata_create(bytes: &[u8]) -> *const c_void {
    // SAFETY: FFI call to CoreFoundation with valid byte slice.
    unsafe { CFDataCreate(core::ptr::null(), bytes.as_ptr(), bytes.len() as isize) }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::ffi::c_void;

    use once_cell::sync::Lazy;
    use parking_lot::Mutex;

    use super::*;

    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn factory_returns_nonnull() {
        // SAFETY: test-only, no host.
        let ptr = unsafe { MarsAudioServerPlugInFactory(core::ptr::null(), core::ptr::null()) };
        assert!(!ptr.is_null());
    }

    #[test]
    fn classify_plugin_object() {
        assert_eq!(
            classify_object(K_AUDIO_OBJECT_PLUGIN_OBJECT),
            Some(ObjectType::Plugin)
        );
    }

    #[test]
    fn object_registry_allocate() {
        let mut reg = ObjectRegistry::default();
        let id1 = reg.allocate_id();
        let id2 = reg.allocate_id();
        assert_eq!(id1, 2);
        assert_eq!(id2, 3);
    }

    #[test]
    fn device_is_running_property_reflects_io_state() {
        let _guard = TEST_LOCK.lock();
        let uid = "test.running.uid".to_string();
        let device_id = {
            let mut reg = PLUGIN.object_registry.lock();
            let device_id = reg.allocate_id();
            let stream_id = reg.allocate_id();
            reg.devices.insert(
                uid.clone(),
                DeviceObjectInfo {
                    device_id,
                    stream_id,
                    uid: uid.clone(),
                    name: "Test Device".to_string(),
                    kind: "virtual_output".to_string(),
                    channels: 2,
                    hidden: false,
                    io_running: false,
                },
            );
            device_id
        };

        let address = AudioObjectPropertyAddress {
            m_selector: K_AUDIO_DEVICE_PROPERTY_IS_RUNNING,
            m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut out_size = 0_u32;
        let mut running_value = 9_u32;
        // SAFETY: all pointers are valid and sized for a UInt32 write.
        let status = unsafe {
            device_get_property(
                device_id,
                &address,
                std::mem::size_of::<UInt32>() as UInt32,
                &mut out_size,
                (&mut running_value as *mut UInt32).cast::<c_void>(),
            )
        };
        assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
        assert_eq!(out_size, std::mem::size_of::<UInt32>() as UInt32);
        assert_eq!(running_value, 0);

        {
            let mut reg = PLUGIN.object_registry.lock();
            let device = reg.devices.get_mut(&uid);
            assert!(device.is_some(), "device inserted in registry");
            if let Some(device) = device {
                device.io_running = true;
            }
        }

        out_size = 0;
        running_value = 9;
        // SAFETY: all pointers are valid and sized for a UInt32 write.
        let status = unsafe {
            device_get_property(
                device_id,
                &address,
                std::mem::size_of::<UInt32>() as UInt32,
                &mut out_size,
                (&mut running_value as *mut UInt32).cast::<c_void>(),
            )
        };
        assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
        assert_eq!(out_size, std::mem::size_of::<UInt32>() as UInt32);
        assert_eq!(running_value, 1);

        let mut reg = PLUGIN.object_registry.lock();
        reg.devices.remove(&uid);
    }
}
