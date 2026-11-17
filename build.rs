// Windows: embed an application manifest so the GUI gets themed common controls (v6)
// and per-monitor DPI awareness. Without it wxWidgets falls back to Windows 95 styling
// and blurry bitmap scaling.
fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::{embed_manifest, new_manifest};
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
            embed_manifest(new_manifest("OptoCloud.UnityUnpacker"))
                .expect("unable to embed manifest");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
