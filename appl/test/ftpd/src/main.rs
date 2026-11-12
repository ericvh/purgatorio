//! ftpd [-h home] [-u user:password] dir: a small FTP server for the
//! ftpfs tests. It announces `tcp!127.0.0.1!0`, writes `ftpd: addr` on
//! standard error, and serves the tree at `dir` (as `/`) to one client at
//! a time until killed: login as `anonymous` with any password or as the
//! `-u` user, `PWD` gives `home` (default `/`), data goes over passive
//! (`PASV`) or active (`PORT`) connections, and `LIST` answers as Unix
//! `ls -l` does, every file owned by `bob`, group `staff`. Commands: USER PASS SYST PWD CWD CDUP TYPE PASV PORT
//! LIST NLST RETR STOR DELE MKD RMD RNFR RNTO SIZE NOOP QUIT.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::mode::OREAD;
use purgatorio::dial::{self, Connection, local};
use purgatorio::{Context, DMDIR, Dir, Error, daytime};

struct Session {
    root: String,
    home: String,
    cwd: String,
    user: Option<String>,
    login: Option<(String, String)>,
    logged: bool,
    pasv: Option<Connection>,
    port: Option<String>,
    rnfr: Option<String>,
}

fn reply(out: &mut File, text: &str) -> std::io::Result<()> {
    out.write_all(format!("{text}\r\n").as_bytes())
}

fn clean(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for e in path.split('/') {
        match e {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            e => out.push(e),
        }
    }
    format!("/{}", out.join("/"))
}

