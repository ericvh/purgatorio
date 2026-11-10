//! Archive formats shared by the archive commands: POSIX ustar ([`ustar`]),
//! gzip streams ([`gz`]), and the namespace side of archiving, walking a
//! tree into an archive and unpacking one into the current directory
//! ([`fs`]).

pub mod fs;
pub mod gz;
pub mod ustar;

/// An Inferno-style permission string as `ls -l` prints it, from the
/// `Dir` mode bits.
pub fn mode_string(m: u32) -> String {
    let mut s = String::with_capacity(11);
    s.push(if m & purgatorio::DMDIR != 0 { 'd' } else { '-' });
    s.push('-');
    for shift in [6, 3, 0] {
        let b = m >> shift;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}
