//! sha256 helpers. Wraps the `sha2` crate behind a tiny API so the rest of the
//! codebase never touches the hash implementation directly (and so tests can
//! swap in a different one).

use sha2::Digest as _;

pub struct Hasher(sha2::Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self(sha2::Sha256::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn hex(self) -> String {
        let digest = self.0.finalize();
        let mut out = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
        }
        out
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hex_of(bytes: &[u8]) -> String {
    let mut h = Hasher::new();
    h.update(bytes);
    h.hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digest() {
        assert_eq!(
            hex_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
