//! Module policy (`docs/signing.md`): each prog has a level, `none`,
//! `warn` or `require`, and every module it loads is checked against it.
//! The formats and the signature check are in `purg_auth::modsig`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;
use purg_auth::modsig::{self, DIGEST, Trust};

use crate::auth::{PUBLIC, read_file};
use crate::error::{Error, Result};
use crate::pgrp::Pgrp;
use crate::prog::Prog;

/// Keys trusted to sign modules or to certify module signers.
pub const MODULES: &str = "/lib/keys/modules";
/// Revoked keys and module digests, shared with mount authentication.
pub const REVOKED: &str = "/lib/keys/revoked";
/// Keys trusted to sign the kernel's ahead-of-time images, which are
/// native code (`purg-vm-aot`, `docs/signing.md`).
pub const AOT_KEYS: &str = "/lib/keys/aot";
/// The longest key list read.
const MAX_LIST: usize = 64 * 1024;

/// What happens to a module that does not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    /// It loads; nothing is checked.
    #[default]
    None,
    /// It loads after a warning on the loading prog's fd 2.
    Warn,
    /// It does not load.
    Require,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::None => "none",
            Level::Warn => "warn",
            Level::Require => "require",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "none" => Some(Level::None),
            "warn" => Some(Level::Warn),
            "require" => Some(Level::Require),
            _ => None,
        }
    }
}

/// A prog's module policy. Children inherit it.
#[derive(Clone, Default)]
pub struct ModPolicy {
    pub level: Level,
    /// The namespace the trust lists are read in: a copy of the prog's
    /// namespace made when the level first left `none`, so a confined
    /// prog cannot bind its own lists over them.
    pub trust: Option<Arc<Pgrp>>,
}

/// A module loaded into a prog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    /// The name it was loaded by.
    pub path: String,
    pub digest: [u8; DIGEST],
    /// The signing keys of its trusted signatures. Empty when none was
    /// verified.
    pub signers: Vec<[u8; PUBLIC]>,
}

impl ModPolicy {
    /// Checks a module's signatures against the lists in the pinned
    /// namespace. Returns the trusted signing keys.
    fn verify(
        &self,
        info: &modsig::Info<'_>,
        now: u64,
    ) -> core::result::Result<Vec<[u8; PUBLIC]>, &'static str> {
        let (anchors, revoked) = match &self.trust {
            Some(pgrp) => trust_lists(pgrp, MODULES),
            None => (Vec::new(), Some(Vec::new())),
        };
        modsig::check(
            info,
            &Trust {
                anchors: &anchors,
                revoked: revoked.as_deref(),
                now,
            },
        )
    }
}

/// Reads the key list `anchors` (such as [`MODULES`] or [`AOT_KEYS`]) and
/// the revocation list in `pgrp`. A missing anchor list trusts nobody. A
/// missing revocation list revokes nothing; one that is there but cannot
/// be read or parsed is `None`, which fails closed.
pub fn trust_lists(pgrp: &Pgrp, anchors: &str) -> (Vec<[u8; PUBLIC]>, Option<Vec<[u8; PUBLIC]>>) {
    // Module signatures are Ed25519: a hybrid id in either list names no
    // module signer, and is not a parse error.
    let keys = read_file(pgrp, anchors, MAX_LIST)
        .map(|t| purg_auth::keys::ed25519(&purg_auth::keys::parse_list_lenient(&t)))
        .unwrap_or_default();
    let revoked = match read_file(pgrp, REVOKED, MAX_LIST) {
        Some(t) => purg_auth::keys::parse_list(&t).map(|l| purg_auth::keys::ed25519(&l)),
        None if pgrp.namec(REVOKED, "/").is_ok() => None,
        None => Some(Vec::new()),
    };
    (keys, revoked)
}

impl Prog {
    /// Applies this prog's module policy to `wasm`, loaded as `path`, and
    /// records the module with its digest in the prog. Under `require` a
    /// module that does not pass is an error; under `warn` it is a line on
    /// fd 2.
    pub fn check_module(&mut self, path: &str, wasm: &[u8]) -> Result<()> {
        let info = modsig::inspect(wasm).map_err(|_| Error::new(error::EBADEXEC))?;
        let policy = self.proc.module_policy();
        let mut signers = Vec::new();
        if policy.level != Level::None {
            let now = self.kernel.platform.now_ns() / 1_000_000_000;
            match policy.verify(&info, now) {
                Ok(s) => signers = s,
                Err(e) if policy.level == Level::Require => return Err(Error::new(e)),
                Err(e) => {
                    let msg = alloc::format!("{path}: warning: {e}\n");
                    let _ = self.write(2, msg.as_bytes());
                }
            }
        }
        self.proc.add_module(Module {
            path: String::from(path),
            digest: info.digest,
            signers,
        });
        Ok(())
    }
}
