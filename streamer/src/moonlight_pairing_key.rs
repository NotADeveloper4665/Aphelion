//! The PIN-derived AES key used by the legacy Moonlight pairing exchange.
//!
//! This is deliberately only the key-derivation primitive. It does not establish
//! client identity, verify certificates or signatures, or authorize a stream. A
//! caller must not treat this key as proof that pairing succeeded.

use sha2::{Digest, Sha256};
use zeroize::Zeroize;

const PIN_BYTES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingKeyError {
    InvalidPin,
}

impl std::fmt::Display for PairingKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPin => formatter.write_str("Moonlight pairing PIN must contain exactly four ASCII digits"),
        }
    }
}

impl std::error::Error for PairingKeyError {}

/// A 128-bit AES key derived from a 16-byte Moonlight pairing salt and PIN.
///
/// Moonlight's pairing salt arrives as hexadecimal text on the pairing endpoint;
/// parse that representation at the HTTP boundary and pass the decoded bytes here.
/// The key is intentionally not `Clone` or `Debug` and is wiped when dropped.
pub struct MoonlightPairingKey([u8; 16]);

impl MoonlightPairingKey {
    pub fn from_pin(salt: &[u8; 16], pin: &str) -> Result<Self, PairingKeyError> {
        if pin.len() != PIN_BYTES || !pin.bytes().all(|digit| digit.is_ascii_digit()) {
            return Err(PairingKeyError::InvalidPin);
        }

        let mut digest = Sha256::new();
        digest.update(salt);
        digest.update(pin.as_bytes());
        let hash = digest.finalize();
        let mut key = [0_u8; 16];
        key.copy_from_slice(&hash[..16]);
        Ok(Self(key))
    }

    /// Exposes key bytes only for an immediate cryptographic operation.
    pub fn expose_for_crypto(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Drop for MoonlightPairingKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_first_128_bits_of_sha256_salt_then_pin() {
        let salt: [u8; 16] = std::array::from_fn(|index| index as u8);
        let key = MoonlightPairingKey::from_pin(&salt, "1234").unwrap();
        assert_eq!(key.expose_for_crypto(), &[
            0xba, 0xd0, 0xb4, 0xf7, 0xca, 0xe0, 0x8e, 0xb7,
            0xc1, 0xb5, 0xac, 0xc7, 0x63, 0xa8, 0xed, 0x25,
        ]);
    }

    #[test]
    fn accepts_leading_zero_and_rejects_malformed_pin() {
        let salt = [0_u8; 16];
        let key = MoonlightPairingKey::from_pin(&salt, "0000").unwrap();
        assert_eq!(key.expose_for_crypto(), &[
            0x25, 0xbe, 0xc9, 0x80, 0x0c, 0x43, 0xd6, 0x0d,
            0x49, 0xdf, 0xfa, 0x02, 0xb3, 0xcc, 0xd4, 0x93,
        ]);
        for pin in ["", "123", "12345", "12a4", "１２３４"] {
            assert!(matches!(MoonlightPairingKey::from_pin(&salt, pin), Err(PairingKeyError::InvalidPin)));
        }
    }
}
