# Safety

Caller must ensure `blocks` reference valid, committed memory regions that remain
live for the duration of the call, aligned to the SIMD width selected for this
variant (64-byte alignment covers AVX-512). The implementation issues raw aligned
loads/stores — and, for non-temporal variants, streaming stores — directly over
the block memory with no bounds checking. The CPU must have the variant's
instruction set: the function doesn't check, and the run leaves out a test the
CPU can't run (`runner::leave_out_what_the_cpu_lacks`).
