#import <CoreAudio/AudioHardware.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <Foundation/Foundation.h>

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

static char *mars_copy_utf8(NSString *value) {
    if (value == nil) {
        return NULL;
    }
    const char *utf8 = value.UTF8String;
    if (utf8 == NULL) {
        return NULL;
    }
    const size_t len = strlen(utf8);
    char *result = (char *)malloc(len + 1);
    if (result == NULL) {
        return NULL;
    }
    memcpy(result, utf8, len + 1);
    return result;
}

static void mars_set_error(char **out_error, NSString *message) {
    if (out_error == NULL) {
        return;
    }
    *out_error = mars_copy_utf8(message ?: @"unknown error");
}

static NSString *mars_osstatus_error(NSString *operation, OSStatus status) {
    return [NSString stringWithFormat:@"%@ failed with OSStatus=%d", operation, (int)status];
}

void mars_tap_free_cstring(char *value) {
    if (value != NULL) {
        free(value);
    }
}

int32_t mars_tap_check_capability(uint8_t *out_supported, char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    if (out_supported == NULL) {
        mars_set_error(out_error, @"out_supported must not be null");
        return -50;
    }

    AudioObjectPropertyAddress process_address = {
        kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    AudioObjectPropertyAddress tap_address = {
        kAudioHardwarePropertyTapList,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };

    const Boolean has_process_property =
        AudioObjectHasProperty(kAudioObjectSystemObject, &process_address);
    const Boolean has_tap_property = AudioObjectHasProperty(kAudioObjectSystemObject, &tap_address);

    *out_supported = (has_process_property && has_tap_property) ? 1 : 0;
    if (*out_supported == 0) {
        mars_set_error(out_error,
                       @"CoreAudio process/tap properties are unavailable on this host");
    }

    return noErr;
}

int32_t mars_tap_list_processes_json(char **out_json, char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    if (out_json == NULL) {
        mars_set_error(out_error, @"out_json must not be null");
        return -50;
    }
    *out_json = NULL;

    AudioObjectPropertyAddress list_address = {
        kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };

    UInt32 list_size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &list_address, 0,
                                                     NULL, &list_size);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioObjectGetPropertyDataSize(process list)",
                                           status));
        return status;
    }

    const UInt32 process_count = list_size / sizeof(AudioObjectID);
    NSMutableArray<NSDictionary *> *records = [NSMutableArray arrayWithCapacity:process_count];
    if (process_count > 0) {
        AudioObjectID *process_ids =
            (AudioObjectID *)calloc(process_count, sizeof(AudioObjectID));
        if (process_ids == NULL) {
            mars_set_error(out_error, @"failed to allocate process id buffer");
            return -108;
        }

        UInt32 process_bytes = list_size;
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &list_address, 0, NULL,
                                            &process_bytes, process_ids);
        if (status != noErr) {
            free(process_ids);
            mars_set_error(out_error,
                           mars_osstatus_error(@"AudioObjectGetPropertyData(process list)", status));
            return status;
        }

        for (UInt32 index = 0; index < process_count; index += 1) {
            const AudioObjectID process_id = process_ids[index];

            pid_t pid = 0;
            UInt32 pid_size = sizeof(pid);
            AudioObjectPropertyAddress pid_address = {
                kAudioProcessPropertyPID,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            };
            status = AudioObjectGetPropertyData(process_id, &pid_address, 0, NULL, &pid_size,
                                                &pid);
            if (status != noErr) {
                continue;
            }

            UInt32 running = 0;
            UInt32 running_size = sizeof(running);
            AudioObjectPropertyAddress running_address = {
                kAudioProcessPropertyIsRunning,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            };
            if (AudioObjectGetPropertyData(process_id, &running_address, 0, NULL, &running_size,
                                           &running) != noErr) {
                running = 0;
            }

            UInt32 running_input = 0;
            UInt32 running_input_size = sizeof(running_input);
            AudioObjectPropertyAddress running_input_address = {
                kAudioProcessPropertyIsRunningInput,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            };
            if (AudioObjectGetPropertyData(process_id, &running_input_address, 0, NULL,
                                           &running_input_size,
                                           &running_input) != noErr) {
                running_input = 0;
            }

            UInt32 running_output = 0;
            UInt32 running_output_size = sizeof(running_output);
            AudioObjectPropertyAddress running_output_address = {
                kAudioProcessPropertyIsRunningOutput,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            };
            if (AudioObjectGetPropertyData(process_id, &running_output_address, 0, NULL,
                                           &running_output_size,
                                           &running_output) != noErr) {
                running_output = 0;
            }

            CFStringRef bundle_ref = NULL;
            UInt32 bundle_size = sizeof(bundle_ref);
            AudioObjectPropertyAddress bundle_address = {
                kAudioProcessPropertyBundleID,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            };
            NSString *bundle = @"";
            if (AudioObjectGetPropertyData(process_id, &bundle_address, 0, NULL, &bundle_size,
                                           &bundle_ref) == noErr &&
                bundle_ref != NULL) {
                bundle = [(__bridge NSString *)bundle_ref copy];
                CFRelease(bundle_ref);
            }

            NSDictionary *record = @{
                @"process_object_id" : @(process_id),
                @"pid" : @((int32_t)pid),
                @"bundle_id" : bundle,
                @"is_running" : @((running != 0) ? YES : NO),
                @"is_running_input" : @((running_input != 0) ? YES : NO),
                @"is_running_output" : @((running_output != 0) ? YES : NO),
            };
            [records addObject:record];
        }

        free(process_ids);
    }

    NSArray<NSDictionary *> *sorted =
        [records sortedArrayUsingComparator:^NSComparisonResult(NSDictionary *left,
                                                                NSDictionary *right) {
          const int32_t left_pid = [left[@"pid"] intValue];
          const int32_t right_pid = [right[@"pid"] intValue];
          if (left_pid < right_pid) {
              return NSOrderedAscending;
          }
          if (left_pid > right_pid) {
              return NSOrderedDescending;
          }

          const uint32_t left_object = [left[@"process_object_id"] unsignedIntValue];
          const uint32_t right_object = [right[@"process_object_id"] unsignedIntValue];
          if (left_object < right_object) {
              return NSOrderedAscending;
          }
          if (left_object > right_object) {
              return NSOrderedDescending;
          }
          return NSOrderedSame;
      }];

    NSError *json_error = nil;
    NSData *json_data = [NSJSONSerialization dataWithJSONObject:sorted options:0 error:&json_error];
    if (json_data == nil) {
        mars_set_error(out_error,
                       [NSString stringWithFormat:@"failed to encode process JSON: %@",
                                                  json_error.localizedDescription]);
        return -1;
    }

    char *json_utf8 =
        (char *)malloc((size_t)json_data.length + 1);
    if (json_utf8 == NULL) {
        mars_set_error(out_error, @"failed to allocate process JSON buffer");
        return -108;
    }
    memcpy(json_utf8, json_data.bytes, (size_t)json_data.length);
    json_utf8[json_data.length] = '\0';
    *out_json = json_utf8;

    return noErr;
}

