use std::fmt;
use std::io;
use std::net::IpAddr;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    InvalidUrl(String),
    UnsupportedScheme(String),
    Resolve(io::Error),
    UnsafeAddress(IpAddr),
    Connect(io::Error),
    Tls(String),
    Io(io::Error),
    BadResponse(String),
    ResponseTooLarge(usize),
    TooManyRedirects(u32),
    /// The request's hostname was on the bundled adblock blocklist.
    Blocked(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(s) => write!(f, "invalid url: {s}"),
            Self::UnsupportedScheme(s) => {
                write!(f, "unsupported scheme {s:?}; only http and https are allowed")
            }
            Self::Resolve(e) => write!(f, "dns resolution failed: {e}"),
            Self::UnsafeAddress(ip) => write!(
                f,
                "address {ip} is not globally routable; refusing to connect (ssrf guard)"
            ),
            Self::Connect(e) => write!(f, "connect failed: {e}"),
            Self::Tls(s) => write!(f, "tls error: {s}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::BadResponse(s) => write!(f, "malformed http response: {s}"),
            Self::ResponseTooLarge(limit) => write!(f, "response exceeded {limit}-byte cap"),
            Self::TooManyRedirects(limit) => write!(f, "exceeded {limit} redirect hops"),
            Self::Blocked(host) => write!(f, "request to {host} blocked by adblock"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
