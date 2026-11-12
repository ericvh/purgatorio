//! dbm/list dbfile: write each key of the dbm database `dbfile`, a tab and
//! its value on a line, in the database's own order.

use std::io::Write;

use dbm::Db;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, file] = args.as_slice() else {
        return Err(Error::from("usage: dbm/list dbfile"));
    };
    let mut db = match Db::open(file, true) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("dbm/list: {file}: {}", Error::from(e));
            purgatorio::exits("fail:open");
        }
    };
    let mut out = Vec::new();
    for (k, v) in db.pairs().map_err(Error)? {
        out.extend_from_slice(&k);
        out.push(b'\t');
        out.extend_from_slice(&v);
        out.push(b'\n');
    }
    std::io::stdout().write_all(&out).map_err(Error::from)
}
