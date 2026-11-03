//! Which AMD cores the CPU is, for the two choices that depend on more than
//! its instruction set: how long literals are copied (see
//! `bytes::copy_long`) and how the decoder copies elements (see
//! `decode::adapts_copy_width`).
//!
//! CPUID is read once and the answer cached.

use std::sync::atomic::{AtomicU8, Ordering};

/// Set once the CPU has been looked at.
const KNOWN: u8 = 1;
/// AMD family 19h: Zen 3 and Zen 4.
const FAMILY_19H: u8 = 2;
/// AMD family 19h with a Zen 3 model number.
const ZEN3: u8 = 4;

/// Whether the CPU is an AMD family 19h core: Zen 3 or Zen 4.
#[inline]
pub(crate) fn amd_family_19h() -> bool {
    amd() & FAMILY_19H != 0
}

/// Whether the CPU is an AMD Zen 3 core (family 19h, models 00h-0Fh,
/// 20h-2Fh and 40h-5Fh).
#[inline]
pub(crate) fn amd_zen3() -> bool {
    amd() & ZEN3 != 0
}

fn amd() -> u8 {
    static STATE: AtomicU8 = AtomicU8::new(0);
    let state = STATE.load(Ordering::Relaxed);
    if state != 0 {
        return state;
    }
    #[allow(unused_unsafe)]
    // SAFETY: CPUID is available on every x86-64 CPU.
    let (vendor, signature) = unsafe {
        use std::arch::x86_64::__cpuid;
        (__cpuid(0), __cpuid(1).eax)
    };
    let mut state = KNOWN;
    let amd = (vendor.ebx, vendor.edx, vendor.ecx)
        == (
            u32::from_le_bytes(*b"Auth"),
            u32::from_le_bytes(*b"enti"),
            u32::from_le_bytes(*b"cAMD"),
        );
    // The family is the base family plus the extended one; the model puts
    // the extended model above the base one.
    let family = ((signature >> 8) & 0xf) + ((signature >> 20) & 0xff);
    let model = ((signature >> 4) & 0xf) | (((signature >> 16) & 0xf) << 4);
    if amd && family == 0x19 {
        state |= FAMILY_19H;
        if matches!(model, 0x00..=0x0f | 0x20..=0x2f | 0x40..=0x5f) {
            state |= ZEN3;
        }
    }
    STATE.store(state, Ordering::Relaxed);
    state
}
