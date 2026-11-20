# Top-level build. `make` builds the emu and every guest into root/bin.
# The real work is done by cargo and `cargo xtask` (xtask/); this file only
# checks the toolchain and names the usual steps.

CARGO ?= cargo
RUST_CHANNEL := $(shell sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)

.PHONY: all toolchain root emu run test check kernel probe clean help

all: root emu

help:
	@echo "make            build the emu and the guests (root/bin)"
	@echo "make run        start the emu with an interactive sh"
	@echo "make test       unit and acceptance tests, both VMs"
	@echo "make check      rustfmt and clippy, host and guests"
	@echo "make kernel     the native kernel under QEMU (needs qemu-system-aarch64)"
	@echo "make probe      the bare-metal probe under QEMU"
	@echo "make clean      remove target/"

# rust-toolchain.toml pins the compiler, components and targets. Only
# rustup reads it: a distro cargo ignores it and fails on edition 2024 or
# on crates that need a newer compiler.
toolchain:
	@command -v rustup >/dev/null 2>&1 || { \
	  echo "make: rustup is not installed. Install it from https://rustup.rs;"; \
	  echo "      a distro rustc/cargo package cannot build this tree."; exit 1; }
	@command -v cc >/dev/null 2>&1 || { \
	  echo "make: no C compiler (cc). ring and wasmtime build C code;"; \
	  echo "      install gcc or clang (Xcode command line tools on macOS)."; exit 1; }
	@rustup toolchain install >/dev/null 2>&1 || rustup show active-toolchain >/dev/null
	@$(CARGO) --version | grep -q " $(RUST_CHANNEL) " || { \
	  echo "make: '$(CARGO)' is `$(CARGO) --version`, not $(RUST_CHANNEL)."; \
	  echo "      Put ~/.cargo/bin ahead of any distro cargo in PATH."; exit 1; }

root: toolchain
	$(CARGO) xtask build-root

emu: toolchain
	$(CARGO) build -p purg-emu

run: all
	target/debug/emu -r root

test: root
	$(CARGO) test --workspace

check: toolchain
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) xtask clippy-guests

kernel: toolchain
	$(CARGO) xtask kernel

probe: toolchain
	$(CARGO) xtask probe

clean:
	$(CARGO) clean
