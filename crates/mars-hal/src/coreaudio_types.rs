//! CoreAudio C type bindings shared between the plugin (driver side) and mars-hal-client.
//!
//! All types are `#[repr(C)]` to match Apple's AudioServerPlugIn.h layout.

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// Scalar types
// ---------------------------------------------------------------------------

pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type UInt32 = u32;
pub type UInt8 = u8;
pub type Float32 = f32;
pub type Float64 = f64;
pub type Boolean = u8;
pub type HRESULT = i32;
pub type ULONG = u32;
pub type CFStringRef = *const c_void;
pub type CFPropertyListRef = *const c_void;
pub type CFDictionaryRef = *const c_void;

// ---------------------------------------------------------------------------
// OSStatus constants
// ---------------------------------------------------------------------------

pub const K_NO_ERR: OSStatus = 0;
pub const K_AUDIO_HARDWARE_NO_ERROR: OSStatus = 0;
pub const K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR: OSStatus = 0x77686F3F; // 'who?'
pub const K_AUDIO_HARDWARE_BAD_OBJECT_ERROR: OSStatus = 0x216F626A; // '!obj'
pub const K_AUDIO_HARDWARE_BAD_DEVICE_ERROR: OSStatus = 0x21646576; // '!dev'
pub const K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR: OSStatus = 0x6E6F7065; // 'nope'
pub const K_AUDIO_HARDWARE_UNSUPPORTED_OPERATION_ERROR: OSStatus = 0x756E6F70; // 'unop'

// COM HRESULT values
pub const S_OK: HRESULT = 0;
pub const E_NOINTERFACE: HRESULT = 0x8000_0004_u32 as i32;

// ---------------------------------------------------------------------------
// FourCC helper
// ---------------------------------------------------------------------------

pub const fn fourcc(bytes: &[u8; 4]) -> u32 {
    ((bytes[0] as u32) << 24)
        | ((bytes[1] as u32) << 16)
        | ((bytes[2] as u32) << 8)
        | (bytes[3] as u32)
}

// ---------------------------------------------------------------------------
// Object class IDs (FourCC)
// ---------------------------------------------------------------------------

pub const K_AUDIO_OBJECT_CLASS_ID: UInt32 = fourcc(b"aobj");
pub const K_AUDIO_PLUG_IN_CLASS_ID: UInt32 = fourcc(b"aplg");
pub const K_AUDIO_DEVICE_CLASS_ID: UInt32 = fourcc(b"adev");
pub const K_AUDIO_STREAM_CLASS_ID: UInt32 = fourcc(b"astr");
pub const K_AUDIO_CONTROL_CLASS_ID: UInt32 = fourcc(b"actl");
pub const K_AUDIO_LEVEL_CONTROL_CLASS_ID: UInt32 = fourcc(b"levl");
pub const K_AUDIO_VOLUME_CONTROL_CLASS_ID: UInt32 = fourcc(b"vlme");
pub const K_AUDIO_TRANSPORT_TYPE_VIRTUAL: UInt32 = fourcc(b"virt");

// ---------------------------------------------------------------------------
// Well-known AudioObjectIDs
// ---------------------------------------------------------------------------

pub const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
pub const K_AUDIO_OBJECT_PLUGIN_OBJECT: AudioObjectID = 1; // plugin's own object ID

// ---------------------------------------------------------------------------
// Scope / Element
// ---------------------------------------------------------------------------

pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: UInt32 = fourcc(b"glob");
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT: UInt32 = fourcc(b"inpt");
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: UInt32 = fourcc(b"outp");
pub const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: UInt32 = 0;

// ---------------------------------------------------------------------------
// Standard property selectors — Plugin
// ---------------------------------------------------------------------------

