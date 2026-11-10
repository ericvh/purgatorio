//! newer newfile oldfile: succeed if `newfile` exists and is no older
//! than `oldfile`, which must also exist. Neither may be a directory. The
//! failure statuses say which file was at fault: `new:...`, `old:...` or
//! `older`.

use purgatorio::{Context, DMDIR, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, new, old] = args.as_slice() else {
        return Err(Error::from("usage: newer newfile oldfile"));
    };
    let n = match purgatorio::stat(new) {
        Ok(d) if d.mode & DMDIR != 0 => purgatorio::exits("fail:new:directory"),
        Ok(d) => d,
        Err(e) => purgatorio::exits(&format!("fail:new:{e}")),
    };
    let o = match purgatorio::stat(old) {
        Ok(d) if d.mode & DMDIR != 0 => purgatorio::exits("fail:old:directory"),
        Ok(d) => d,
        Err(e) => purgatorio::exits(&format!("fail:old:{e}")),
    };
    if n.mtime < o.mtime {
        purgatorio::exits("fail:older");
    }
    Ok(())
}
