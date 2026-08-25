//! Compile-time embedding for the license that accompanies official builds.

// Keep the complete license in a named Mach-O section so it remains attached
// to the executable even when it is copied out of the driver bundle.
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__TEXT,__mars_license"))]
static EMBEDDED_OFFICIAL_DRIVER_BINARY_LICENSE: [u8; include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../bundles/mars.driver/Contents/Resources/MARS-OFFICIAL-DRIVER-BINARY-LICENSE.txt"
))
.len()] = *include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../bundles/mars.driver/Contents/Resources/MARS-OFFICIAL-DRIVER-BINARY-LICENSE.txt"
));

#[cfg(test)]
mod tests {
    use super::EMBEDDED_OFFICIAL_DRIVER_BINARY_LICENSE;

    #[test]
    fn official_driver_license_has_the_required_scope() {
        let license = std::str::from_utf8(&EMBEDDED_OFFICIAL_DRIVER_BINARY_LICENSE)
            .expect("driver license must be UTF-8");
        let normalized = license.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(
            normalized.contains("This license applies only to the precompiled mars.driver bundle")
        );
        assert!(normalized.contains(
            "No permission is granted to use the Official Driver Binary for a Commercial Purpose"
        ));
        assert!(normalized.contains("binaries independently compiled from that source code"));
    }
}
