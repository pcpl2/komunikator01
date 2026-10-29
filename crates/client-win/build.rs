//! Embeds the application manifest (common controls v6, per-monitor DPI) on Windows targets,
//! and rebuilds when the built-in server changes.
fn main() {
    println!("cargo:rerun-if-changed=komunikator.rc");
    println!("cargo:rerun-if-changed=komunikator.manifest");
    // The built-in server (src/defaults.rs).
    println!("cargo:rerun-if-env-changed=KOMUNIKATOR_SERVER");
    println!("cargo:rerun-if-env-changed=KOMUNIKATOR_FINGERPRINT");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("komunikator.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
