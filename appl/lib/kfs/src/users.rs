//! The user table kfs keeps in `adm/users` (users(6)):
//! `id:name:leader:member,...` lines. Ids are 16-bit on disk.

use super::NONE;

/// The conventional id of the group `noworld`, whose members get no
/// "other" permissions.
pub const NOWORLD: i16 = 9999;

#[derive(Clone, Debug)]
struct User {
    uid: i16,
    name: String,
    leader: i16,
    members: Vec<i16>,
}

/// The users kfs knows.
#[derive(Clone, Debug)]
pub struct Users {
    users: Vec<User>,
}

/// The table kfs uses when there is no `adm/users`.
const DEFAULT: &[(i16, &str, &str)] = &[
    (-1, "adm", "adm"),
    (NONE, "none", "adm"),
    (NOWORLD, "noworld", ""),
    (10000, "sys", ""),
    (10001, "upas", "upas"),
    (10002, "bootes", "bootes"),
    (10006, "inferno", ""),
];

fn bad_name(s: &str) -> bool {
    s.is_empty()
        || s.len() + 1 > super::NAMELEN
        || s == "."
        || s == ".."
        || s.bytes().any(|b| b <= b' ')
}

impl Default for Users {
    fn default() -> Users {
        let mut u = Users { users: Vec::new() };
        for &(id, name, leader) in DEFAULT {
            let lid = if leader == name {
                id
            } else if leader.is_empty() {
                0
            } else {
                u.uid(leader)
            };
            u.users.push(User {
                uid: id,
                name: String::from(name),
                leader: lid,
                members: Vec::new(),
            });
        }
        u
    }
}

impl Users {
    /// Parses an `adm/users` file. Errors are kfs's console messages.
    pub fn parse(text: &str) -> Result<Users, String> {
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if lines.is_empty() {
            return Err(String::from("empty /adm/users"));
        }
        let mut u = Users { users: Vec::new() };
        for l in &lines {
            let mut f = l.splitn(3, ':');
            let (Some(id), Some(name)) = (f.next(), f.next()) else {
                return Err(format!("invalid /adm/users line: {l}"));
            };
            let Ok(n) = id.parse::<i16>() else {
                return Err(format!("invalid /adm/users line: {l}"));
            };
            if n.to_string() != id || name.is_empty() {
                return Err(format!("invalid /adm/users line: {l}"));
            }
            if bad_name(name) || u.users.iter().any(|x| x.uid == n || x.name == name) {
                continue;
            }
            u.users.push(User {
                uid: n,
                name: String::from(name),
                leader: 0,
                members: Vec::new(),
            });
        }
        for l in &lines {
            let f: Vec<&str> = l.splitn(4, ':').collect();
            let name = f.get(1).copied().unwrap_or("");
            let lead = f.get(2).copied().unwrap_or("");
            let mem = f.get(3).copied().unwrap_or("");
            let members: Vec<i16> = mem
                .split([',', '\n'])
                .filter(|m| !m.is_empty())
                .map(|m| u.uid(m))
                .filter(|&m| m != NONE)
                .collect();
            let leader = if lead.is_empty() {
                None
            } else {
                let lu = u.uid(lead);
                Some(lu)
            };
            let Some(x) = u.users.iter_mut().find(|x| x.name == name) else {
                continue;
            };
            if let Some(lu) = leader {
                x.leader = if lu != NONE { lu } else { x.uid };
            }
            x.members = members;
        }
        Ok(u)
    }

    /// A user's id; an unknown name is `none`.
    pub fn uid(&self, name: &str) -> i16 {
        if name == "none" {
            return NONE;
        }
        self.users
            .iter()
            .find(|u| u.name == name)
            .map_or(NONE, |u| u.uid)
    }

    /// A user's name; an unknown id is `#<id>`.
    pub fn name(&self, id: i16) -> String {
        if id == NONE {
            return String::from("none");
        }
        self.users
            .iter()
            .find(|u| u.uid == id)
            .map_or_else(|| format!("#{id}"), |u| u.name.clone())
    }

    /// Whether `uid` is `gid` or a member of it.
    pub fn in_group(&self, uid: i16, gid: i16) -> bool {
        if uid == gid {
            return true;
        }
        self.users
            .iter()
            .find(|u| u.uid == gid)
            .is_some_and(|g| g.members.contains(&uid))
    }

    /// Whether `uid` leads group `gid` (a group with no leader is led by
    /// all its members).
    pub fn leads(&self, uid: i16, gid: i16) -> bool {
        match self.users.iter().find(|u| u.uid == gid) {
            Some(g) if g.leader == uid => true,
            Some(g) if g.leader == 0 => self.in_group(uid, gid),
            _ => false,
        }
    }
}
