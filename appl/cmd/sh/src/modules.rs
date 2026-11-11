//! The builtin modules compiled into sh, and the commands and `${}`
//! substitutions each one adds when it is loaded.
//!
//! Inferno loads sh modules as Dis files. Here they are part of sh itself,
//! so `load name` only records that the module's names are in use. A name
//! that is not one of these is loaded as a reactor from `/bin/lib`.

/// One compiled-in module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Module {
    Std,
    Expr,
    Mpexpr,
    String,
    Regex,
    Arg,
    Csv,
    Echo,
    File2chan,
    Mload,
    Sexprs,
    Test,
}

const ALL: &[Module] = &[
    Module::Std,
    Module::Expr,
    Module::Mpexpr,
    Module::String,
    Module::Regex,
    Module::Arg,
    Module::Csv,
    Module::Echo,
    Module::File2chan,
    Module::Mload,
    Module::Sexprs,
    Module::Test,
];

impl Module {
    pub fn by_name(name: &str) -> Option<Module> {
        ALL.iter().copied().find(|m| m.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Module::Std => "std",
            Module::Expr => "expr",
            Module::Mpexpr => "mpexpr",
            Module::String => "string",
            Module::Regex => "regex",
            Module::Arg => "arg",
            Module::Csv => "csv",
            Module::Echo => "echo",
            Module::File2chan => "file2chan",
            Module::Mload => "mload",
            Module::Sexprs => "sexprs",
            Module::Test => "test",
        }
    }

    /// The commands the module adds.
    pub fn cmds(self) -> &'static [&'static str] {
        match self {
            Module::Std => &[
                "if", "while", "for", "and", "or", "not", "~", "fn", "raise", "pctl",
            ],
            Module::Expr | Module::Mpexpr => &["ntest"],
            Module::String => crate::string::CMDS,
            Module::Regex => &["match"],
            Module::Arg => &["arg"],
            Module::Csv => &["getcsv"],
            Module::Echo => &["echo"],
            Module::File2chan => &[
                "file2chan",
                "rblock",
                "rread",
                "rreadone",
                "rwrite",
                "rerror",
                "fetchwdata",
                "putrdata",
            ],
            Module::Mload => &["mload", "munload"],
            Module::Sexprs => &["getsexprs", "islist"],
            Module::Test => &["report"],
        }
    }

    /// The `${}` substitutions the module adds.
    pub fn substs(self) -> &'static [&'static str] {
        match self {
            Module::Std => &["hd", "tl", "index", "quote", "unquote", "pid"],
            Module::Expr | Module::Mpexpr => &["expr"],
            Module::String => crate::string::SUBSTS,
            Module::Regex => &["re"],
            Module::Csv => &["csv"],
            Module::File2chan => &["rget"],
            Module::Sexprs => &[
                "els",
                "text",
                "b64",
                "textels",
                "mktext",
                "mklist",
                "mktextlist",
            ],
            Module::Arg | Module::Echo | Module::Mload | Module::Test => &[],
        }
    }
}

/// A module loaded with `load` or into an `mload` namespace.
#[derive(Clone, Debug, PartialEq)]
pub enum Loaded {
    Builtin(Module),
    /// A reactor from `/bin/lib/<name>.wasm`: one `${name}` substitution.
    Reactor(String, i32),
}

impl Loaded {
    pub fn name(&self) -> &str {
        match self {
            Loaded::Builtin(m) => m.name(),
            Loaded::Reactor(n, _) => n,
        }
    }

    pub fn has_cmd(&self, cmd: &str) -> bool {
        matches!(self, Loaded::Builtin(m) if m.cmds().contains(&cmd))
    }

    pub fn has_subst(&self, name: &str) -> bool {
        match self {
            Loaded::Builtin(m) => m.substs().contains(&name),
            Loaded::Reactor(n, _) => n == name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for m in ALL {
            assert_eq!(Module::by_name(m.name()), Some(*m));
        }
        assert_eq!(Module::by_name("tk"), None);
    }
}
