//! Arm semihosting, used only to exit QEMU (`-semihosting`).

use core::arch::asm;

/// Exits QEMU with `code` through `SYS_EXIT` (`ADP_Stopped_ApplicationExit`).
pub fn exit(code: u32) -> ! {
    const SYS_EXIT: u64 = 0x18;
    const APPLICATION_EXIT: u64 = 0x20026;
    let block = [APPLICATION_EXIT, u64::from(code)];
    // SAFETY: `hlt #0xf000` is the AArch64 semihosting trap; the block
    // outlives the call.
    unsafe {
        asm!("hlt #0xf000", in("x0") SYS_EXIT, in("x1") block.as_ptr(), options(nostack));
    }
    loop {
        // SAFETY: parks the core if semihosting is disabled.
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}
