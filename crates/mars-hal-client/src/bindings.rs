//! Low-level CoreAudio FFI bindings for communicating with the Mars driver plugin.

use std::ffi::c_void;

use mars_hal::coreaudio_types::{
    AudioObjectID, AudioObjectPropertyAddress, K_AUDIO_HARDWARE_PROPERTY_PLUG_IN_LIST,
    K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
    K_AUDIO_OBJECT_SYSTEM_OBJECT, K_AUDIO_PLUG_IN_PROPERTY_BUNDLE_ID, MARS_DRIVER_BUNDLE_ID,
    OSStatus, UInt32,
};

use crate::HalClientError;

// ---------------------------------------------------------------------------
// CoreAudio extern declarations
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        out_data_size: *mut UInt32,
    ) -> OSStatus;

    fn AudioObjectGetPropertyData(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        io_data_size: *mut UInt32,
        out_data: *mut c_void,
    ) -> OSStatus;

    fn AudioObjectSetPropertyData(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        data_size: UInt32,
        data: *const c_void,
    ) -> OSStatus;

    // CoreFoundation
    fn CFDataCreate(alloc: *const c_void, bytes: *const u8, length: isize) -> *const c_void;
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFRelease(cf: *const c_void);

    fn CFStringGetLength(string: *const c_void) -> isize;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut u8,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Find the AudioObjectID of the Mars driver plugin by scanning all loaded plugins.
pub(crate) fn find_mars_plugin_id() -> Result<AudioObjectID, HalClientError> {
    let plugin_ids = get_plugin_list()?;

    for plugin_id in plugin_ids {
        if let Ok(bundle_id) = get_plugin_bundle_id(plugin_id) {
            if bundle_id == MARS_DRIVER_BUNDLE_ID {
                return Ok(plugin_id);
            }
        }
    }

    Err(HalClientError::DriverNotFound)
}

/// Check whether any loaded plugin has the Mars bundle ID.
pub(crate) fn is_mars_plugin_loaded() -> bool {
    find_mars_plugin_id().is_ok()
}

