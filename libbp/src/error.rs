//! Errors carry the `errno` value the C API reports to the caller.

use std::fmt;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BpError {
    pub errno: i32,
    pub msg: String,
}

impl BpError {
    pub fn new(errno: i32, msg: impl Into<String>) -> Self {
        BpError {
            errno,
            msg: msg.into(),
        }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(libc::EINVAL, msg)
    }

    /// A receive deadline passed with nothing to deliver (what SO_RCVTIMEO reports).
    pub fn timeout() -> Self {
        Self::new(libc::EAGAIN, "receive timed out")
    }

    /// A signal interrupted a blocking receive.
    pub fn interrupted() -> Self {
        Self::new(libc::EINTR, "interrupted by signal")
    }

    pub fn protocol(msg: impl Into<String>) -> Self {
        Self::new(libc::EIO, msg)
    }

    pub fn from_io(err: &io::Error) -> Self {
        let errno = match err.raw_os_error() {
            Some(e) => e,
            None => match err.kind() {
                io::ErrorKind::ConnectionRefused => libc::ECONNREFUSED,
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => libc::ETIMEDOUT,
                io::ErrorKind::ConnectionReset => libc::ECONNRESET,
                io::ErrorKind::NotFound => libc::EHOSTUNREACH,
                _ => libc::EIO,
            },
        };
        BpError::new(errno, err.to_string())
    }
}

impl fmt::Display for BpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (errno {})", self.msg, self.errno)
    }
}

impl std::error::Error for BpError {}

pub type Result<T> = std::result::Result<T, BpError>;
