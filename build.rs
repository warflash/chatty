//! macOS: the binary finds `libtranscribe.dylib` (and its ggml dylibs) next to
//! itself or in an app bundle's `Contents/Frameworks`, and in a dev build in
//! the fetched `native/transcribe/lib`. The sys crate's own rpath does not
//! reach downstream binaries.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("native/transcribe/lib");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dev.display());
    }
}
