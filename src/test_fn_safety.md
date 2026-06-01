# Safety

Caller must ensure `blocks` reference valid, committed memory regions that remain
live for the duration of the call, aligned to the SIMD width selected for this
variant (64-byte alignment covers AVX-512). The implementation issues raw aligned
loads/stores — and, for non-temporal variants, streaming stores — directly over
the block memory with no bounds checking.
