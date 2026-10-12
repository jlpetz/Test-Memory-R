//! Independent bit oracle and schedule checks; no large allocation.
use super::*;

fn balanced_reference(h: u64) -> u64 {
    let mut a = 0;
    for byte in 0..8 {
        let nibble = (h >> (4 * byte)) & 15;
        a |= (nibble | ((nibble ^ 15) << 4)) << (8 * byte);
    }
    a
}

fn bit_reference(a: u64, width: usize, payload: usize) -> [u64; 8] {
    let mut out = [0; 8];
    for p in 0..512 {
        let in_burst = p % payload;
        let pin = in_burst % width;
        let transfer = in_burst / width;
        let value = ((a >> pin) & 1) ^ ((transfer & 1) as u64);
        out[p / 64] |= value << (p % 64);
    }
    out
}

#[test]
fn review_all_balanced_12_and_24_seeds_match_independent_bit_oracle() {
    let mut cases = 0;
    // All distinct balanced first-transfer seeds: 256 at 12 DQ and 4096 at 24 DQ.
    for (beat, width, payload, count) in [
        (BusBeat::Bits12, 12, 256, 256),
        (BusBeat::Bits24, 24, 512, 4096),
    ] {
        for h in 0..count {
            let a = balanced_reference(h);
            let actual = crate::pattern_gen::bus_flip_line(a, beat);
            let expected = bit_reference(a, width, payload);
            assert_eq!(actual, expected, "{beat:?} seed {h}");
            for inverse in [false, true] {
                let image = actual.map(|w| if inverse { !w } else { w });
                for burst in (0..512).step_by(payload) {
                    let bit = |at: usize| (image[at / 64] >> (at % 64)) & 1;
                    for p in 0..payload - width {
                        assert_ne!(bit(burst + p), bit(burst + p + width));
                    }
                }
            }
            cases += 1;
        }
    }
    println!("PASS: all {cases} balanced 12/24-bit seeds match an independent bit oracle; both polarities toggle at every payload adjacency.");
}

#[test]
fn review_old_16_32_64_images_are_preserved_without_calling_the_new_packer_for_expected() {
    let mut cases = 0;
    for beat in [BusBeat::Bits16, BusBeat::Bits32, BusBeat::Bits64] {
        for n in 0..4096u64 {
            let old_address = 0x1e98_0000_0000u64 + n * 64;
            let key = 0x1234_5678_9abc_def0;
            let h = fmix(old_address.wrapping_add(key));
            let a = balanced_reference(h);
            let expected = match beat {
                BusBeat::Bits64 => [a, !a, a, !a, a, !a, a, !a],
                BusBeat::Bits32 => [(a & 0xffff_ffff) | (!a << 32); 8],
                BusBeat::Bits16 => {
                    let pair = (a & 65535) | ((!a & 65535) << 16);
                    [pair | pair << 32; 8]
                }
                _ => unreachable!(),
            };
            assert_eq!(crate::pattern_gen::mode14_line(old_address as usize, key, beat), expected);
            let lid = line_base(3) + n;
            let native_key = old_address.wrapping_add(key).wrapping_sub(lid);
            assert_eq!(switch_line(lid, native_key, beat), expected);
            cases += 1;
        }
    }
    println!("PASS: {cases} matched hash inputs retain the original legacy14 and native switch images.");
}

#[test]
fn review_polarity_pairs_and_unknown_widths_survive_rotation() {
    use Family::*;
    for program in [
        Program::default_simple(),
        Program::new(&[Switch, Ident, Switch]).unwrap(),
        Program::new(&[Switch]).unwrap(),
    ] {
        for wrc in [2, 4, 6, 100] {
            for cycle in 1..=16 {
                let epoch = StepEpoch { seed: 7, suite_cycle: cycle, step: 0 };
                for g in (0..u64::from(wrc) * 3).step_by(2) {
                    let a = program.phase(&epoch, wrc, g, Schedule::Rounds, None);
                    let b = program.phase(&epoch, wrc, g + 1, Schedule::Rounds, None);
                    assert_eq!((a.key, a.beat, a.family), (b.key, b.beat, b.family));
                    assert_eq!((a.mask, b.mask), (0, !0));
                }
            }
        }
        let mut switches = Vec::new();
        for cycle in (1..=32).step_by(2) {
            let at = |c| StepEpoch { seed: 11, suite_cycle: c, step: 2 };
            let a = program.phase(&at(cycle), 3, 0, Schedule::OncePerRun, None);
            let b = program.phase(&at(cycle + 1), 3, 0, Schedule::OncePerRun, None);
            assert_eq!((a.key, a.beat, a.family), (b.key, b.beat, b.family));
            assert_eq!((a.mask, b.mask), (0, !0));
            if a.family == Switch { switches.push(a.beat); }
        }
        for (i, beat) in switches.iter().enumerate() {
            assert_eq!(*beat, if i % 2 == 0 { BusBeat::Bits64 } else { BusBeat::Bits32 });
        }
    }
    println!("PASS: round pairs and verify-only cycle pairs share keys and opposite polarity, including alternating unknown-width profiles.");
}
