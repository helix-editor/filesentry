fn main() {
    // Declare the custom cfg for check-cfg
    println!("cargo::rustc-check-cfg=cfg(watcher_disable)");

    // Gate on the target OS (CARGO_CFG_TARGET_OS) rather than the host, so that
    // cross-compiling picks the right backend.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let supported = matches!(target_os.as_str(), "linux" | "macos" | "windows");

    // Disable the watcher (compile out to the polling fallback) if there is no
    // backend for this target, or FILESENTRY_DISABLE is set (for testing).
    let disable = !supported || std::env::var_os("FILESENTRY_DISABLE").is_some();

    if disable {
        println!("cargo::rustc-cfg=watcher_disable");
    }

    println!("cargo::rerun-if-env-changed=FILESENTRY_DISABLE");
}
