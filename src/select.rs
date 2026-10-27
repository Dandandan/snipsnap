//! Conditional moves and optimization barriers for the hot loops.
//!
//! The asm versions are used on x86_64 and aarch64; elsewhere, and under
//! Miri (which cannot run inline assembly), plain Rust stands in.

/// Hides the value of `value` from the optimizer, so that it stays in its
/// own register instead of being rederived from wherever it came from.
#[inline(always)]
pub(crate) fn opaque(value: usize) -> usize {
    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
    {
        let mut value = value;
        // SAFETY: the empty assembly block only claims to modify `value`.
        unsafe {
            std::arch::asm!(
                "/* {0} */",
                inout(reg) value,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        value
    }
    #[cfg(any(not(any(target_arch = "x86_64", target_arch = "aarch64")), miri))]
    {
        value
    }
}

/// Generates `fn $name(selector, if_zero, otherwise) -> $ty` returning
/// `if_zero` when `selector == 0` and `otherwise` if not, using a conditional
/// move.
///
/// The decoder's fast loop selects between literal and copy this way, and the
/// encoder between copy encodings. Written as plain Rust, LLVM may turn such
/// a select into a branch, which mispredicts on most real data, or (for the
/// decoder's next tag) narrow it to a byte and re-extend it at the top of the
/// loop, adding a cycle to the loop-carried dependency chain.
macro_rules! select_if_zero {
    ($name:ident, $ty:ty) => {
        #[inline(always)]
        #[allow(
            clippy::pointers_in_nomem_asm_block,
            reason = "the assembly only moves the pointer values"
        )]
        pub(crate) fn $name(selector: usize, if_zero: $ty, otherwise: $ty) -> $ty {
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            {
                let mut result = otherwise;
                // SAFETY: register-only instructions without memory access.
                unsafe {
                    std::arch::asm!(
                        "test {s}, {s}",
                        "cmovz {r}, {z}",
                        s = in(reg) selector,
                        z = in(reg) if_zero,
                        r = inout(reg) result,
                        options(pure, nomem, nostack),
                    );
                }
                result
            }
            #[cfg(all(target_arch = "aarch64", not(miri)))]
            {
                let result;
                // SAFETY: register-only instructions without memory access.
                unsafe {
                    std::arch::asm!(
                        "cmp {s}, #0",
                        "csel {r}, {z}, {o}, eq",
                        s = in(reg) selector,
                        z = in(reg) if_zero,
                        o = in(reg) otherwise,
                        r = lateout(reg) result,
                        options(pure, nomem, nostack),
                    );
                }
                result
            }
            #[cfg(any(not(any(target_arch = "x86_64", target_arch = "aarch64")), miri))]
            {
                if selector == 0 { if_zero } else { otherwise }
            }
        }
    };
}

select_if_zero!(select_if_zero, usize);
select_if_zero!(select_ptr_if_zero, *const u8);
