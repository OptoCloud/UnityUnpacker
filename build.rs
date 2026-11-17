// Windows: embed an application manifest so the GUI gets themed common controls (v6)
// and per-monitor DPI awareness (without it wxWidgets falls back to Windows 95 styling
// and blurry bitmap scaling), plus the icon Explorer shows for the executables.
fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::{embed_manifest, new_manifest};
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
            embed_manifest(new_manifest("OptoCloud.UnityUnpacker"))
                .expect("unable to embed manifest");
            embed_resource::compile("assets/app.rc", embed_resource::NONE)
                .manifest_required()
                .expect("unable to embed icon");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
}
