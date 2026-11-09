use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;

/// An Inferno error string (`errstr`). The well-known strings live in
/// [`purg_abi::error`]; devices and remote servers may produce any text.
#[derive(Clone, PartialEq, Eq)]
pub struct Error(Cow<'static, str>);

pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    pub const fn new(s: &'static str) -> Error {
        Error(Cow::Borrowed(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&'static str> for Error {
    fn from(s: &'static str) -> Error {
        Error(Cow::Borrowed(s))
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error(Cow::Owned(s))
    }
}

impl From<Cow<'static, str>> for Error {
    fn from(s: Cow<'static, str>) -> Error {
        Error(s)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error({:?})", &*self.0)
    }
}
