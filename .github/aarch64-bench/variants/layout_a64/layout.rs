//! Code alignment for Intel's JCC erratum.
//!
//! On cores from Skylake to Cascade Lake with Intel's microcode update, a
//! 32-byte block of code holding a jump that crosses or ends on its boundary
//! is not cached as decoded micro-ops. Hot loops with such jumps ran 10-20%
//! slower in the decoder, and the encoder compressed HTML and protobuf up to
//! 47% slower, depending only on where the linker placed them. These
//! directives pin the layout of the functions and loops that use them. They
//! are no-ops on other architectures and under Miri.

/// Aligns the start of the calling function to 64 bytes (on x86-64), so that
/// the layout of its loops does not depend on where the linker places it.
///
/// The directive raises the alignment of the function's section; it also
/// pads to 64 bytes where it stands, so it belongs on a path that is taken
/// once per call at most.
#[inline(always)]
pub(crate) fn align_function_64() {
    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
    // SAFETY: an assembler directive; the padding is executed as nops.
    unsafe {
        std::arch::asm!(".p2align 6", options(nomem, nostack, preserves_flags));
    }
}

/// Aligns the next instruction to 32 bytes (on x86-64). Placed at the top of
/// a loop in a function that uses [`align_function_64`], it fixes where the
/// loop's branches fall relative to 32-byte boundaries, at the cost of the
/// padding being executed as nops on every pass.
#[inline(always)]
pub(crate) fn align_32() {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    // SAFETY: an assembler directive; the padding is executed as nops.
    unsafe {
        std::arch::asm!(".p2align 5", options(nomem, nostack, preserves_flags));
    }
}
