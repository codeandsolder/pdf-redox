//! Reproducible digests for exact source and output PDF binding.

use sha2::{Digest, Sha256};

/// Lowercase SHA-256 of exact bytes, used to tie semantic edits to a PDF.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut text, "{byte:02x}");
    }
    text
}
