use std::ffi::c_void;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::*;

static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn insert_test_device(
    uid: &str,
    name: &str,
    kind: &str,
    channels: u16,
) -> (AudioObjectID, AudioObjectID, Option<AudioObjectID>) {
    let mut reg = PLUGIN.object_registry.lock();
    let device_id = reg.allocate_id();
    let stream_id = reg.allocate_id();
    let volume_control_id = (!kind.contains("input")).then(|| reg.allocate_id());
    reg.devices.insert(
        uid.to_string(),
        DeviceObjectInfo {
            device_id,
            stream_id,
            volume_control_id,
            uid: uid.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            channels,
            hidden: false,
            volume_scalar: 1.0,
            io_running: false,
            sample_time_frames: 0,
            zero_ts_seed: 0,
        },
    );
    (device_id, stream_id, volume_control_id)
}

fn remove_test_device(uid: &str) {
    let mut reg = PLUGIN.object_registry.lock();
    reg.devices.remove(uid);
    let _ = global_registry().remove(&stream_name(StreamDirection::Vout, uid));
    let _ = global_registry().remove(&stream_name(StreamDirection::Vin, uid));
}

fn assert_close(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1.0e-5,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn factory_returns_nonnull() {
    // SAFETY: test-only, no host.
    let ptr = unsafe { MarsAudioServerPlugInFactory(core::ptr::null(), core::ptr::null()) };
    assert!(!ptr.is_null());
}

#[test]
fn query_interface_accepts_audio_server_plugin_driver_iid() {
    let mut interface = core::ptr::null_mut();
    let hr = unsafe {
        plugin_query_interface(
            core::ptr::null_mut(),
            IID_AUDIO_SERVER_PLUGIN_DRIVER,
            &mut interface,
        )
    };
    assert_eq!(hr, S_OK);
    assert!(!interface.is_null());
}

#[test]
fn factory_object_supports_query_interface_via_interface_struct() {
    let obj = unsafe { MarsAudioServerPlugInFactory(core::ptr::null(), core::ptr::null()) };
    assert!(!obj.is_null());

    let driver = obj as AudioServerPlugInDriverRef;
    let interface_struct = unsafe { *driver };
    assert!(!interface_struct.is_null());

    let mut interface = core::ptr::null_mut();
    let hr = unsafe {
        ((*interface_struct).query_interface)(obj, IID_AUDIO_SERVER_PLUGIN_DRIVER, &mut interface)
    };

    assert_eq!(hr, S_OK);
    assert_eq!(interface, obj);
}

#[test]
fn classify_plugin_object() {
    assert_eq!(
        classify_object(K_AUDIO_OBJECT_PLUGIN_OBJECT),
        Some(ObjectType::Plugin)
    );
}

#[test]
fn plugin_property_queries_learn_runtime_plugin_object_id() {
    let _guard = TEST_LOCK.lock();
    PLUGIN.plugin_object_id.store(0, Ordering::Relaxed);

    let address = AudioObjectPropertyAddress {
        m_selector: K_MARS_PROPERTY_CONFIG_SUMMARY,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let has_property = unsafe { plugin_has_property(core::ptr::null_mut(), 49, 0, &address) };

    assert_eq!(has_property, 1);
    assert_eq!(PLUGIN.plugin_object_id.load(Ordering::Relaxed), 49);

    PLUGIN.plugin_object_id.store(0, Ordering::Relaxed);
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
    let (device_id, _, _) = insert_test_device(&uid, "Test Device", "virtual_output", 2);

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
            0,
            core::ptr::null(),
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
            0,
            core::ptr::null(),
            std::mem::size_of::<UInt32>() as UInt32,
            &mut out_size,
            (&mut running_value as *mut UInt32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(out_size, std::mem::size_of::<UInt32>() as UInt32);
    assert_eq!(running_value, 1);

    remove_test_device(&uid);
}

#[test]
fn device_owned_objects_include_volume_controls_for_output_devices() {
    let _guard = TEST_LOCK.lock();
    let uid = "test.owned-controls.uid".to_string();
    let (device_id, stream_id, volume_control_id) =
        insert_test_device(&uid, "Test Device", "virtual_output", 2);
    let volume_control_id = volume_control_id.expect("output devices expose a volume control");

    let address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let control_class = K_AUDIO_CONTROL_CLASS_ID;
    let stream_class = K_AUDIO_STREAM_CLASS_ID;

    let control_size = device_property_data_size(
        device_id,
        &address,
        std::mem::size_of_val(&control_class) as UInt32,
        (&control_class as *const UInt32).cast::<c_void>(),
    );
    assert_eq!(
        control_size,
        Some(std::mem::size_of::<AudioObjectID>() as UInt32)
    );

    let stream_size = device_property_data_size(
        device_id,
        &address,
        std::mem::size_of_val(&stream_class) as UInt32,
        (&stream_class as *const UInt32).cast::<c_void>(),
    );
    assert_eq!(
        stream_size,
        Some(std::mem::size_of::<AudioObjectID>() as UInt32)
    );

    let mut out_size = 0_u32;
    let mut owned_object = 0_u32;
    let status = unsafe {
        device_get_property(
            device_id,
            &address,
            std::mem::size_of_val(&control_class) as UInt32,
            (&control_class as *const UInt32).cast::<c_void>(),
            std::mem::size_of::<AudioObjectID>() as UInt32,
            &mut out_size,
            (&mut owned_object as *mut UInt32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(out_size, std::mem::size_of::<AudioObjectID>() as UInt32);
    assert_eq!(owned_object, volume_control_id);

    let mut stream_out_size = 0_u32;
    let mut stream_object = 0_u32;
    let status = unsafe {
        device_get_property(
            device_id,
            &address,
            std::mem::size_of_val(&stream_class) as UInt32,
            (&stream_class as *const UInt32).cast::<c_void>(),
            std::mem::size_of::<AudioObjectID>() as UInt32,
            &mut stream_out_size,
            (&mut stream_object as *mut UInt32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(
        stream_out_size,
        std::mem::size_of::<AudioObjectID>() as UInt32
    );
    assert_eq!(stream_object, stream_id);

    let mut control_list_out_size = 0_u32;
    let mut listed_control = 0_u32;
    let control_list_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_OBJECT_PROPERTY_CONTROL_LIST,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let status = unsafe {
        device_get_property(
            device_id,
            &control_list_address,
            0,
            core::ptr::null(),
            std::mem::size_of::<AudioObjectID>() as UInt32,
            &mut control_list_out_size,
            (&mut listed_control as *mut UInt32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(
        control_list_out_size,
        std::mem::size_of::<AudioObjectID>() as UInt32
    );
    assert_eq!(listed_control, volume_control_id);

    remove_test_device(&uid);
}

#[test]
fn device_streams_respect_scope() {
    let _guard = TEST_LOCK.lock();
    let uid = "test.stream-scope.uid".to_string();
    let (device_id, stream_id, _) =
        insert_test_device(&uid, "Test Input Device", "virtual_input", 2);

    let input_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_STREAMS,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let output_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_STREAMS,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    assert_eq!(
        device_property_data_size(device_id, &input_address, 0, core::ptr::null()),
        Some(std::mem::size_of::<AudioObjectID>() as UInt32)
    );
    assert_eq!(
        device_property_data_size(device_id, &output_address, 0, core::ptr::null()),
        Some(0)
    );

    let mut out_size = 0_u32;
    let mut input_stream = 0_u32;
    let status = unsafe {
        device_get_property(
            device_id,
            &input_address,
            0,
            core::ptr::null(),
            std::mem::size_of::<AudioObjectID>() as UInt32,
            &mut out_size,
            (&mut input_stream as *mut UInt32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(out_size, std::mem::size_of::<AudioObjectID>() as UInt32);
    assert_eq!(input_stream, stream_id);

    let mut output_out_size = u32::MAX;
    let mut output_stream = 99_u32;
    let status = unsafe {
        device_get_property(
            device_id,
            &output_address,
            0,
            core::ptr::null(),
            std::mem::size_of::<AudioObjectID>() as UInt32,
            &mut output_out_size,
            (&mut output_stream as *mut UInt32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(output_out_size, 0);

    remove_test_device(&uid);
}

#[test]
fn control_and_device_volume_properties_share_state() {
    let _guard = TEST_LOCK.lock();
    let uid = "test.volume.shared-state.uid".to_string();
    let (device_id, _, volume_control_id) =
        insert_test_device(&uid, "Volume Device", "virtual_output", 2);
    let control_id = volume_control_id.expect("output devices expose a volume control");

    let control_scalar_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_LEVEL_CONTROL_PROPERTY_SCALAR_VALUE,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let control_db_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_LEVEL_CONTROL_PROPERTY_DECIBEL_VALUE,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let device_scalar_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_VOLUME_SCALAR,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let device_db_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_VOLUME_DECIBELS,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let new_scalar = 0.25_f32;
    let status = unsafe {
        control_set_property(
            control_id,
            &control_scalar_address,
            std::mem::size_of::<Float32>() as UInt32,
            (&new_scalar as *const Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);

    let mut out_size = 0_u32;
    let mut scalar_value = -1.0_f32;
    let status = unsafe {
        device_get_property(
            device_id,
            &device_scalar_address,
            0,
            core::ptr::null(),
            std::mem::size_of::<Float32>() as UInt32,
            &mut out_size,
            (&mut scalar_value as *mut Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(out_size, std::mem::size_of::<Float32>() as UInt32);
    assert_close(scalar_value, new_scalar);

    let mut db_value = 0.0_f32;
    let status = unsafe {
        control_get_property(
            control_id,
            &control_db_address,
            std::mem::size_of::<Float32>() as UInt32,
            &mut out_size,
            (&mut db_value as *mut Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_close(db_value, volume_scalar_to_decibels(new_scalar));

    let new_db = -12.0_f32;
    let status = unsafe {
        device_set_property(
            device_id,
            &device_db_address,
            std::mem::size_of::<Float32>() as UInt32,
            (&new_db as *const Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);

    let mut control_scalar_value = -1.0_f32;
    let status = unsafe {
        control_get_property(
            control_id,
            &control_scalar_address,
            std::mem::size_of::<Float32>() as UInt32,
            &mut out_size,
            (&mut control_scalar_value as *mut Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_close(control_scalar_value, volume_decibels_to_scalar(new_db));

    let mut device_db_value = 1.0_f32;
    let status = unsafe {
        device_get_property(
            device_id,
            &device_db_address,
            0,
            core::ptr::null(),
            std::mem::size_of::<Float32>() as UInt32,
            &mut out_size,
            (&mut device_db_value as *mut Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_close(device_db_value, new_db);

    remove_test_device(&uid);
}

#[test]
fn write_mix_applies_device_volume_before_writing_ring() {
    let _guard = TEST_LOCK.lock();
    let uid = "test.vmix.uid".to_string();
    let (device_id, _, volume_control_id) =
        insert_test_device(&uid, "Scaled Output", "virtual_output", 2);
    let control_id = volume_control_id.expect("output devices expose a volume control");
    let control_scalar_address = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_LEVEL_CONTROL_PROPERTY_SCALAR_VALUE,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let new_scalar = 0.5_f32;
    let status = unsafe {
        control_set_property(
            control_id,
            &control_scalar_address,
            std::mem::size_of::<Float32>() as UInt32,
            (&new_scalar as *const Float32).cast::<c_void>(),
        )
    };
    assert_eq!(status, K_AUDIO_HARDWARE_NO_ERROR);

    let start_status = unsafe { plugin_start_io(core::ptr::null_mut(), device_id, 1) };
    assert_eq!(start_status, K_AUDIO_HARDWARE_NO_ERROR);

    let mut io_buffer = [1.0_f32, -0.5_f32, 0.25_f32, -0.25_f32];
    let io_status = unsafe {
        plugin_do_io_operation(
            core::ptr::null_mut(),
            device_id,
            0,
            0,
            K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX,
            2,
            core::ptr::null(),
            io_buffer.as_mut_ptr().cast::<c_void>(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(io_status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(io_buffer, [0.5_f32, -0.25_f32, 0.125_f32, -0.125_f32]);

    {
        let ring_name = stream_name(StreamDirection::Vout, &uid);
        let ring_handle = global_registry()
            .open(&ring_name)
            .expect("plugin_start_io creates the VOut ring");
        let mut ring = ring_handle
            .try_lock()
            .expect("ring lock should be available");
        let mut written = [0.0_f32; 4];
        ring.read_interleaved(&mut written)
            .expect("written audio should be readable");
        assert_eq!(written, io_buffer);
    }

    let stop_status = unsafe { plugin_stop_io(core::ptr::null_mut(), device_id, 1) };
    assert_eq!(stop_status, K_AUDIO_HARDWARE_NO_ERROR);
    remove_test_device(&uid);
}

#[test]
fn zero_timestamp_tracks_monotonic_sample_time_and_seed() {
    let _guard = TEST_LOCK.lock();
    let uid = "test.zero-ts.uid".to_string();
    let (device_id, _, _) = insert_test_device(&uid, "Zero TS Device", "virtual_output", 2);

    // SAFETY: test passes valid object id and ignores driver pointer.
    let start_status = unsafe { plugin_start_io(core::ptr::null_mut(), device_id, 1) };
    assert_eq!(start_status, K_AUDIO_HARDWARE_NO_ERROR);

    let mut io_buffer = [0.0_f32; 4];
    // SAFETY: buffer pointer and frame size are valid for 2 stereo frames.
    let io_status = unsafe {
        plugin_do_io_operation(
            core::ptr::null_mut(),
            device_id,
            0,
            0,
            K_AUDIO_SERVER_PLUG_IN_IO_OPERATION_WRITE_MIX,
            2,
            core::ptr::null(),
            io_buffer.as_mut_ptr().cast::<c_void>(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(io_status, K_AUDIO_HARDWARE_NO_ERROR);

    let mut sample_time = -1.0_f64;
    let mut host_time = 0_u64;
    let mut seed_after_start = 0_u64;
    // SAFETY: output pointers are valid.
    let ts_status = unsafe {
        plugin_get_zero_time_stamp(
            core::ptr::null_mut(),
            device_id,
            1,
            &mut sample_time,
            &mut host_time,
            &mut seed_after_start,
        )
    };
    assert_eq!(ts_status, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(sample_time, 2.0);
    assert!(host_time > 0);
    assert!(seed_after_start >= 1);

    // SAFETY: test passes valid object id and ignores driver pointer.
    let stop_status = unsafe { plugin_stop_io(core::ptr::null_mut(), device_id, 1) };
    assert_eq!(stop_status, K_AUDIO_HARDWARE_NO_ERROR);

    let mut sample_time_after_stop = -1.0_f64;
    let mut host_time_after_stop = 0_u64;
    let mut seed_after_stop = 0_u64;
    // SAFETY: output pointers are valid.
    let ts_status_after_stop = unsafe {
        plugin_get_zero_time_stamp(
            core::ptr::null_mut(),
            device_id,
            1,
            &mut sample_time_after_stop,
            &mut host_time_after_stop,
            &mut seed_after_stop,
        )
    };
    assert_eq!(ts_status_after_stop, K_AUDIO_HARDWARE_NO_ERROR);
    assert_eq!(sample_time_after_stop, sample_time);
    assert!(seed_after_stop > seed_after_start);

    remove_test_device(&uid);
}
