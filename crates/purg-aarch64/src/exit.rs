//! Leaving QEMU: semihosting `SYS_EXIT` under TCG, PSCI `SYSTEM_OFF` where
//! there is no semihosting (KVM). The method starts as semihosting and is
//! set from the DTB with [`set_method`] once it has been read.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::board::ExitMethod;
use crate::semihost;

const SEMIHOSTING: u8 = 0;
const PSCI_HVC: u8 = 1;
const PSCI_SMC: u8 = 2;

static METHOD: AtomicU8 = AtomicU8::new(SEMIHOSTING);

/// Sets how [`exit`] leaves QEMU.
pub fn set_method(m: ExitMethod) {
    let v = match m {
        ExitMethod::Semihosting => SEMIHOSTING,
        ExitMethod::Psci { smc: false } => PSCI_HVC,
        ExitMethod::Psci { smc: true } => PSCI_SMC,
    };
    METHOD.store(v, Ordering::Release);
}

/// The method [`exit`] uses.
pub fn method() -> ExitMethod {
    match METHOD.load(Ordering::Acquire) {
        PSCI_HVC => ExitMethod::Psci { smc: false },
        PSCI_SMC => ExitMethod::Psci { smc: true },
        _ => ExitMethod::Semihosting,
    }
}

/// Leaves QEMU. Semihosting passes `code` on; PSCI `SYSTEM_OFF` cannot, so
/// callers print their verdict on the console first.
pub fn exit(code: u32) -> ! {
    const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
    match method() {
        // SAFETY: PSCI SYSTEM_OFF does not return; the conduit comes from
        // the DTB.
        ExitMethod::Psci { smc: false } => unsafe {
            asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(nostack))
        },
        // SAFETY: as above.
        ExitMethod::Psci { smc: true } => unsafe {
            asm!("smc #0", in("x0") PSCI_SYSTEM_OFF, options(nostack))
        },
        ExitMethod::Semihosting => semihost::exit(code),
    }
    loop {
        // SAFETY: parks the core if SYSTEM_OFF returned.
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}