fn mode_string(d: &Dir) -> String {
    let mut s = String::from(if d.mode & DMDIR != 0 { "d" } else { "-" });
    for shift in [6, 3, 0] {
        let b = d.mode >> shift;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn list_dir(path: &str) -> Result<Vec<Dir>, Error> {
    let fd = purgatorio::open(path, OREAD)?;
    let mut all = Vec::new();
    loop {
        let batch = purgatorio::dirread(fd.as_raw_fd())?;
        if batch.is_empty() {
            break;
        }
        all.extend(batch);
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

impl Session {
    /// The local path of a client's path.
    fn local(&self, arg: &str) -> String {
        let p = if arg.starts_with('/') {
            clean(arg)
        } else {
            clean(&format!("{}/{arg}", self.cwd))
        };
        format!("{}{}", self.root, if p == "/" { "" } else { &p })
    }

    fn virt(&self, arg: &str) -> String {
        if arg.starts_with('/') {
            clean(arg)
        } else {
            clean(&format!("{}/{arg}", self.cwd))
        }
    }

    /// The data connection for a transfer.
    fn data(&mut self) -> Result<File, Error> {
        if let Some(ann) = self.pasv.take() {
            let c = dial::listen(&ann.dir)?;
            return Ok(File::from(c.data.expect("a data file")));
        }
        if let Some(addr) = self.port.take() {
            let c = dial::dial(&addr)?;
            return Ok(File::from(c.data.expect("a data file")));
        }
        Err(Error::from("no data connection"))
    }

    fn command(&mut self, out: &mut File, line: &str) -> Result<bool, Error> {
        let (cmd, arg) = match line.split_once(' ') {
            Some((c, a)) => (c.to_ascii_uppercase(), a),
            None => (line.to_ascii_uppercase(), ""),
        };
        let r = |out: &mut File, t: &str| reply(out, t).map_err(Error::from);
        if !self.logged && !matches!(cmd.as_str(), "USER" | "PASS" | "QUIT") {
            r(out, "530 Please login with USER and PASS.")?;
            return Ok(true);
        }
        match cmd.as_str() {
            "USER" => {
                self.user = Some(String::from(arg));
                r(out, "331 Password required.")?;
            }
            "PASS" => {
                let ok = match (&self.user, &self.login) {
                    (Some(u), _) if u == "anonymous" => true,
                    (Some(u), Some((lu, lp))) => u == lu && arg == lp,
                    _ => false,
                };
                if ok {
                    self.logged = true;
                    r(out, "230-Welcome.")?;
                    r(out, "230 Logged in.")?;
                } else {
                    r(out, "530 Login incorrect.")?;
                }
            }
            "SYST" => r(out, "215 UNIX Type: L8")?,
            "NOOP" => r(out, "200 OK.")?,
            "TYPE" => r(out, "200 Type set.")?,
            "PWD" => r(
                out,
                &format!("257 \"{}\" is the current directory.", self.cwd),
            )?,
            "CWD" | "CDUP" => {
                let to = if cmd == "CDUP" {
                    String::from("..")
                } else {
                    String::from(arg)
                };
                let v = self.virt(&to);
                match purgatorio::stat(&self.local(&to)) {
                    Ok(d) if d.mode & DMDIR != 0 => {
                        self.cwd = v;
                        r(out, "250 OK.")?;
                    }
                    _ => r(out, "550 No such directory.")?,
                }
            }
            "PASV" => {
                let ann = dial::announce("tcp!127.0.0.1!0")?;
                let here = local(&ann.dir)?;
                let port: u16 = here
                    .rsplit('!')
                    .next()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                self.pasv = Some(ann);
                r(
                    out,
                    &format!(
                        "227 Entering Passive Mode (127,0,0,1,{},{}).",
                        port >> 8,
                        port & 255
                    ),
                )?;
            }
            "PORT" => {
                let n: Vec<u16> = arg
                    .split(',')
                    .filter_map(|x| x.trim().parse().ok())
                    .collect();
                if n.len() != 6 {
                    r(out, "501 Bad PORT.")?;
                } else {
                    self.port = Some(format!(
                        "tcp!{}.{}.{}.{}!{}",
                        n[0],
                        n[1],
                        n[2],
                        n[3],
                        n[4] * 256 + n[5]
                    ));
                    r(out, "200 PORT OK.")?;
                }
            }
            "LIST" | "NLST" => {
                let arg = if arg.starts_with('-') { "" } else { arg };
                let path = self.local(arg);
                let dirs = match purgatorio::stat(&path) {
                    Ok(d) if d.mode & DMDIR != 0 => list_dir(&path)?,
                    Ok(d) => vec![d],
                    Err(_) => {
                        r(out, "550 No such file.")?;
                        return Ok(true);
                    }
                };
                r(out, "150 Here it comes.")?;
                let mut data = self.data()?;
                let mut text = String::new();
                for d in dirs {
                    if cmd == "NLST" {
                        text.push_str(&format!("{}\r\n", d.name));
                        continue;
                    }
                    let when = daytime::filet(i64::from(d.mtime) + 400 * 86400, i64::from(d.mtime));
                    text.push_str(&format!(
                        "{} 1 {} {} {} {} {}\r\n",
                        mode_string(&d),
                        "bob",
                        "staff",
                        d.length,
                        when,
                        d.name
                    ));
                }
                let _ = data.write_all(text.as_bytes());
                drop(data);
                r(out, "226 Done.")?;
            }
            "RETR" => match std::fs::read(self.local(arg)) {
                Ok(bytes) => {
                    r(out, "150 Opening data connection.")?;
                    let mut data = self.data()?;
                    let _ = data.write_all(&bytes);
                    drop(data);
                    r(out, "226 Done.")?;
                }
                Err(_) => r(out, "550 No such file.")?,
            },
            "STOR" => {
                let path = self.local(arg);
                match File::create(&path) {
                    Ok(mut f) => {
                        r(out, "150 Ok to send data.")?;
                        let mut data = self.data()?;
                        let mut bytes = Vec::new();
                        let _ = data.read_to_end(&mut bytes);
                        f.write_all(&bytes).map_err(Error::from)?;
                        r(out, "226 Done.")?;
                    }
                    Err(_) => r(out, "553 Could not create file.")?,
                }
            }
            "SIZE" => match purgatorio::stat(&self.local(arg)) {
                Ok(d) if d.mode & DMDIR == 0 => r(out, &format!("213 {}", d.length))?,
                _ => r(out, "550 No such file.")?,
            },
            "DELE" | "RMD" => {
                let path = self.local(arg);
                let isdir = purgatorio::stat(&path).is_ok_and(|d| d.mode & DMDIR != 0);
                if isdir != (cmd == "RMD") {
                    r(out, "550 Wrong kind of file.")?;
                } else if purgatorio::remove(&path).is_ok() {
                    r(out, "250 Removed.")?;
                } else {
                    r(out, "550 Remove failed.")?;
                }
            }
            "MKD" => {
                let path = self.local(arg);
                match purgatorio::create(&path, OREAD, DMDIR | 0o775) {
                    Ok(_) => r(out, &format!("257 \"{}\" created.", self.virt(arg)))?,
                    Err(_) => r(out, "550 Create failed.")?,
                }
            }
            "RNFR" => {
                let path = self.local(arg);
                if purgatorio::stat(&path).is_ok() {
                    self.rnfr = Some(path);
                    r(out, "350 Ready for RNTO.")?;
                } else {
                    r(out, "550 No such file.")?;
                }
            }
            "RNTO" => {
                let to = self.virt(arg);
                let Some(from) = self.rnfr.take() else {
                    r(out, "503 RNFR first.")?;
                    return Ok(true);
                };
                // Only within a directory, as wstat renames.
                let name = to.rsplit('/').next().unwrap_or("");
                let mut d = Dir::null();
                d.name = String::from(name);
                match purgatorio::wstat(&from, &d) {
                    Ok(()) => r(out, "250 Renamed.")?,
                    Err(_) => r(out, "550 Rename failed.")?,
                }
            }
            "QUIT" => {
                r(out, "221 Goodbye.")?;
                return Ok(false);
            }
            _ => r(out, "502 Command not implemented.")?,
        }
        Ok(true)
    }
}

fn serve(
    conn: Connection,
    root: &str,
    home: &str,
    login: Option<(String, String)>,
) -> Result<(), Error> {
    let data = File::from(conn.data.expect("a data file"));
    let mut input = BufReader::new(data);
    let mut s = Session {
        root: String::from(root.trim_end_matches('/')),
        home: String::from(home),
        cwd: String::from(home),
        user: None,
        login,
        logged: false,
        pasv: None,
        port: None,
        rnfr: None,
    };
    s.cwd = s.home.clone();
    reply(input.get_mut(), "220 test ftpd ready.").map_err(Error::from)?;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).map_err(Error::from)? == 0 {
            return Ok(());
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if !s.command(input.get_mut(), l)? {
            return Ok(());
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let usage = || Error::from("usage: ftpd [-h home] [-u user:password] dir");
    let mut home = String::from("/");
    let mut login = None;
    let mut rest = &args[1..];
    loop {
        match rest {
            [f, v, tail @ ..] if f == "-h" => {
                home = v.clone();
                rest = tail;
            }
            [f, v, tail @ ..] if f == "-u" => {
                let (u, p) = v.split_once(':').ok_or_else(usage)?;
                login = Some((String::from(u), String::from(p)));
                rest = tail;
            }
            _ => break,
        }
    }
    let [dir] = rest else {
        return Err(usage());
    };
    let ann = dial::announce("tcp!127.0.0.1!0")?;
    eprintln!("ftpd: {}", local(&ann.dir)?);
    loop {
        let conn = dial::listen(&ann.dir)?;
        if let Err(e) = serve(conn, dir, &home, login.clone()) {
            eprintln!("ftpd: {e}");
        }
    }
}
