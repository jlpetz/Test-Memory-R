//! What a build with the window (the `gui` feature, TODO 7) links that the CLI-only build doesn't;
//! without the feature, none of it, so that exe has no delay-import directory and no manifest.
//!
//! - The import older Windows lacks. Reactor imports `TryCreatePackageDependency` from
//!   `api-ms-win-appmodel-runtime-l1-1-5.dll`, which Windows has from build 22000 (Windows 11,
//!   Server 2025). Imported the normal way, the loader refuses the exe before `main` on Server 2022;
//!   delay-loaded, the DLL loads only when the window opens, which `main` allows from 22000 only, so
//!   the CLI still starts there.
//! - The manifest (`tmr.manifest`, TODO 29 S6), which lets a double-click open the window with no
//!   console from build 26100 (`src/launch.rs`).

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_GUI").is_some() {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:api-ms-win-appmodel-runtime-l1-1-5.dll");
        // The delay-load helper the linker calls on the first call into the DLL
        println!("cargo::rustc-link-arg-bins=delayimp.lib");

        // Merged with the linker's own (its UAC level, asInvoker, as an exe with none runs at) and
        // embedded as the exe's resource 1. Forward slashes: the linker reads the option's path
        // as given.
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR").replace('\\', "/");
        println!("cargo::rerun-if-changed=tmr.manifest");
        println!("cargo::rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo::rustc-link-arg-bins=/MANIFESTINPUT:{dir}/tmr.manifest");
    }
}