pub const K_AUDIO_OBJECT_PROPERTY_BASE_CLASS: UInt32 = fourcc(b"bcls");
pub const K_AUDIO_OBJECT_PROPERTY_CLASS: UInt32 = fourcc(b"clas");
pub const K_AUDIO_OBJECT_PROPERTY_OWNER: UInt32 = fourcc(b"stdw");
pub const K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS: UInt32 = fourcc(b"ownd");
pub const K_AUDIO_OBJECT_PROPERTY_CONTROL_LIST: UInt32 = fourcc(b"ctrl");
pub const K_AUDIO_OBJECT_PROPERTY_NAME: UInt32 = fourcc(b"lnam");
pub const K_AUDIO_OBJECT_PROPERTY_MANUFACTURER: UInt32 = fourcc(b"lmak");
pub const K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST: UInt32 = fourcc(b"cust");
pub const K_AUDIO_CONTROL_PROPERTY_SCOPE: UInt32 = fourcc(b"cscp");
pub const K_AUDIO_CONTROL_PROPERTY_ELEMENT: UInt32 = fourcc(b"celm");
pub const K_AUDIO_LEVEL_CONTROL_PROPERTY_SCALAR_VALUE: UInt32 = fourcc(b"lcsv");
pub const K_AUDIO_LEVEL_CONTROL_PROPERTY_DECIBEL_VALUE: UInt32 = fourcc(b"lcdv");
pub const K_AUDIO_LEVEL_CONTROL_PROPERTY_DECIBEL_RANGE: UInt32 = fourcc(b"lcdr");
pub const K_AUDIO_LEVEL_CONTROL_PROPERTY_CONVERT_SCALAR_TO_DECIBELS: UInt32 = fourcc(b"lcsd");
pub const K_AUDIO_LEVEL_CONTROL_PROPERTY_CONVERT_DECIBELS_TO_SCALAR: UInt32 = fourcc(b"lcds");

pub const K_AUDIO_PLUG_IN_PROPERTY_BUNDLE_ID: UInt32 = fourcc(b"piid");
pub const K_AUDIO_PLUG_IN_PROPERTY_DEVICE_LIST: UInt32 = fourcc(b"dev#");
pub const K_AUDIO_PLUG_IN_PROPERTY_RESOURCE_BUNDLE: UInt32 = fourcc(b"rsrc");

// ---------------------------------------------------------------------------
// Standard property selectors — Device
// ---------------------------------------------------------------------------

pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_UID: UInt32 = fourcc(b"uid ");
pub const K_AUDIO_DEVICE_PROPERTY_MODEL_UID: UInt32 = fourcc(b"muid");
pub const K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE: UInt32 = fourcc(b"tran");
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_NAME_CF_STRING: UInt32 = fourcc(b"lnam");
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_DEVICE: UInt32 = fourcc(b"dflt");
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_CAN_BE_DEFAULT_SYSTEM_DEVICE: UInt32 = fourcc(b"sflt");
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_HIDDEN: UInt32 = fourcc(b"hidn");
pub const K_AUDIO_DEVICE_PROPERTY_LATENCY: UInt32 = fourcc(b"ltnc");
pub const K_AUDIO_DEVICE_PROPERTY_STREAMS: UInt32 = fourcc(b"stm#");
pub const K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE: UInt32 = fourcc(b"nsrt");
pub const K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES: UInt32 = fourcc(b"nsr#");
pub const K_AUDIO_DEVICE_PROPERTY_ZERO_TIME_STAMP_PERIOD: UInt32 = fourcc(b"ring");
pub const K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET: UInt32 = fourcc(b"saft");
pub const K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN: UInt32 = fourcc(b"clkd");
pub const K_AUDIO_DEVICE_PROPERTY_IS_ALIVE: UInt32 = fourcc(b"livn");
pub const K_AUDIO_DEVICE_PROPERTY_IS_RUNNING: UInt32 = fourcc(b"goin");
pub const K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO: UInt32 = fourcc(b"dch2");
pub const K_AUDIO_DEVICE_PROPERTY_VOLUME_SCALAR: UInt32 = fourcc(b"volm");
pub const K_AUDIO_DEVICE_PROPERTY_VOLUME_DECIBELS: UInt32 = fourcc(b"vold");
pub const K_AUDIO_DEVICE_PROPERTY_VOLUME_RANGE_DECIBELS: UInt32 = fourcc(b"vdb#");
pub const K_AUDIO_DEVICE_PROPERTY_VOLUME_SCALAR_TO_DECIBELS: UInt32 = fourcc(b"v2db");
pub const K_AUDIO_DEVICE_PROPERTY_VOLUME_DECIBELS_TO_SCALAR: UInt32 = fourcc(b"db2v");

