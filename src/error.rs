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

/// Render an error together with everything it wraps.
///
/// `reqwest::Error`'s `Display` is only the kind and the URL — "error
/// sending request for url (https://…)". The part an operator needs ("dns
/// error: Name or service not known", "invalid peer certificate: Expired",
/// "tcp connect error: Connection refused") lives in the `source()` chain
/// underneath it, so a bare `{e}` reports that a request failed and nothing
/// about why. Same for `tungstenite::Error` around a TLS or io error.
///
/// Segments already present in the accumulated text are skipped: the
/// hyper/rustls/io layers repeat each other's message verbatim often
/// enough that the chain reads as "x: x: x" without it.
pub fn chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let segment = e.to_string();
        if !out.contains(&segment) {
            out.push_str(": ");
            out.push_str(&segment);
        }
        source = e.source();
    }
    out
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
        let e = r
            .with_context(|| format!("widget {} failed", 42))
            .unwrap_err();
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

    /// A two-level error, shaped like the reqwest → hyper → rustls chain
    /// whose tail `Display` alone throws away.
    #[derive(Debug)]
    struct Layered {
        message: &'static str,
        inner: Option<Box<Layered>>,
    }

    impl fmt::Display for Layered {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Layered {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.inner
                .as_deref()
                .map(|e| e as &(dyn std::error::Error + 'static))
        }
    }

    fn layered(messages: &[&'static str]) -> Layered {
        let mut it = messages.iter().rev();
        let last = it.next().expect("at least one message");
        let mut error = Layered {
            message: last,
            inner: None,
        };
        for message in it {
            error = Layered {
                message,
                inner: Some(Box::new(error)),
            };
        }
        error
    }

    #[test]
    fn chain_appends_every_source() {
        let e = layered(&[
            "error sending request for url (https://example.invalid/x)",
            "client error (Connect)",
            "invalid peer certificate: Expired",
        ]);
        assert_eq!(
            chain(&e),
            "error sending request for url (https://example.invalid/x): \
             client error (Connect): invalid peer certificate: Expired"
        );
    }

    #[test]
    fn chain_skips_repeated_segments() {
        // io::Error re-Displayed by each wrapper is the common case.
        let e = layered(&["connection refused", "connection refused"]);
        assert_eq!(chain(&e), "connection refused");
    }

    #[test]
    fn chain_of_a_leaf_error_is_its_display() {
        assert_eq!(chain(&Error::msg("no sources here")), "no sources here");
    }
}
