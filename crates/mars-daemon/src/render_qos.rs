//! Scheduling-class helpers for the `marsd-render` thread.
//!
//! Phase 1 of issue #47: promote the render thread to the
//! `USER_INTERACTIVE` QoS class so it is scheduled ahead of the daemon's
//! IPC/serialization and worker threads. Phase 2 (real realtime scheduling
//! via `THREAD_TIME_CONSTRAINT_POLICY`) is intentionally deferred: it must
//! only land after the remaining render-path lock/allocation work (#43,
//! #44, #45, #46), because realtime scheduling turns today's benign
//! normal-priority lock sharing into genuine priority inversion.

/// Promote the calling thread to `QOS_CLASS_USER_INTERACTIVE`.
///
/// Called once from the top of the `marsd-render` thread closure; logs the
/// outcome either way.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn promote_render_thread_qos() {
    // SAFETY: `pthread_set_qos_class_self_np` only affects the calling
    // thread and takes plain value arguments; no pointers are involved.
    let status = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if status == 0 {
        tracing::info!("marsd-render thread promoted to QOS_CLASS_USER_INTERACTIVE");
    } else {
        tracing::warn!(
            status,
            "failed to set marsd-render thread QoS class; continuing at default priority"
        );
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn promote_render_thread_qos() {
    tracing::debug!("render thread QoS promotion is only implemented on macOS");
}
