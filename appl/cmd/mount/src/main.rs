//! mount [-abrcA] fd mtpt [spec]: mount a 9P connection on `fd` at `mtpt`.

use purgatorio::abi::mount;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "mount [-abrcA] fd mtpt [spec]");
    let (mut order, mut create) = (mount::MREPL, 0);
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'r' => order = mount::MREPL,
            'c' => create = mount::MCREATE,
            'A' => {}
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() < 2 || rest.len() > 3 {
        return Err(Error::from("usage: mount [-abrcA] fd mtpt [spec]"));
    }
    let fd: i32 = rest[0].parse().map_err(|_| Error::from("bad fd"))?;
    let spec = rest.get(2).map(String::as_str).unwrap_or("");
    purgatorio::mount(fd, -1, &rest[1], order | create, spec)
        .map_err(|e| Error(format!("mount: {e}")))
}
