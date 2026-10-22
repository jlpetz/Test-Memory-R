//! The window's import that older Windows lacks (TODO 7). Reactor imports `TryCreatePackageDependency`
//! from `api-ms-win-appmodel-runtime-l1-1-5.dll`, which Windows has from build 22000 (Windows 11,
//! Server 2025). Imported the normal way, the loader refuses the exe before `main` on Server 2022;
//! delay-loaded, the DLL loads only when the window opens, which `main` allows from 22000 only, so
//! the CLI still starts there. Only with the `gui` feature: without it nothing imports the DLL, and
//! the exe has no delay-import directory.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_GUI").is_some() {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:api-ms-win-appmodel-runtime-l1-1-5.dll");
        // The delay-load helper the linker calls on the first call into the DLL
        println!("cargo::rustc-link-arg-bins=delayimp.lib");
    }
}