int32_t mars_tap_default_output_device_uid(char **out_uid, char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    if (out_uid == NULL) {
        mars_set_error(out_error, @"out_uid must not be null");
        return -50;
    }
    *out_uid = NULL;

    AudioObjectPropertyAddress default_output_address = {
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };

    AudioDeviceID output_device = kAudioObjectUnknown;
    UInt32 output_device_size = sizeof(output_device);
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject,
                                                 &default_output_address, 0, NULL,
                                                 &output_device_size, &output_device);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioObjectGetPropertyData(default output device)",
                                           status));
        return status;
    }

    if (output_device == kAudioObjectUnknown) {
        mars_set_error(out_error, @"default output device is unavailable");
        return -1;
    }

    AudioObjectPropertyAddress uid_address = {
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    CFStringRef uid_ref = NULL;
    UInt32 uid_size = sizeof(uid_ref);
    status = AudioObjectGetPropertyData(output_device, &uid_address, 0, NULL, &uid_size,
                                        &uid_ref);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioObjectGetPropertyData(device uid)", status));
        return status;
    }
    if (uid_ref == NULL) {
        mars_set_error(out_error, @"default output device returned null UID");
        return -1;
    }

    NSString *uid = [(__bridge NSString *)uid_ref copy];
    CFRelease(uid_ref);
    char *uid_utf8 = mars_copy_utf8(uid);
    if (uid_utf8 == NULL) {
        mars_set_error(out_error, @"failed to allocate default output UID buffer");
        return -108;
    }
    *out_uid = uid_utf8;
    return noErr;
}