// ---------------------------------------------------------------------------
// Standard property selectors — Stream
// ---------------------------------------------------------------------------

pub const K_AUDIO_STREAM_PROPERTY_DIRECTION: UInt32 = fourcc(b"sdir");
pub const K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE: UInt32 = fourcc(b"term");
pub const K_AUDIO_STREAM_PROPERTY_START_CHANNEL: UInt32 = fourcc(b"schn");
pub const K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT: UInt32 = fourcc(b"sfmt");
pub const K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT: UInt32 = fourcc(b"pft ");
pub const K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS: UInt32 = fourcc(b"sfma");
pub const K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS: UInt32 = fourcc(b"pfta");
pub const K_AUDIO_STREAM_PROPERTY_LATENCY: UInt32 = fourcc(b"ltnc");
pub const K_AUDIO_STREAM_PROPERTY_IS_ACTIVE: UInt32 = fourcc(b"sact");

// ---------------------------------------------------------------------------
// Hardware (system-level) property selectors
// ---------------------------------------------------------------------------

pub const K_AUDIO_HARDWARE_PROPERTY_PLUG_IN_LIST: UInt32 = fourcc(b"plg#");

// ---------------------------------------------------------------------------
// Audio format constants
// ---------------------------------------------------------------------------

pub const K_AUDIO_FORMAT_LINEAR_PCM: UInt32 = fourcc(b"lpcm");

pub const K_AUDIO_FORMAT_FLAG_IS_FLOAT: UInt32 = 1 << 0;
pub const K_AUDIO_FORMAT_FLAG_IS_PACKED: UInt32 = 1 << 3;
pub const K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED: UInt32 =
    K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED;

// Stream terminal types
pub const K_INPUT_TERMINAL: UInt32 = 0x0201;
pub const K_OUTPUT_TERMINAL: UInt32 = 0x0301;

// ---------------------------------------------------------------------------
// Custom property selectors (MARS)
// ---------------------------------------------------------------------------

pub const K_MARS_PROPERTY_DESIRED_STATE: UInt32 = fourcc(b"mdst");
// Avoid collision with CoreAudio's deprecated `kAudioHardwarePropertyProcessIsMaster` (`'mast'`).
pub const K_MARS_PROPERTY_APPLIED_STATE: UInt32 = fourcc(b"mpas");
pub const K_MARS_PROPERTY_RUNTIME_STATS: UInt32 = fourcc(b"mrts");
pub const K_MARS_PROPERTY_CONFIG_SUMMARY: UInt32 = fourcc(b"mcfg");

// ---------------------------------------------------------------------------
// Custom property data types (see AudioServerPlugIn.h)
// ---------------------------------------------------------------------------

pub const K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_CFPROPERTYLIST: UInt32 = fourcc(b"plst");
pub const K_AUDIO_SERVER_PLUG_IN_CUSTOM_PROPERTY_DATA_TYPE_NONE: UInt32 = 0;

// ---------------------------------------------------------------------------
// AudioObjectPropertyAddress
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioObjectPropertyAddress {
    pub m_selector: UInt32,
    pub m_scope: UInt32,
    pub m_element: UInt32,
}

