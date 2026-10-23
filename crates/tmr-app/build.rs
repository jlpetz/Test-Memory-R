//! What a build of tmr.exe links besides its code.
//!
//! - The icon (TODO 7), in every build: tmr.ico's images as the exe's icon resources, which Explorer, the taskbar and a console window
//!   show. Written here as a .res, which the linker takes as it takes an object, so the build needs no resource compiler.
//! - With the window (the `gui` feature), what the CLI-only build doesn't link; without the feature, none of it, so that exe has no
//!   delay-import directory and no manifest:
//!   - The import older Windows lacks. Reactor imports `TryCreatePackageDependency` from `api-ms-win-appmodel-runtime-l1-1-5.dll`, which
//!     Windows has from build 22000 (Windows 11, Server 2025). Imported the normal way, the loader refuses the exe before `main` on Server
//!     2022; delay-loaded, the DLL loads only when the window opens, which `main` allows from 22000 only, so the CLI still starts there.
//!   - The manifest (`tmr.manifest`, TODO 29 S6), which lets a double-click open the window with no console from build 26100
//!     (`src/launch.rs`).

use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");

    println!("cargo::rerun-if-changed=tmr.ico");
    let ico = std::fs::read(Path::new(&dir).join("tmr.ico")).expect("tmr.ico, the exe's icon, beside Cargo.toml");
    let res = Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("tmr.res");
    std::fs::write(&res, icon_res(&ico)).expect("tmr.res written to OUT_DIR");
    println!("cargo::rustc-link-arg-bins={}", res.display());

    if std::env::var_os("CARGO_FEATURE_GUI").is_some() {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:api-ms-win-appmodel-runtime-l1-1-5.dll");
        // The delay-load helper the linker calls on the first call into the DLL
        println!("cargo::rustc-link-arg-bins=delayimp.lib");

        // Merged with the linker's own (its UAC level, asInvoker, as an exe with none runs at) and embedded as the exe's resource 1.
        // Forward slashes: the linker reads the option's path as given.
        let dir = dir.replace('\\', "/");
        println!("cargo::rerun-if-changed=tmr.manifest");
        println!("cargo::rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo::rustc-link-arg-bins=/MANIFESTINPUT:{dir}/tmr.manifest");
    }
}

/// An .ico file as a .res, as rc.exe compiles `1 ICON "tmr.ico"`: each image an RT_ICON resource (ids from 1), then the RT_GROUP_ICON
/// that lists them (id 1, the exe's first icon group, so the exe's icon).
fn icon_res(ico: &[u8]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]);
    // ICONDIR: reserved 0, type 1 (an icon), the image count; then a 16-byte ICONDIRENTRY an image
    assert!(ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1, "tmr.ico isn't an icon file");
    let count = u16_at(4);
    // A .res starts with an empty entry
    let mut res = resource(0, 0, 0, &[]);
    // GRPICONDIR: reserved, type, count; then a 14-byte GRPICONDIRENTRY an image
    let mut group: Vec<u8> = [0, 1, count].iter().flat_map(|v: &u16| v.to_le_bytes()).collect();
    for i in 0..usize::from(count) {
        let entry = 6 + 16 * i;
        let (size, offset) = (u32_at(entry + 8) as usize, u32_at(entry + 12) as usize);
        let id = u16::try_from(i + 1).expect("an icon of fewer than 65,535 images");
        res.extend(resource(RT_ICON, id, 0x1010, &ico[offset..offset + size]));
        // The ICONDIRENTRY's first 12 bytes (width, height, colours, reserved, planes, bit count, size), then the image's id in place of
        // its offset
        group.extend_from_slice(&ico[entry..entry + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    res.extend(resource(RT_GROUP_ICON, 1, 0x1030, &group));
    res
}

/// One .res entry: its 32-byte RESOURCEHEADER (the data's size, the header's, the type and the name as ordinals, the data version, the
/// memory flags, the language, US English but for the empty entry, the version and characteristics), then the data, padded to 4 bytes.
fn resource(kind: u16, name: u16, flags: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + data.len() + 3);
    out.extend_from_slice(&u32::try_from(data.len()).expect("a resource under 4 GiB").to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    for value in [0xFFFF, kind, 0xFFFF, name] {
        out.extend_from_slice(&u16::to_le_bytes(value));
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&u16::to_le_bytes(if kind == 0 { 0 } else { 0x0409 }));
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}
