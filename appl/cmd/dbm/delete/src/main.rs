//! dbm/delete dbfile key: delete `key` and its value from the dbm database
//! `dbfile`; `not found` on standard error when it is not there.

use dbm::Db;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, file, key] = args.as_slice() else {
        return Err(Error::from("usage: dbm/delete dbfile key"));
    };
    let mut db = match Db::open(file, false) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("dbm/delete: {file}: {}", Error::from(e));
            purgatorio::exits("fail:open");
        }
    };
    if !db.delete(key.as_bytes()).map_err(Error)? {
        eprintln!("not found");
    }
    Ok(())
}
