use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error(String);

impl Error {
    pub fn msg(s: impl Into<String>) -> Self {
        Error(s.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

/// Adds `.context("...")` and `.with_context(|| ...)` to any `Result`
/// whose error implements `Display`. Mirrors the small slice of `anyhow`
/// we actually used.
pub trait Context<T> {
    fn context(self, ctx: &'static str) -> Result<T>;
    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, ctx: &'static str) -> Result<T> {
        self.map_err(|e| Error(format!("{}: {}", ctx, e)))
    }
    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T> {
        self.map_err(|e| Error(format!("{}: {}", f(), e)))
    }
}