static CATapDescription *mars_build_process_tap_description(const uint32_t *process_ids,
                                                            uint32_t process_count,
                                                            uint8_t exclusive,
                                                            uint8_t mono) {
    NSMutableArray<NSNumber *> *processes =
        [NSMutableArray arrayWithCapacity:process_count];
    for (uint32_t index = 0; index < process_count; index += 1) {
        [processes addObject:@(process_ids[index])];
    }

    if (exclusive != 0) {
        if (mono != 0) {
            return [[CATapDescription alloc]
                initMonoGlobalTapButExcludeProcesses:processes];
        }
        return [[CATapDescription alloc]
            initStereoGlobalTapButExcludeProcesses:processes];
    }

    if (mono != 0) {
        return [[CATapDescription alloc] initMonoMixdownOfProcesses:processes];
    }
    return [[CATapDescription alloc] initStereoMixdownOfProcesses:processes];
}

static int32_t mars_copy_tap_uid(AudioObjectID tap_id, char **out_uid, char **out_error) {
    if (out_uid == NULL) {
        mars_set_error(out_error, @"out_uid must not be null");
        return -50;
    }
    *out_uid = NULL;

    AudioObjectPropertyAddress uid_address = {
        kAudioTapPropertyUID,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    CFStringRef uid_ref = NULL;
    UInt32 uid_size = sizeof(uid_ref);
    OSStatus status =
        AudioObjectGetPropertyData(tap_id, &uid_address, 0, NULL, &uid_size, &uid_ref);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioObjectGetPropertyData(tap uid)", status));
        return status;
    }
    if (uid_ref == NULL) {
        mars_set_error(out_error, @"tap UID is null");
        return -1;
    }

    NSString *uid = [(__bridge NSString *)uid_ref copy];
    CFRelease(uid_ref);
    char *uid_utf8 = mars_copy_utf8(uid);
    if (uid_utf8 == NULL) {
        mars_set_error(out_error, @"failed to allocate tap UID buffer");
        return -108;
    }
    *out_uid = uid_utf8;
    return noErr;
}

int32_t mars_tap_create_process_tap(const uint32_t *process_ids, uint32_t process_count,
                                    uint8_t exclusive, uint8_t mono, const char *name,
                                    uint8_t private_tap, int32_t mute_behavior,
                                    uint32_t *out_tap_id, char **out_tap_uid,
                                    char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    if (process_ids == NULL || process_count == 0 || out_tap_id == NULL ||
        out_tap_uid == NULL) {
        mars_set_error(out_error,
                       @"process tap requires process_ids, process_count, out_tap_id, and out_tap_uid");
        return -50;
    }

    CATapDescription *description =
        mars_build_process_tap_description(process_ids, process_count, exclusive, mono);
    if (description == nil) {
        mars_set_error(out_error, @"failed to construct CATapDescription for process tap");
        return -1;
    }

    if (name != NULL && strlen(name) > 0) {
        description.name = [NSString stringWithUTF8String:name];
    }
    description.privateTap = (private_tap != 0);

    switch (mute_behavior) {
    case 1:
        description.muteBehavior = CATapMuted;
        break;
    case 2:
        description.muteBehavior = CATapMutedWhenTapped;
        break;
    default:
        description.muteBehavior = CATapUnmuted;
        break;
    }

    AudioObjectID tap_id = kAudioObjectUnknown;
    OSStatus status = AudioHardwareCreateProcessTap(description, &tap_id);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioHardwareCreateProcessTap(process)", status));
        return status;
    }

    char *tap_uid = NULL;
    const int32_t uid_status = mars_copy_tap_uid(tap_id, &tap_uid, out_error);
    if (uid_status != noErr) {
        (void)AudioHardwareDestroyProcessTap(tap_id);
        return uid_status;
    }

    *out_tap_id = tap_id;
    *out_tap_uid = tap_uid;
    return noErr;
}

