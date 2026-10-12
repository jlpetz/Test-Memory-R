# Switching patterns for future memory transfers — 2026-10-11

Keep TMR's ordinary aligned buffers, word identities, epochs and accumulator verification.
Extend the **switching generator** with a compiled transfer profile: a description of how
payload bits become successive values on physical data pins. One packing algorithm can
support 12, 24 and other widths; profiles supply data and encoding rules.

The current production code accepts only 16/32/64-bit beats. This research adds an offline
reference implementation and simulations, not production DDR6 support.

**What is established about the hardware**

| Subject | Verified public evidence | Implication for TMR |
|---|---|---|
| LPDDR6 | JEDEC's JESD209-6 announcement specifies two independent subchannels per die, each with 12 DQ signals, 32-byte access granularity and flexible 32/64-byte accesses. [1] | A 24-DQ device is not automatically one 24-bit transfer stream. Model the independent subchannels. |
| LPDDR6 BL24 | Keysight describes 288 transmitted bits for 256 payload bits plus 32 overhead bits. [2] | Payload size remains 32 bytes while successive values on a pin follow a 12-DQ packet map. |
| Metadata placement | A Samsung patent gives an example with overhead interspersed on selected DQ pins and beats; it explicitly labels the layout exemplary. [3] | Width and payload size alone do not determine adjacency on a wire. This is useful for an illustrative model, not a normative profile. |
| Enhanced Write DBI | Keysight describes host-configured XOR masks; the earlier Synopsys presentation details a two-mask form. [2, 4] | Four ones per software byte is not a universal DBI-immunity argument. |
| Desktop DDR6 | The public SmartDV controller page describes a working draft with proposed four 24-bit subchannels. [5] | Prepare for 24 DQ, but do not assign LPDDR6's packet map to DDR6 or claim a final DDR6 map has been verified. |

I did not obtain and verify the complete normative payload-bit tables and active controller
encoding for a real LPDDR6/DDR6 platform. In particular, the user-supplied fixed
"16 metadata + 16 ECC/DBI" explanation is not a sufficient specification for all operating
modes. The public patent uses a field for DBI **or** link ECC. Treat mode selection and
encoding as part of the profile, not universally fixed extra bits.

A burst length counts transfers, not necessarily full clock periods. Narrow pins do not
require a different CPU word size.

**Why a power-of-two payload does not settle switching**

The payload size tells us how many application bits to generate and verify. A pin's
switching depends on which two of those bits, or overhead bits, arrive consecutively
on that same pin.

For a tiny example, the eight payload bits `01010101` toggle on every transfer over a
single pin. Over two pins, in consecutive two-bit groups, they become `01, 01, 01, 01`:
each pin stays constant. Both transfers deliver the same eight-bit payload.

LPDDR6 adds another distinction: some transfer slots contain metadata rather than the next
application bit. Therefore a raw application-bit distance of 12 is not automatically one
transfer's separation on a DQ pin.

```mermaid
flowchart LR
    A["TMR: ordinary 32/64-byte payload"] --> B["Controller transforms and packet mapping"]
    B --> C["Physical pins over successive transfers"]
    C --> D["Decoded payload returned to TMR"]
```

Identity verification compares the decoded payload with what TMR expects. Switching
optimization concerns the middle of this path. A 64-byte application line can remain the
unit of fast SIMD processing in both cases.

Single/dual memory-channel configuration changes routing and concurrency; it does not turn
two independent subchannels into one wider same-pin switching history. This design needs
no physical-address discovery, bank map, privileged driver or per-request worker routing.

**The profile interface**

Use a profile ID/revision with explicit assumptions:

| Field | Purpose |
|---|---|
| Payload bytes and alignment | Locate the logical payload tile from a global offset; preserve overlapping-chunk reproducibility |
| DQ pins and transfer count | Define the within-burst time/pin coordinates; neither must be a power of two |
| Independent subchannels | Keep unrelated transfer streams separate |
| Payload-bit-to-slot map | Map each controllable application bit to a transfer and pin, including controller payload numbering assumptions |
| Overhead/unknown slots | Represent metadata, link protection and other values TMR does not write directly |
| Read and write transforms | Specify known inversion, masks, scramblers, encoding order and relevant mode/state |
| Burst order and boundary assumptions | State whether the model is about one burst, or also claims a relationship between successive bursts |
| Source, revision and confidence | Distinguish a mathematical model, a standards-backed layout and a platform-validated profile |

`SMBIOS says DDR5` is useful for selecting a **nominal** 32-DQ model. It does not establish
the controller's current scrambler, encryption, DBI mode, packet mapping or inter-burst
ordering. Record that limitation rather than interpreting the selected width as measured
physical behavior.

