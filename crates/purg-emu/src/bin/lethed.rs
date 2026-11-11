//! `lethed`: serves a host directory read-only over Lethe. See
//! `purg_emu::lethed` and `docs/lethe.md`.

fn main() -> std::process::ExitCode {
    purg_emu::lethed::main()
}