int32_t mars_tap_create_system_tap(const char *device_uid, int32_t stream_index, uint8_t mono,
                                   const char *name, uint8_t private_tap,
                                   int32_t mute_behavior, uint32_t *out_tap_id,
                                   char **out_tap_uid, char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    if (out_tap_id == NULL || out_tap_uid == NULL) {
        mars_set_error(out_error, @"out_tap_id and out_tap_uid must not be null");
        return -50;
    }

    NSMutableArray<NSNumber *> *no_processes = [NSMutableArray array];
    CATapDescription *description = nil;

    if (device_uid != NULL && strlen(device_uid) > 0) {
        NSString *device_uid_string = [NSString stringWithUTF8String:device_uid];
        const NSInteger stream = (stream_index >= 0) ? stream_index : 0;
        description = [[CATapDescription alloc]
            initExcludingProcesses:no_processes
                       andDeviceUID:device_uid_string
                         withStream:stream];
        description.mono = (mono != 0);
    } else {
        if (mono != 0) {
            description = [[CATapDescription alloc]
                initMonoGlobalTapButExcludeProcesses:no_processes];
        } else {
            description = [[CATapDescription alloc]
                initStereoGlobalTapButExcludeProcesses:no_processes];
        }
    }

    if (description == nil) {
        mars_set_error(out_error, @"failed to construct CATapDescription for system tap");
        return -1;
    }

    if (name != NULL && strlen(name) > 0) {
        description.name = [NSString stringWithUTF8String:name];
    }
    description.privateTap = (private_tap != 0);

    switch (mute_behavior) {
    case 1:
        description.muteBehavior = CATapMuted;
        break;
    case 2:
        description.muteBehavior = CATapMutedWhenTapped;
        break;
    default:
        description.muteBehavior = CATapUnmuted;
        break;
    }

    AudioObjectID tap_id = kAudioObjectUnknown;
    OSStatus status = AudioHardwareCreateProcessTap(description, &tap_id);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioHardwareCreateProcessTap(system)", status));
        return status;
    }

    char *tap_uid = NULL;
    const int32_t uid_status = mars_copy_tap_uid(tap_id, &tap_uid, out_error);
    if (uid_status != noErr) {
        (void)AudioHardwareDestroyProcessTap(tap_id);
        return uid_status;
    }

    *out_tap_id = tap_id;
    *out_tap_uid = tap_uid;
    return noErr;
}

int32_t mars_tap_create_private_aggregate_device(const char *aggregate_uid,
                                                 const char *aggregate_name,
                                                 const char *tap_uid,
                                                 uint8_t auto_start,
                                                 uint32_t *out_device_id,
                                                 char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    if (aggregate_uid == NULL || aggregate_name == NULL || tap_uid == NULL ||
        out_device_id == NULL) {
        mars_set_error(out_error,
                       @"aggregate_uid, aggregate_name, tap_uid, and out_device_id are required");
        return -50;
    }

    NSString *aggregate_uid_string = [NSString stringWithUTF8String:aggregate_uid];
    NSString *aggregate_name_string = [NSString stringWithUTF8String:aggregate_name];
    NSString *tap_uid_string = [NSString stringWithUTF8String:tap_uid];

    NSDictionary *sub_tap = @{
        [NSString stringWithUTF8String:kAudioSubTapUIDKey] : tap_uid_string,
        [NSString stringWithUTF8String:kAudioSubTapDriftCompensationKey] : @0,
    };

    NSMutableDictionary *aggregate_description = [NSMutableDictionary dictionaryWithDictionary:@{
        [NSString stringWithUTF8String:kAudioAggregateDeviceUIDKey] : aggregate_uid_string,
        [NSString stringWithUTF8String:kAudioAggregateDeviceNameKey] : aggregate_name_string,
        [NSString stringWithUTF8String:kAudioAggregateDeviceIsPrivateKey] : @1,
        [NSString stringWithUTF8String:kAudioAggregateDeviceTapListKey] : @[ sub_tap ],
    }];

    aggregate_description[
        [NSString stringWithUTF8String:kAudioAggregateDeviceTapAutoStartKey]] =
        (auto_start != 0) ? @1 : @0;

    AudioObjectID device_id = kAudioObjectUnknown;
    OSStatus status = AudioHardwareCreateAggregateDevice(
        (__bridge CFDictionaryRef)aggregate_description, &device_id);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioHardwareCreateAggregateDevice", status));
        return status;
    }

    *out_device_id = device_id;
    return noErr;
}

int32_t mars_tap_destroy_process_tap(uint32_t tap_id, char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    const OSStatus status = AudioHardwareDestroyProcessTap(tap_id);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioHardwareDestroyProcessTap", status));
    }
    return status;
}

int32_t mars_tap_destroy_aggregate_device(uint32_t device_id, char **out_error) {
    if (out_error != NULL) {
        *out_error = NULL;
    }
    const OSStatus status = AudioHardwareDestroyAggregateDevice(device_id);
    if (status != noErr) {
        mars_set_error(out_error,
                       mars_osstatus_error(@"AudioHardwareDestroyAggregateDevice", status));
    }
    return status;
}
