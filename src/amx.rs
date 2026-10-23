//! The AMX tile layer (TODO 102): detection, the tile configuration and its release. The kernels call stdarch's `_tile_*` intrinsics
//! themselves, in `#[target_feature(enable = "amx-tile,amx-int8")]` functions, naming the tile registers by number. Taken from plumb's
//! `plumb_tiles` (`detect.rs`, `session.rs`) without its safe slice API, nesting checks and stable `asm!` path: TMR's kernels walk raw
//! pointers (Rule 6), it owns its threads, and it builds on nightly.
//!
//! Every tile is 16 rows of 64 bytes (palette 1, all eight tiles), so a tile at a row stride of 64 is 1 KiB of memory, 16 lines.

use std::arch::x86_64::{__cpuid_count, _tile_loadconfig, _tile_release};

/// Rows a tile has, bytes a row: a tile is 1 KiB, 16 cache lines at a stride of 64.
pub const ROWS: usize = 16;
pub const ROW_BYTES: usize = 64;
pub const TILE_BYTES: usize = ROWS * ROW_BYTES;

/// Whether this CPU and OS run AMX-INT8 in the fixed tile shape: AMX-TILE and AMX-INT8 with the OS's tile state
/// (`is_x86_feature_detected!` checks XCR0 bits 17 and 18 too), a palette 1 of 8 tiles of at least 16 rows of 64 bytes (leaf 1Dh), and TMUL
/// limits that take a 16 x 64 operand (leaf 1Eh). Windows needs no per-process opt-in (plumb found, on Server 2025). The one test for it,
/// behind the run's choice of tests (`simd::Need`) and the tests' skip; cached, since the runner asks once a step.
pub fn has_amx_int8() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| is_x86_feature_detected!("amx-tile") && is_x86_feature_detected!("amx-int8") && palette1_fits() && tmul_fits())
}

/// Leaf 1Dh: palette 1 has 8 tile names of at least 16 rows of 64 bytes, or `configure`'s descriptor would fault in LDTILECFG. Every AMX
/// CPU so far reports exactly that.
fn palette1_fits() -> bool {
    if __cpuid_count(0, 0).eax < 0x1D || __cpuid_count(0x1D, 0).eax < 1 {
        return false;
    }
    let p1 = __cpuid_count(0x1D, 1);
    let (bytes_per_tile, bytes_per_row, names, rows) = (p1.eax >> 16, p1.ebx & 0xFFFF, p1.ebx >> 16, p1.ecx & 0xFFFF);
    bytes_per_tile as usize >= TILE_BYTES && bytes_per_row as usize >= ROW_BYTES && rows as usize >= ROWS && names >= 8
}

/// Leaf 1Eh: TMUL takes K = 16 rows of B and N = 64 bytes a row.
fn tmul_fits() -> bool {
    if __cpuid_count(0, 0).eax < 0x1E {
        return false;
    }
    let tmul = __cpuid_count(0x1E, 0).ebx;
    (tmul & 0xFF) as usize >= ROWS && (tmul >> 8 & 0xFFFF) as usize >= ROW_BYTES
}

/// The LDTILECFG descriptor: palette 1, all eight tiles 16 rows of 64 bytes.
#[repr(C, align(64))]
struct TileConfig([u8; 64]);

static CONFIG: TileConfig = TileConfig(palette1_all_16x64());

/// Byte 0 the palette, byte 1 the row to resume an interrupted load from (0), each tile's bytes a row as a u16 at 16.., its rows at 48..;
/// tiles 8 to 15 don't exist, and palette 1 requires their fields zero.
const fn palette1_all_16x64() -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = 1;
    let mut t = 0;
    while t < 8 {
        b[16 + 2 * t] = ROW_BYTES as u8;
        b[48 + t] = ROWS as u8;
        t += 1;
    }
    b
}

/// This thread's tiles, configured; released (TILERELEASE) when dropped. While they are configured, Windows saves and restores 8 KiB of
/// tile state at each of the thread's context switches, so a kernel configures them for its own call only. Release builds abort on a
/// panic, so the drop on unwinding is the tests' (each runs on its own thread, and a test that fails mid-kernel ends it).
pub struct Tiles {
    // Tile state belongs to the thread that configured it
    _thread: std::marker::PhantomData<*mut ()>,
}

/// Configures this thread's tiles (LDTILECFG with `CONFIG`), every tile zeroed.
///
/// # Safety
/// The CPU has AMX-TILE with the OS's tile state and this palette (`has_amx_int8`). Call it in an `amx-tile` function, where it inlines.
#[inline]
#[target_feature(enable = "amx-tile")]
pub unsafe fn configure() -> Tiles {
    // SAFETY: AMX-TILE with the OS's tile state (the caller's); CONFIG is 64 bytes of a descriptor palette 1 accepts (`palette1_fits`)
    unsafe { _tile_loadconfig(CONFIG.0.as_ptr()) };
    Tiles { _thread: std::marker::PhantomData }
}

impl Drop for Tiles {
    fn drop(&mut self) {
        // SAFETY: a `Tiles` exists only after `configure`, on a CPU with AMX-TILE
        unsafe { _tile_release() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TODO 102: the descriptor is palette 1 with all eight tiles 16 rows of 64 bytes, its reserved bytes and tiles 8 to 15 zero, and
    /// 64-byte aligned (LDTILECFG's).
    #[test]
    fn the_tile_configuration_is_palette_1_with_eight_16x64_tiles() {
        let b = &CONFIG.0;
        assert_eq!(b[0], 1, "palette");
        assert!(b[1..16].iter().all(|&x| x == 0), "start row and reserved bytes");
        for t in 0..8 {
            assert_eq!(u16::from_le_bytes([b[16 + 2 * t], b[17 + 2 * t]]), 64, "bytes a row of tile {t}");
            assert_eq!(b[48 + t], 16, "rows of tile {t}");
        }
        assert!(b[32..48].iter().all(|&x| x == 0) && b[56..64].iter().all(|&x| x == 0), "tiles 8 to 15");
        assert_eq!(std::ptr::from_ref(&CONFIG).addr() % 64, 0);
    }
}