// ---------------------------------------------------------------------------
// AudioStreamBasicDescription
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioStreamBasicDescription {
    pub m_sample_rate: Float64,
    pub m_format_id: UInt32,
    pub m_format_flags: UInt32,
    pub m_bytes_per_packet: UInt32,
    pub m_frames_per_packet: UInt32,
    pub m_bytes_per_frame: UInt32,
    pub m_channels_per_frame: UInt32,
    pub m_bits_per_channel: UInt32,
    pub m_reserved: UInt32,
}

impl AudioStreamBasicDescription {
    pub fn float32_stereo(sample_rate: f64, channels: u32) -> Self {
        Self {
            m_sample_rate: sample_rate,
            m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED,
            m_bytes_per_packet: 4 * channels,
            m_frames_per_packet: 1,
            m_bytes_per_frame: 4 * channels,
            m_channels_per_frame: channels,
            m_bits_per_channel: 32,
            m_reserved: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// AudioStreamRangedDescription
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioStreamRangedDescription {
    pub m_format: AudioStreamBasicDescription,
    pub m_sample_rate_range: AudioValueRange,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioValueRange {
    pub m_minimum: Float64,
    pub m_maximum: Float64,
}

// ---------------------------------------------------------------------------
// AudioServerPlugInCustomPropertyInfo
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioServerPlugInCustomPropertyInfo {
    pub m_selector: UInt32,
    pub m_property_data_type: UInt32,
    pub m_qualifier_data_type: UInt32,
}

// ---------------------------------------------------------------------------
// AudioServerPlugInIOCycleInfo
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioServerPlugInIOCycleInfo {
    pub m_io_cycle_counter: u64,
    pub m_nominal_io_buffer_frame_size: UInt32,
    pub m_current_time: u64,
    pub m_io_buffer_frame_size: UInt32,
    pub m_io_cycle_start_time: u64,
    pub m_io_cycle_end_time: u64,
}

// ---------------------------------------------------------------------------
// AudioServerPlugInClientInfo
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioServerPlugInClientInfo {
    pub m_client_id: UInt32,
    pub m_process_id: i32,
    pub m_is_native_endian: Boolean,
    pub m_bundle_id: *const c_void, // CFStringRef
}

// ---------------------------------------------------------------------------
// AudioServerPlugInHostInterface
// ---------------------------------------------------------------------------

pub type AudioServerPlugInHostRef = *const AudioServerPlugInHostInterface;

#[derive(Debug)]
#[repr(C)]
pub struct AudioServerPlugInHostInterface {
    pub properties_changed: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        object_id: AudioObjectID,
        number_addresses: UInt32,
        addresses: *const AudioObjectPropertyAddress,
    ) -> OSStatus,

    pub copy_from_storage: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        key: CFStringRef,
        data: *mut CFPropertyListRef,
    ) -> OSStatus,

    pub write_to_storage: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        key: CFStringRef,
        data: CFPropertyListRef,
    ) -> OSStatus,

    pub delete_from_storage:
        unsafe extern "C" fn(host: AudioServerPlugInHostRef, key: CFStringRef) -> OSStatus,

    pub request_device_configuration_change: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        device_object_id: AudioObjectID,
        change_action: u64,
        change_info: *const c_void,
    ) -> OSStatus,
}

// ---------------------------------------------------------------------------
// IUnknown / AudioServerPlugInDriver UUIDs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct CFUUIDBytes {
    pub byte0: UInt8,
    pub byte1: UInt8,
    pub byte2: UInt8,
    pub byte3: UInt8,
    pub byte4: UInt8,
    pub byte5: UInt8,
    pub byte6: UInt8,
    pub byte7: UInt8,
    pub byte8: UInt8,
    pub byte9: UInt8,
    pub byte10: UInt8,
    pub byte11: UInt8,
    pub byte12: UInt8,
    pub byte13: UInt8,
    pub byte14: UInt8,
    pub byte15: UInt8,
}

pub type REFIID = CFUUIDBytes;