Profiles should be resolved/compiled outside hot loops. A future width or permutation can
then be another profile without another set of hand-written per-word generators. A genuinely
new nonlinear/stateful encoder might need a new encoder model; do not promise arbitrary
future encoding can be expressed as a fixed bit permutation.

**A generic generator that was implemented and checked**

The reference is
[transfer_profiles.py](../research/pattern_review/transfer_profiles.py); results are in
[transfer-profile-results.json](../research/pattern_review/transfer-profile-results.json).

For an unencoded profile, define the desired value at transfer `t`, pin `l`:

```text
wire_target(t, l) = lane_seed[l] XOR (t & 1) XOR polarity
```

For each application bit, look up its `(t, l)` and assign that target value to the payload.
Do not fabricate the values of metadata slots. Every adjacent payload-to-payload edge on
one pin now toggles, irrespective of payload numbering or whether DQ width divides 64.

The obvious bit-by-bit version is only a reference. Its mapping is affine over XOR, so
precompute a base payload and masks for groups of four seed bits:

```text
payload = BASE XOR TABLE_0[seed nibble 0]
               XOR TABLE_1[seed nibble 1] XOR ...
```

This uses the same runtime packing method for every profile. Hash a global payload-tile ID
and epoch once per tile, then perform ordinary vector table loads/XORs. Generate and verify
the same payload through the existing per-width macros. Polarity remains an explicit
payload complement.

For the models below:

| Model | Payload | Nibble tables | Table data, excluding base |
|---|---:|---:|---:|
| 12 DQ, 24 transfers | 32 B | 3 | 1,536 B |
| Abstract 24 DQ, 24 transfers | 64 B | 6 | 6,144 B |

On a 256-bit kernel, one 32-byte contribution is one vector load. A 64-byte contribution
needs two such loads, or one 512-bit load. These are operation counts, **not throughput
measurements**. The table loads and hash still need codegen and CPU-cost checks before
choosing this implementation over constant-mask specializations. Keep today's cheap
16/32/64-bit encoders where useful.

The reference checked 5,120 seed cases per profile across five profiles: **25,600
seed/profile cases**, with explicit P/~P checks. It also checked shuffled payload-bit maps
and a fixed per-pin XOR transform. It supports the algorithm's independence from width
and bit numbering; it does not validate an undocumented hardware map.

**What the simulation found**

The 12-DQ illustrative model uses the overhead locations from the patent's Fig. 4:
DQ 4, 5, 10 and 11 at transfers 8–11 and 20–23. Remaining payload positions are assigned
**synthetic beat-major numbering**. The 24-DQ model duplicates this abstract lane layout.
It is **not** a claimed JEDEC DDR6 map. Both models omit controller payload encoding.

Average switching on **known adjacent payload-to-payload edges**, over 1,024 seeded cases:

| Generator | Illustrative 12-DQ model | Abstract 24-DQ model |
|---|---:|---:|
| Current 16-bit switch image | 31.64% | 59.99% |
| Current 32-bit switch image | 50.13% | 50.13% |
| Current 64-bit switch image | 50.15% | 49.95% |
| Repeat a raw 12-/24-bit alternating group in the application payload | 86.78% | 85.74% |
| Apply the explicit payload-to-pin map | 100% | 100% |
| Pseudo-random payload | 50.06% | 49.97% |

These numbers demonstrate the consequence of a mapping assumption. They do **not** predict
the percentages a real LPDDR6/DDR6 controller will produce.

Counting overhead honestly changes the headline:

| Quantity for the mapped generator | 12-DQ example | 24-DQ example |
|---|---:|---:|
| All within-burst same-pin edges | 276 | 552 |
| Known adjacent payload edges, all toggling | 240 | 480 |
| Minimum flips over arbitrary unknown metadata | 244 | 488 |
| Maximum flips over arbitrary unknown metadata | 276 | 552 |
| Resulting mathematical interval | 88.41–100% | 88.41–100% |

These bounds are exact for this model when unknown slots are freely variable. A short
two-state dynamic program per pin accounts for endpoint parity across each metadata gap.
The dynamic program was checked against all completions of 729 six-transfer ternary
traces (4,096 binary completions).
For example, the four-slot internal gap lies between opposite known endpoints, forcing at
least one transition across its five edges. Treating unknown edges independently misses
that constraint.

If unknown bits were independent fair bits, the expected total would be 93.48%, but real
metadata/ECC is not generally independent. The 88.41% figure is a **transition bound** for
this synthetic unencoded map, unrelated to the 88.9% payload efficiency `256/288`.
Encoding the payload can invalidate the known-edge assumption itself, so this is not an
88.41% guarantee for LPDDR6 hardware.

**Encoding changes the optimization**

Classic count-based byte inversion leaves a four-ones/four-zeros byte unchanged under its
strict threshold rule. That is a conditional advantage of the current balanced-byte
switch image, at the actual input to that encoder.

