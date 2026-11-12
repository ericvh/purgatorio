//! dbm/fetch dbfile key: write the value stored under `key` in the dbm
//! database `dbfile`, as it is, with nothing after it; `not found` on
//! standard error when there is none.

use std::io::Write;

use dbm::Db;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, file, key] = args.as_slice() else {
        return Err(Error::from("usage: dbm/fetch dbfile key"));
    };
    let mut db = match Db::open(file, true) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("dbm/fetch: {file}: {}", Error::from(e));
            purgatorio::exits("fail:open");
        }
    };
    match db.fetch(key.as_bytes()).map_err(Error)? {
        Some(v) => std::io::stdout().write_all(&v).map_err(Error::from)?,
        None => eprintln!("not found"),
    }
    Ok(())
}
