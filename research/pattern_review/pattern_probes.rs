//! Pure formula checks; no large memory allocation.
use super::*;

#[test]
fn review_repro_linepair_collision_above_32_gib() {
    // F(x) = x XOR (x >> 32). Complementing F corresponds to complementing
    // only the high 32 bits of x. For this key, x(0) and x(2^32)
    // have that relationship. The odd line's explicit complement makes
    // two stored words equal.
    let key = 0x405a_c1f5_0000_0000;
    let epoch = StepEpoch { seed: 0x1707_e896_db96_74fb, suite_cycle: 1, step: 0 };
    let phase = Program::single(Family::Linepair).phase(&epoch, 4, 0, Some(BusBeat::Bits32));
    assert_eq!(phase.key, key, "reachable through an ordinary seed= and first dispatch");
    let word_a = 0u64;
    let word_b = (1u64 << 32) + 8;
    let a = linepair_line(word_a / 8, key)[0];
    let b = linepair_line(word_b / 8, key)[0];
    assert_eq!(a, b);
    assert_eq!(a, 0x405a_c1f5_405a_c1f5);
    println!("REPRO: linepair seed {:#x}, key {key:#x}, byte offsets 0 and {:#x}, same value {a:#x}",
             epoch.seed, word_b * 8);
}

#[test]
fn review_repro_odd_round_switch_pairs_never_get_inverted() {
    let program = Program::default_simple();
    let mut switches = 0;
    for suite_cycle in 1..=16 {
        let epoch = StepEpoch { seed: 7, suite_cycle, step: 0 };
        // A valid one-own-cycle step with write_read_cycles=3.
        for round in 0..3 {
            let phase = program.phase(&epoch, 3, round, Some(BusBeat::Bits32));
            if phase.family == Family::Switch {
                switches += 1;
                assert_eq!(phase.mask, 0);
            }
        }
    }
    assert_eq!(switches, 16);
    println!("REPRO: wrc=3, own cycles=1: 16 suite cycles all run switch-P, never switch-~P.");
}

#[test]
fn review_switch_wire_formula_equals_legacy_mode14() {
    for beat in [BusBeat::Bits16, BusBeat::Bits32, BusBeat::Bits64] {
        for n in 0..512u64 {
            let old_address = 0x1e98_0000_0000u64 + n * 64;
            let old_key = 0x1234_5678_9abc_def0;
            let new_lid = line_base(3) + n;
            // Match the pre-hash input. This proves the encoder's identical
            // image, without pretending the real schedules/keys are equal.
            let new_key = old_address.wrapping_add(old_key).wrapping_sub(new_lid);
            assert_eq!(
                switch_line(new_lid, new_key, beat),
                crate::pattern_gen::mode14_line(old_address as usize, old_key, beat)
            );
        }
    }
    println!("PASS: native switch and legacy14 encoder images equal for 1,536 matched hash inputs.");
}

#[test]
fn review_identity_bounds_are_math_not_allocation_requirements() {
    // Both identities remain injective at the 32 GiB linepair collision.
    // Their separate 8 TiB/thread encoding bound also needs explicit validation.
    let b = (1u64 << 32) + 8;
    let key = 0x405a_c1f5_0000_0000;
    assert_ne!(ident_line(0, key)[0], ident_line(b / 8, key)[0]);
    assert_ne!(random_line(0, key)[0], random_line(b / 8, key)[0]);
    assert_eq!(line_base(0) + (1u64 << 37), line_base(1));
    println!("PASS: ident/random distinguish linepair's witness; thread IDs partition 8 TiB ranges.");
}
