//! dbm/keys dbfile: write each key of the dbm database `dbfile` on a line,
//! in the database's own order.

use std::io::Write;

use dbm::Db;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, file] = args.as_slice() else {
        return Err(Error::from("usage: dbm/keys dbfile"));
    };
    let mut db = match Db::open(file, true) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("dbm/keys: {file}: {}", Error::from(e));
            purgatorio::exits("fail:open");
        }
    };
    let mut out = Vec::new();
    for (k, _) in db.pairs().map_err(Error)? {
        out.extend_from_slice(&k);
        out.push(b'\n');
    }
    std::io::stdout().write_all(&out).map_err(Error::from)
}