/// Set a custom property on the plugin object with raw bytes (CFData).
pub(crate) fn set_property_cfdata(
    object_id: AudioObjectID,
    selector: UInt32,
    bytes: &[u8],
) -> Result<(), HalClientError> {
    // SAFETY: FFI call to CoreFoundation, passing valid byte slice.
    let cf_data = unsafe { CFDataCreate(core::ptr::null(), bytes.as_ptr(), bytes.len() as isize) };
    if cf_data.is_null() {
        return Err(HalClientError::CoreFoundationError(
            "CFDataCreate returned null".to_string(),
        ));
    }

    let address = AudioObjectPropertyAddress {
        m_selector: selector,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    // The property data is a CFDataRef (a pointer).
    let ptr_size = size_of::<*const c_void>() as UInt32;

    // SAFETY: FFI call with valid address, cf_data is a valid CFDataRef.
    let status = unsafe {
        AudioObjectSetPropertyData(
            object_id,
            &address,
            0,
            core::ptr::null(),
            ptr_size,
            (&cf_data as *const *const c_void).cast::<c_void>(),
        )
    };

    // SAFETY: cf_data is a valid CF object.
    unsafe { CFRelease(cf_data) };

    if status != 0 {
        return Err(HalClientError::OsStatus(status));
    }

    Ok(())
}

/// Get a custom property from the plugin object and return the CFData bytes.
pub(crate) fn get_property_cfdata(
    object_id: AudioObjectID,
    selector: UInt32,
) -> Result<Vec<u8>, HalClientError> {
    let address = AudioObjectPropertyAddress {
        m_selector: selector,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    // First get the data size.
    let mut data_size: UInt32 = 0;
    // SAFETY: FFI call with valid address.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(object_id, &address, 0, core::ptr::null(), &mut data_size)
    };
    if status != 0 {
        return Err(HalClientError::OsStatus(status));
    }

    // The property returns a CFDataRef (a pointer).
    let mut cf_data: *const c_void = core::ptr::null();
    let mut io_size = size_of::<*const c_void>() as UInt32;

    // SAFETY: FFI call, io_size matches the buffer.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &address,
            0,
            core::ptr::null(),
            &mut io_size,
            (&mut cf_data as *mut *const c_void).cast::<c_void>(),
        )
    };
    if status != 0 {
        return Err(HalClientError::OsStatus(status));
    }

    if cf_data.is_null() {
        return Err(HalClientError::CoreFoundationError(
            "GetPropertyData returned null CFData".to_string(),
        ));
    }

    // SAFETY: cf_data is a valid CFDataRef returned by the driver.
    let ptr = unsafe { CFDataGetBytePtr(cf_data) };
    let len = unsafe { CFDataGetLength(cf_data) };

    if ptr.is_null() || len <= 0 {
        // SAFETY: valid CF object.
        unsafe { CFRelease(cf_data) };
        return Err(HalClientError::CoreFoundationError(
            "CFData has no bytes".to_string(),
        ));
    }

    // SAFETY: ptr is valid for len bytes.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len as usize) }.to_vec();

    // SAFETY: valid CF object.
    unsafe { CFRelease(cf_data) };

    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn get_plugin_list() -> Result<Vec<AudioObjectID>, HalClientError> {
    let address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_HARDWARE_PROPERTY_PLUG_IN_LIST,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let mut data_size: UInt32 = 0;
    // SAFETY: FFI call with valid address.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &address,
            0,
            core::ptr::null(),
            &mut data_size,
        )
    };
    if status != 0 {
        return Err(HalClientError::OsStatus(status));
    }

    if data_size == 0 {
        return Ok(Vec::new());
    }

    let count = data_size as usize / size_of::<AudioObjectID>();
    let mut ids = vec![0_u32; count];

    // SAFETY: buffer is correctly sized.
    let status = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &address,
            0,
            core::ptr::null(),
            &mut data_size,
            ids.as_mut_ptr().cast::<c_void>(),
        )
    };
    if status != 0 {
        return Err(HalClientError::OsStatus(status));
    }

    Ok(ids)
}

fn get_plugin_bundle_id(plugin_id: AudioObjectID) -> Result<String, HalClientError> {
    let address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_PLUG_IN_PROPERTY_BUNDLE_ID,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let mut cf_string: *const c_void = core::ptr::null();
    let mut io_size = size_of::<*const c_void>() as UInt32;

    // SAFETY: FFI call with valid address.
    let status = unsafe {
        AudioObjectGetPropertyData(
            plugin_id,
            &address,
            0,
            core::ptr::null(),
            &mut io_size,
            (&mut cf_string as *mut *const c_void).cast::<c_void>(),
        )
    };
    if status != 0 {
        return Err(HalClientError::OsStatus(status));
    }

    if cf_string.is_null() {
        return Err(HalClientError::CoreFoundationError(
            "bundle ID is null".to_string(),
        ));
    }

    // SAFETY: cf_string is a valid CFStringRef.
    let len = unsafe { CFStringGetLength(cf_string) };
    // Allocate buffer: worst case 4 bytes per character + NUL.
    let buf_size = (len * 4 + 1) as usize;
    let mut buf = vec![0_u8; buf_size];

    // SAFETY: FFI call with valid buffer.
    let ok = unsafe {
        CFStringGetCString(
            cf_string,
            buf.as_mut_ptr(),
            buf_size as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };

    // SAFETY: valid CF object.
    unsafe { CFRelease(cf_string) };

    if ok == 0 {
        return Err(HalClientError::CoreFoundationError(
            "CFStringGetCString failed".to_string(),
        ));
    }

    // Find NUL terminator.
    let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..nul_pos]).into_owned();

    Ok(s)
}
