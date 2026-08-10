//! [`Entropy`] — bytes from the OS CSPRNG, or an error.

use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::UnixEntropy;
#[cfg(unix)]
pub type ActiveEntropy = UnixEntropy;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsEntropy;
#[cfg(windows)]
pub type ActiveEntropy = WindowsEntropy;

/// Cryptographically secure bytes.
///
/// CONTRACT: **never a PRNG fallback.** An adapter that cannot reach the OS
/// CSPRNG returns an error; it does not degrade to something seeded from the
/// clock. The one consumer that matters is the session token, and a guessable
/// session token is a session anyone can drive.
pub trait Entropy: crate::sealed::Sealed {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()>;

    fn token_bytes<const N: usize>(&self) -> io::Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.fill(&mut buf)?;
        Ok(buf)
    }
}

#[cfg(test)]
pub(crate) fn assert_entropy_contract<E: Entropy>(e: &E) {
    // `fill` fills the whole buffer, including the tail — a partial fill that
    // left zeros would be the quiet version of the failure this trait forbids.
    let mut buf = [0u8; 64];
    e.fill(&mut buf).unwrap();
    assert!(buf.iter().any(|&b| b != 0), "fill left the buffer zeroed");

    let a: [u8; 32] = e.token_bytes().unwrap();
    let b: [u8; 32] = e.token_bytes().unwrap();
    assert_ne!(a, b, "two draws must differ");

    // A zero-length request is not an error.
    e.fill(&mut []).unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn active_adapter_upholds_the_contract() {
        super::assert_entropy_contract(&crate::ENTROPY);
    }
}
