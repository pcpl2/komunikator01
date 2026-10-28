//! Embeds the application manifest (common controls v6, per-monitor DPI) on Windows targets.
fn main() {
    println!("cargo:rerun-if-changed=komunikator.rc");
    println!("cargo:rerun-if-changed=komunikator.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("komunikator.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
