fn main() {
    // Declare the custom cfg for check-cfg
    println!("cargo::rustc-check-cfg=cfg(watcher_disable)");

    // Disable the watcher if:
    // 1. We're not on Linux (inotify is Linux-only)
    // 2. The FILESENTRY_DISABLE environment variable is set (for testing)
    let disable = !cfg!(target_os = "linux")
        || std::env::var("FILESENTRY_DISABLE").is_ok();

    if disable {
        println!("cargo::rustc-cfg=watcher_disable");
    }

    // Re-run if the environment variable changes
    println!("cargo::rerun-if-env-changed=FILESENTRY_DISABLE");
}