Enhanced programmable XOR needs separate analysis. An illustrative two-mask encoder can
map balanced alternating bytes as follows:

```text
payload bytes:     A5, 5A        (8 bit transitions)
selected masks:    A5, 5A
encoded bytes:     00, 00        (0 transitions in those data bits)
```

The selection indicator also travels somewhere in the packet and must be counted there.
This example does not claim a particular controller chooses these masks; it disproves a
general argument that balanced software bytes survive every DBI scheme.

A **fixed mask on each pin across all transfers** preserves transition counts exactly:
`(a XOR k) XOR (b XOR k) = a XOR b`. A changing mask need not. Likewise, a fixed pin
permutation preserves aggregate switching while changing which physical neighbours act as
aggressors. Treating every scrambler as either harmless or destructive is too broad.

For a known encoder, evaluate candidate payloads through the encoder and score the resulting
DQ traces. If inversion/selection makes the mapping nonlinear, the simple inverse map is
no longer sufficient; candidate search can still happen offline or during profile
compilation. For an unknown controller, keep a varied pattern suite and record modeled
intent without asserting a wire-level switching percentage.

**Error-finding intent still needs several waveforms**

An entirely alternating trace has only a small number of independent starting bits. For
`W` fully controlled pin histories it has only `2^W` starting states, regardless of burst
length. This cannot supply unique 64-bit words throughout a large run. Metadata holes
change the detailed constraints but do not remove the need for separate identities.

Keep `random`/`ident` for identity, epoch and polarity coverage. Keep switching for strong
simultaneous activity. Add quiet-victim/aggressor and varied-history experiments through
the same profile representation when implementing the deferred work. Highest toggle
count is one electrical stress, not a universal maximum of error probability.

Burst boundaries deserve their own experiment. A generator can prescribe the interior
of a modeled transfer; software generally does not control which other burst precedes it
at the memory controller. Read/write direction, turnaround, rank selection and controller
scheduling affect those boundaries independently of the payload.

On-die ECC or link correction can prevent a raw fault from reaching the application.
End-to-end identity remains useful, but cannot detect a difference that was corrected before
TMR observed it. Additional hardware observability remains a separate project.

**Implementation order**

1. Fix the current implementation findings in the
   [review](pattern_implementation_review_2026-10-11.md). Preserve native families and
   accumulator verification.
2. Introduce profile IDs and confidence/source fields around today's switch encoders.
   Retain `beat=` as an explicit nominal-width override with documented assumptions.
3. Port the generic packing reference into a bounded experimental implementation, with
   12-/24-DQ synthetic profiles, independent known answers and ordinary scalar/SIMD checks.
   Compile all maps/tables before entering worker loops.
4. Validate a normative packet map and its host payload mapping before calling a profile
   LPDDR6- or DDR6-supported. Specify the active read/write encoding modes separately.
5. Verify generated assembly and CPU cost. Hardware trials can wait until an appropriate
   machine exists; they are not required to prove position purity or packing correctness.

This prepares TMR for changing widths without pretending that a bare `beat=24` option
defines a future memory interface.

**Public sources consulted**

1. [JEDEC, publication of JESD209-6 LPDDR6](https://www.businesswire.com/news/home/20250709315796/en/JEDEC-Releases-New-LPDDR6-Standard-to-Enhance-Mobile-and-AI-Memory-Performance),
   2025-07-09. Primary standards-organization announcement.
2. [Keysight, Scale AI: Engineering the Next Leap in LPDDR6 Low-Power Memory](https://www.keysight.com/gb/en/assets/7126-1042/white-papers/Scale-AI-Engineering-the-Next-Leap-in-LPDDR6-Low-Power-Memory.pdf).
   Vendor technical explanation of BL24 overhead and Enhanced Write DBI.
3. [Samsung patent US20250363001A1](https://patents.google.com/patent/US20250363001A1/en),
   Fig. 4 description and its explicit example-layout qualification. Used only to construct
   the illustrative overhead model.
4. [Brett Murdock / Synopsys, JEDEC mobile-memory presentation, 2024](https://www.jedec.org/sites/default/files/Brett%20Murdock_FINAL_Mobile_2024.pdf),
   [public text mirror](https://www.scribd.com/document/788737512/Brett-Murdock-FINAL-Mobile-2024).
   The direct JEDEC PDF was unavailable to the web reader; inspected the mirror's text.
   Pre-standard material, useful for a programmable-mask example, not a substitute for the
   final specification.
5. [SmartDV DDR6 Controller IP](https://www.smartdvtech.com/products/ddr6-controller-ip).
   Explicitly describes draft alignment and proposed 24-bit subchannels; not a JEDEC
   ratification announcement.
