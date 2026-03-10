fn main() {
    println!("cargo:rerun-if-changed=src/tap_bridge.m");

    #[cfg(target_os = "macos")]
    {
        cc::Build::new()
            .file("src/tap_bridge.m")
            .flag("-fobjc-arc")
            .compile("mars_tap_bridge");
        println!("cargo:rustc-link-lib=framework=CoreAudio");
        println!("cargo:rustc-link-lib=framework=Foundation");
    }
}
