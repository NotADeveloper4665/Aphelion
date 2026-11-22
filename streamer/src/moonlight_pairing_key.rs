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
    InvalidSalt,
}

impl std::fmt::Display for PairingKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPin => formatter.write_str("Moonlight pairing PIN must contain exactly four ASCII digits"),
            Self::InvalidSalt => formatter.write_str("Moonlight pairing salt must contain exactly 32 hexadecimal characters"),
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

/// Decode the 16-byte salt's 32-character hexadecimal RTSP/HTTP representation.
pub fn parse_pairing_salt_hex(input: &str) -> Result<[u8; 16], PairingKeyError> {
    if input.len() != 32 {
        return Err(PairingKeyError::InvalidSalt);
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let bytes = input.as_bytes();
    let mut salt = [0_u8; 16];
    for (index, output) in salt.iter_mut().enumerate() {
        let high = nibble(bytes[index * 2]).ok_or(PairingKeyError::InvalidSalt)?;
        let low = nibble(bytes[index * 2 + 1]).ok_or(PairingKeyError::InvalidSalt)?;
        *output = (high << 4) | low;
    }
    Ok(salt)
}

impl MoonlightPairingKey {
    pub fn from_pin(salt: &[u8; 16], pin: &str) -> Result<Self, PairingKeyError> {
        if pin.len() != PIN_BYTES || !pin.bytes().all(|digit| digit.is_ascii_digit()) {
            return Err(PairingKeyError::InvalidPin);
        }

        let mut digest = Sha256::new();
        digest.update(salt);
        digest.update(pin.as_bytes());
        let mut hash: [u8; 32] = digest.finalize().into();
        let mut key = [0_u8; 16];
        key.copy_from_slice(&hash[..16]);
        hash.zeroize();
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
            0x06, 0xc5, 0x17, 0x5f, 0x3f, 0x41, 0xbb, 0x1c,
            0x8d, 0x50, 0x63, 0xc1, 0x93, 0x66, 0xa6, 0x43,
        ]);
        for pin in ["", "123", "12345", "12a4", "１２３４"] {
            assert!(matches!(MoonlightPairingKey::from_pin(&salt, pin), Err(PairingKeyError::InvalidPin)));
        }
    }

    #[test]
    fn parses_pairing_salt_hex_with_case_and_rejects_bad_width_or_digits() {
        assert_eq!(parse_pairing_salt_hex("00112233445566778899aAbBcCdDeEfF").unwrap(), [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ]);
        for salt in ["", "0011", "00112233445566778899aabbccddeef", "00112233445566778899aabbccddEEgG"] {
            assert!(matches!(parse_pairing_salt_hex(salt), Err(PairingKeyError::InvalidSalt)));
        }
        let non_ascii_at_odd_byte = format!("0é{}", "0".repeat(29));
        assert!(matches!(parse_pairing_salt_hex(&non_ascii_at_odd_byte), Err(PairingKeyError::InvalidSalt)));
    }
}
