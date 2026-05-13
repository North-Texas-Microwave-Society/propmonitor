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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_constructor_and_display_round_trip() {
        let e = Error::msg("something went wrong");
        assert_eq!(format!("{}", e), "something went wrong");
    }

    #[test]
    fn from_io_error_carries_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e: Error = io.into();
        assert!(format!("{}", e).contains("missing"));
    }

    #[test]
    fn from_string_and_str() {
        let e1: Error = String::from("a").into();
        let e2: Error = "b".into();
        assert_eq!(format!("{}", e1), "a");
        assert_eq!(format!("{}", e2), "b");
    }

    #[test]
    fn context_prefixes_static_str() {
        let r: std::result::Result<(), &str> = Err("boom");
        let e = r.context("widget failed").unwrap_err();
        assert_eq!(format!("{}", e), "widget failed: boom");
    }

    #[test]
    fn with_context_lazily_formats() {
        let r: std::result::Result<(), &str> = Err("boom");
        let e = r.with_context(|| format!("widget {} failed", 42)).unwrap_err();
        assert_eq!(format!("{}", e), "widget 42 failed: boom");
    }

    #[test]
    fn context_passes_through_ok() {
        let r: std::result::Result<i32, &str> = Ok(7);
        assert_eq!(r.context("ignored").unwrap(), 7);
    }

    #[test]
    fn error_implements_std_error() {
        // Trait-object usage forces the std::error::Error impl to compile
        // and exercises the bound from Debug.
        let e: Box<dyn std::error::Error> = Box::new(Error::msg("x"));
        assert_eq!(format!("{}", e), "x");
    }
}