/// IUnknown UUID: 00000000-0000-0000-C000-000000000046
pub const IID_IUNKNOWN: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x00,
    byte1: 0x00,
    byte2: 0x00,
    byte3: 0x00,
    byte4: 0x00,
    byte5: 0x00,
    byte6: 0x00,
    byte7: 0x00,
    byte8: 0xC0,
    byte9: 0x00,
    byte10: 0x00,
    byte11: 0x00,
    byte12: 0x00,
    byte13: 0x00,
    byte14: 0x00,
    byte15: 0x46,
};

/// AudioServerPlugInDriverInterface UUID in CoreFoundation `CFUUIDBytes` order.
///
/// On macOS, `REFIID` is `CFUUIDBytes` passed by value, not a pointer to a COM
/// GUID. The byte order must therefore exactly match the bytes from
/// `CFUUIDGetConstantUUIDWithBytes`.
pub const IID_AUDIO_SERVER_PLUGIN_DRIVER: CFUUIDBytes = CFUUIDBytes {
    byte0: 0xEE,
    byte1: 0xA5,
    byte2: 0x77,
    byte3: 0x3D,
    byte4: 0xCC,
    byte5: 0x43,
    byte6: 0x49,
    byte7: 0xF1,
    byte8: 0x8E,
    byte9: 0x00,
    byte10: 0x8F,
    byte11: 0x96,
    byte12: 0xE7,
    byte13: 0xD2,
    byte14: 0x3B,
    byte15: 0x17,
};

// ---------------------------------------------------------------------------
// AudioServerPlugInDriverInterface — COM vtable
// ---------------------------------------------------------------------------

/// COM interface matching Apple's AudioServerPlugIn.h `AudioServerPlugInDriverInterface`.
///
/// The leading `_reserved` slot is part of the IUnknown ABI. Omitting it shifts
/// every function pointer by one word and causes the host to dispatch to the
/// wrong entrypoints during `QueryInterface`.
#[repr(C)]
#[allow(missing_debug_implementations)]
pub struct AudioServerPlugInDriverInterface {
    pub _reserved: *mut c_void,

    pub query_interface: unsafe extern "C" fn(
        driver: *mut c_void,
        iid: REFIID,
        interface: *mut *mut c_void,
    ) -> HRESULT,
    pub add_ref: unsafe extern "C" fn(driver: *mut c_void) -> ULONG,
    pub release: unsafe extern "C" fn(driver: *mut c_void) -> ULONG,

    // Lifecycle
    pub initialize: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        host: AudioServerPlugInHostRef,
    ) -> OSStatus,
    pub create_device: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        description: CFDictionaryRef,
        client_info: *const AudioServerPlugInClientInfo,
        device_object_id: *mut AudioObjectID,
    ) -> OSStatus,
    pub destroy_device: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
    ) -> OSStatus,
    pub add_device_client: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_info: *const AudioServerPlugInClientInfo,
    ) -> OSStatus,
    pub remove_device_client: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_info: *const AudioServerPlugInClientInfo,
    ) -> OSStatus,

    // Configuration change
    pub perform_device_configuration_change: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        change_action: u64,
        change_info: *const c_void,
    ) -> OSStatus,
    pub abort_device_configuration_change: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        change_action: u64,
        change_info: *const c_void,
    ) -> OSStatus,

    // Property operations
    pub has_property: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_process_id: i32,
        address: *const AudioObjectPropertyAddress,
    ) -> Boolean,
    pub is_property_settable: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_process_id: i32,
        address: *const AudioObjectPropertyAddress,
        is_settable: *mut Boolean,
    ) -> OSStatus,
    pub get_property_data_size: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_process_id: i32,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        data_size: *mut UInt32,
    ) -> OSStatus,
    pub get_property_data: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_process_id: i32,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        data_size: UInt32,
        out_data_size: *mut UInt32,
        data: *mut c_void,
    ) -> OSStatus,
    pub set_property_data: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_process_id: i32,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        data_size: UInt32,
        data: *const c_void,
    ) -> OSStatus,

    // IO operations
    pub start_io: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_id: UInt32,
    ) -> OSStatus,
    pub stop_io: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_id: UInt32,
    ) -> OSStatus,
    pub get_zero_time_stamp: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_id: UInt32,
        out_sample_time: *mut Float64,
        out_host_time: *mut u64,
        out_seed: *mut u64,
    ) -> OSStatus,
    pub will_do_io_operation: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        will_do: *mut Boolean,
        will_do_in_place: *mut Boolean,
    ) -> OSStatus,
    pub begin_io_operation: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        io_buffer_frame_size: UInt32,
        io_cycle_info: *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
    pub do_io_operation: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        stream_object_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        io_buffer_frame_size: UInt32,
        io_cycle_info: *const AudioServerPlugInIOCycleInfo,
        io_main_buffer: *mut c_void,
        io_secondary_buffer: *mut c_void,
    ) -> OSStatus,
    pub end_io_operation: unsafe extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_object_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        io_buffer_frame_size: UInt32,
        io_cycle_info: *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
}

