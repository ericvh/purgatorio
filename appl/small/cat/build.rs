//! Links the command with a small stack. The linker's default is 1 MiB;
//! these commands keep one 8 KiB buffer and a few frames on it, and with
//! 32 KiB the stack, the data and the arguments fit in one 64 KiB page.

fn main() {
    println!("cargo:rustc-link-arg-bins=-zstack-size=32768");
}
