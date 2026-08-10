//! `getrandom`, which is `getrandom(2)` where the kernel has it and `/dev/urandom` otherwise.

use super::Entropy;
use std::io;

#[derive(Debug, Clone, Copy, Default)]
pub struct UnixEntropy;

impl UnixEntropy {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for UnixEntropy {}

impl Entropy for UnixEntropy {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        // The error is propagated rather than absorbed: the contract forbids a
        // fallback, and a caller that cannot get entropy must fail loudly.
        getrandom::fill(buf).map_err(|e| match e.raw_os_error() {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::other(format!("the OS CSPRNG refused: {e}")),
        })
    }
}
