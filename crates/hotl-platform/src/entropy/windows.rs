//! `getrandom`, which is `ProcessPrng` on Windows — no file to open, so none of the `/dev/urandom` failure modes exist.

use super::Entropy;
use std::io;

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsEntropy;

impl WindowsEntropy {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for WindowsEntropy {}

impl Entropy for WindowsEntropy {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        // The error is propagated rather than absorbed: the contract forbids a
        // fallback, and a caller that cannot get entropy must fail loudly.
        getrandom::fill(buf).map_err(|e| match e.raw_os_error() {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::other(format!("the OS CSPRNG refused: {e}")),
        })
    }
}
