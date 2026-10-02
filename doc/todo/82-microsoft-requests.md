# TODO 82. [TMR-APP] Microsoft requests: missing and mistyped memory flags, and the strict NUMA docs

**Raised**: 2026-10-02. The user: "we should report all the flags we need in one go", covering the
flags TMR uses and the documented ones it might use later.

**Audit (2026-10-02).** Every memory flag in `src/memory` and `../numa-test`, every one on the
`VirtualAlloc2`, `VirtualFree` and `MEM_EXTENDED_PARAMETER` pages, and winnt.h's
extended-parameter block (SDK 10.0.26100), checked against `windows` 0.62.2 and `windows-sys` 0.61.2.
Both crates agree everywhere.

## A. microsoft/win32metadata (where the `windows` and `windows-sys` bindings come from)

Missing from both crates:

| Constant | Value (winnt.h) | Documented | TMR uses it |
|---|---|---|---|
| `MEM_EXTENDED_PARAMETER_NUMA_NODE_MANDATORY` | `MINLONG64` (`0x8000_0000_0000_0000`), OR-ed into the `MemExtendedParameterNumaNode` value | no | yes (`backend.rs`, `stitched.rs`) |
| `MEM_64K_PAGES` | `0x20400000` (`MEM_LARGE_PAGES \| MEM_PHYSICAL`) | `VirtualAlloc2` | no |
| `MEMORY_CURRENT_PARTITION_HANDLE` | `(HANDLE)(LONG_PTR)-1` | no | no |
| `MEMORY_SYSTEM_PARTITION_HANDLE` | `(HANDLE)(LONG_PTR)-2` | no | no |
| `MEMORY_EXISTING_VAD_PARTITION_HANDLE` | `(HANDLE)(LONG_PTR)-3` | no | no |

Present, but typed so they don't combine with the flags of the parameter they are documented for:

| Constant | Crate type (module) | Documented for | So callers write |
|---|---|---|---|
| `MEM_PRESERVE_PLACEHOLDER` | `UNMAP_VIEW_OF_FILE_FLAGS` (Memory) | `VirtualFree` `dwFreeType` | `VIRTUAL_FREE_TYPE(MEM_RELEASE.0 \| MEM_PRESERVE_PLACEHOLDER.0)` (TMR) |
| `MEM_COALESCE_PLACEHOLDERS` | `u32` (SystemServices) | `VirtualFree` `dwFreeType` | `VIRTUAL_FREE_TYPE(MEM_RELEASE.0 \| MEM_COALESCE_PLACEHOLDERS)` (TMR) |
| `MEM_TOP_DOWN`, `MEM_PHYSICAL`, `MEM_WRITE_WATCH` | `u32` (SystemServices) | `VirtualAlloc2` `AllocationType` | `VIRTUAL_ALLOCATION_TYPE(...)` by hand (not used by TMR) |
| `MEM_EXTENDED_PARAMETER_*` attribute flags | `u32` (SystemServices) | `MEM_EXTENDED_PARAMETER.ULong64` (`u64`) | `as u64` (TMR) |

The ask: add the five, and give the placeholder and allocation-type flags the type of the parameter
they are passed in, or in both modules.

## B. MicrosoftDocs/sdk-api (the docs pages)

- **The strict node.** `MEM_EXTENDED_PARAMETER`, `MEM_EXTENDED_PARAMETER_TYPE` and `VirtualAlloc2`
  describe the node only as preferred. Ask for `MEM_EXTENDED_PARAMETER_NUMA_NODE_MANDATORY` to be
  documented, with what `../numa-test` measured on 10.0.26100:
  - it is OR-ed into the node value;
  - for 1 GiB and 2 MiB pages, it is refused with 1450 once the node is out, where a preferred
    request lands on another node without an error, and can even be split across nodes;
  - 4 KiB requests reject it (87), and so does putting it in `MemExtendedParameterAttributeFlags`.
- **1450 is the same code** whether one node or the whole machine is out (part 4). Worth saying.
- **`AttributeFlags` is missing from `VirtualAlloc2`'s parameter text.** It says each extended
  parameter can be `MemExtendedParameterAddressRequirements` or `MemExtendedParameterNumaNode`,
  but `MemExtendedParameterAttributeFlags` (`NONPAGED_LARGE`, `NONPAGED_HUGE`) is accepted, and is how
  1 GiB pages are requested.
- **Four attribute flags are undocumented:** winnt.h and both crates have
  `MEM_EXTENDED_PARAMETER_GRAPHICS` (`0x1`), `ZERO_PAGES_OPTIONAL` (`0x4`), `SOFT_FAULT_PAGES` (`0x20`)
  and `IMAGE_NO_HPAT` (`0x80`); the page lists only `NONPAGED`, `NONPAGED_LARGE`, `NONPAGED_HUGE` and
  `EC_CODE`. Ask what they do and whether user mode may use them. `ZERO_PAGES_OPTIONAL` might
  matter to TMR: going by its name it lets the kernel skip zeroing, and every large page is zeroed
  when it is allocated. Unmeasured.
- **Undescribed types:** `MemExtendedParameterPartitionHandle`, `UserPhysicalHandle` and
  `ImageMachine` have no description on the `MEM_EXTENDED_PARAMETER_TYPE` page.

## Not ours to report

The refused large-page `MEM_REPLACE_PLACEHOLDER` bugcheck (0x139, 10.0.26100) was reported by the
user; Microsoft is working on it.

## Drafts

To be written from the above when the user asks. Posting is public, so the user posts them or
OKs it.
