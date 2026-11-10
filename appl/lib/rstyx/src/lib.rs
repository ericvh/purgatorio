//! The remote command protocol (`docs/rcmd.md`): `rcmd` and `cpu` are its
//! clients ([`client`]), `auxi/rstyxd` its server. [`proto`] is the
//! framing both sides share, [`link`] the encryption of frames after the
//! opening, and [`io`] the few file descriptor helpers they need.

pub mod client;
pub mod io;
pub mod link;
pub mod proto;
