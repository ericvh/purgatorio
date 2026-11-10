//! plumb [-s src] [-d dest] [-w wdir] [-t type] [-a name value] ... [-i]
//! data ...: send a message to the plumber through `/chan/plumb.input`.
//! The data is the arguments joined by spaces, or standard input with
//! `-i` (which also adds `action=showdata` unless an action is given).

use std::io::{Read, Write};

use plumbmsg::Msg;
use purgatorio::abi::mode;
use purgatorio::{Context, Error};

const USAGE: &str =
    "usage: plumb [-s src] [-d dest] [-w wdir] [-t type] [-a name value] [-i] data ...";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut m = Msg {
        src: String::from("plumb"),
        dir: purgatorio::dot().unwrap_or_default(),
        kind: String::from("text"),
        ..Msg::default()
    };
    let mut attrs = Vec::new();
    let mut stdin = false;
    let mut i = 1;
    let next = |i: &mut usize| -> Result<String, Error> {
        *i += 1;
        args.get(*i - 1).cloned().ok_or_else(|| Error::from(USAGE))
    };
    while i < args.len() && args[i].starts_with('-') && args[i].len() > 1 {
        let opt = args[i].clone();
        i += 1;
        match opt.as_str() {
            "--" => break,
            "-s" => m.src = next(&mut i)?,
            "-d" => m.dst = next(&mut i)?,
            "-w" => m.dir = next(&mut i)?,
            "-t" => m.kind = next(&mut i)?,
            "-a" => {
                let name = next(&mut i)?;
                let value = next(&mut i)?;
                attrs.push((name, value));
            }
            "-i" => stdin = true,
            _ => return Err(Error::from(USAGE)),
        }
    }
    let rest = &args[i..];
    if stdin {
        if !rest.is_empty() {
            return Err(Error::from(USAGE));
        }
        std::io::stdin().read_to_end(&mut m.data)?;
        if plumbmsg::lookup(&attrs, "action").is_none() {
            attrs.push((String::from("action"), String::from("showdata")));
        }
    } else {
        if rest.is_empty() {
            return Err(Error::from(USAGE));
        }
        m.data = rest.join(" ").into_bytes();
    }
    m.attr = plumbmsg::attrs2string(&attrs);
    let fd = purgatorio::open(plumbmsg::INPUT, mode::OWRITE)
        .map_err(|e| Error(format!("can't open {}: {e}", plumbmsg::INPUT)))?;
    std::fs::File::from(fd).write_all(&m.pack())?;
    Ok(())
}
