//! Realtime invariant (issue #38): the HAL IO hot path must not allocate
//! after setup. Lives in its own integration binary so no other test's
//! allocations pollute the counting region. The lock-free design half of
//! the invariant is documented in docs/hal-realtime-invariants.md.
#![allow(clippy::expect_used)]

use mars_hal::plugin::rt_probe;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

#[test]
fn do_io_operation_has_zero_heap_allocation_steady_state() {
    let uid = format!("test.rt-alloc.{}", std::process::id());
    let device_id = rt_probe::setup_probe_device(&uid, 2);

    let mut io_buffer = [0.25_f32; 512];

    // Warm-up covers ring wraps so every lazy path has fired.
    for _ in 0..32 {
        assert_eq!(rt_probe::write_mix_cycle(device_id, &mut io_buffer, 256), 0);
    }

    let region = Region::new(GLOBAL);
    for _ in 0..64 {
        assert_eq!(rt_probe::write_mix_cycle(device_id, &mut io_buffer, 256), 0);
    }
    let stats = region.change();
    assert_eq!(
        stats.allocations + stats.reallocations,
        0,
        "realtime IO callback must not allocate after setup: {stats:?}"
    );

    rt_probe::teardown_probe_device(&uid);
}