// These are raw C-level pointers and do not need Send/Sync semantics enforced
// by Rust. The interface is always referenced through a `*const` obtained from
// a `static` and lives for the entire lifetime of the process.
unsafe impl Send for AudioServerPlugInDriverInterface {}
unsafe impl Sync for AudioServerPlugInDriverInterface {}

/// Plugin driver reference — COM double-indirection pointer.
/// Points to a `*const AudioServerPlugInDriverInterface`.
pub type AudioServerPlugInDriverRef = *mut *const AudioServerPlugInDriverInterface;

// IO operation IDs
pub const K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX: UInt32 = fourcc(b"wmix");
pub const K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_READ_INPUT: UInt32 = fourcc(b"rdin");

// ---------------------------------------------------------------------------
// Bundle ID
// ---------------------------------------------------------------------------

pub const MARS_DRIVER_BUNDLE_ID: &str = "com.mars.driver";

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn fourcc_values() {
        assert_eq!(K_MARS_PROPERTY_DESIRED_STATE, fourcc(b"mdst"));
        assert_eq!(K_MARS_PROPERTY_APPLIED_STATE, fourcc(b"mpas"));
        assert_eq!(K_MARS_PROPERTY_RUNTIME_STATS, fourcc(b"mrts"));
        assert_eq!(K_MARS_PROPERTY_CONFIG_SUMMARY, fourcc(b"mcfg"));
        assert_eq!(K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT, fourcc(b"pft "));
        assert_eq!(
            K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS,
            fourcc(b"pfta")
        );
    }

    #[test]
    fn address_layout() {
        assert_eq!(
            mem::size_of::<AudioObjectPropertyAddress>(),
            3 * mem::size_of::<UInt32>()
        );
    }

    #[test]
    fn asbd_layout() {
        // Apple defines AudioStreamBasicDescription as 40 bytes on 64-bit.
        assert_eq!(mem::size_of::<AudioStreamBasicDescription>(), 40);
    }

    #[test]
    fn boolean_matches_mactypes() {
        assert_eq!(mem::size_of::<Boolean>(), 1);
    }

    #[test]
    fn driver_interface_iunknown_layout() {
        assert_eq!(
            mem::offset_of!(AudioServerPlugInDriverInterface, query_interface),
            mem::size_of::<*mut c_void>()
        );
        assert_eq!(
            mem::offset_of!(AudioServerPlugInDriverInterface, add_ref),
            2 * mem::size_of::<*mut c_void>()
        );
    }

    #[test]
    fn host_interface_method_order_matches_header() {
        assert_eq!(
            mem::offset_of!(AudioServerPlugInHostInterface, properties_changed),
            0
        );
        assert_eq!(
            mem::offset_of!(
                AudioServerPlugInHostInterface,
                request_device_configuration_change
            ),
            4 * mem::size_of::<*mut c_void>()
        );
    }
}
