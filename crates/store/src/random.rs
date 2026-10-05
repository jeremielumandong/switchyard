//! Random identifiers and salts from the OS CSPRNG.

use chacha20poly1305::aead::Generate;

/// `n` random bytes.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    <[u8; N]>::generate()
}

/// `n` random bytes as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let bytes: [u8; 32] = random_bytes();
    hex(&bytes[..n.min(32)])
}

/// Lowercase hex encoding.
pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

/// Decode lowercase or uppercase hex.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip_and_uniqueness() {
        let a = random_hex(8);
        assert_eq!(a.len(), 16);
        assert_ne!(a, random_hex(8));
        assert_eq!(unhex(&hex(&[0, 255, 16])).unwrap(), [0, 255, 16]);
        assert!(unhex("abc").is_none());
    }
}
