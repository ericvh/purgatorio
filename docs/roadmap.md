# Roadmap

The roadmap lives in GitHub issues, so it never conflicts between branches
and needs no commits to update:
[issues](https://github.com/ericvh/purgatorio/issues), grouped by
[milestone](https://github.com/ericvh/purgatorio/milestones) (K1, K2, K3,
K4, Phase G) and by label (`phase-k`, `phase-g`, `commands`, `abi`,
`testing`, `research`, `gap`, `macos`, `security`, `later`). Issue titles
keep the old `PG-n` and `Kn`/`Gn` names. The design and each milestone's
acceptance criteria are in [plan.md](plan.md).

From a checkout: `gh issue list --repo ericvh/purgatorio`, or
`gh issue list --milestone "K3 devices"`.

The items finished before the move are below, for the record.

## Done

- [x] **PG-1** Architecture and roadmap plan ([plan.md](plan.md)).
- [x] **PG-2** Code repository.
- [x] **PG-6** M1: hosted emu with WASI on wasmtime and wasmi (#1).
- [x] **PG-7** M1: bare-metal aarch64 probe image (#2).
- [x] **PG-8** M2a: progs, pipes, env, bind (#3).
- [x] **PG-9** M2b: shell and core apps (#4).
- [x] **PG-10** M2c: probe kernel threads and RAM root (#5).
- [x] **PG-11** M3: Styx servers, mount, `/prog`, file2chan (#6).
- [x] **PG-12** M4: networking (#7).
  - [x] **PG-13** Host dial, listen, export and import.
  - [x] **PG-14** virtio-net and smoltcp on the probe.
- [x] **PG-15** M5: shell, module loading, auth (#8).
  - [x] **PG-17** `load std` and `load expr` in the shell.
  - [x] **PG-21** `load` and `call` of a reactor module.
  - [x] **PG-23** Authenticated mount (Ed25519 challenge).
  - [x] **PG-18** The rest of WASI preview 1.
  - [x] **PG-25** Factotum device `#¤`.
  - [x] **PG-24** TLS 1.3 client device `#D`.
- [x] **PG-3** Hosted emu milestones as stacked PRs.
- [x] **PG-22** Land the stack tip onto `main` (#9).
- [x] **PG-19** Document the Rust toolchain in the README.
- [x] **PG-20** Record the wasm64 decision ([wasm64.md](wasm64.md)).
- [x] **PG-26** macOS CI: gate the ABI section to wasm (#10).
- [x] **PG-27** Bring the README and ABI doc up to M5 (#12).
- [x] **PG-31** W3/W4 apps: sh `load string` and `load regex` (#13); `tar`,
  `gettar`/`puttar`/`lstar`, `gzip`/`gunzip` (#14); `mntgen`, `ndb/query`,
  `logfile`, `cron` (#15).
- [x] ABI 0.5: `fauth` and `stream` as `purgatorio` imports, the SDK's
  `auth::amount`, and `import -A`.
