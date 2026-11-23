//! Bounded raw AES-128-ECB operations for Moonlight's PIN pairing exchange.
//!
//! Moonlight encrypts fixed-size challenge/hash fields without PKCS#7 padding.
//! This module deliberately accepts only complete AES blocks and bounded input.
//! It does not implement authentication, certificate verification, or pairing state.

use aes::{
    cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit},
    Aes128,
};

pub const AES_BLOCK_BYTES: usize = 16;
pub const MAX_PAIRING_CIPHERTEXT_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingCipherError {
    InvalidKey,
    InvalidBlockLength,
    PayloadTooLarge,
}

impl std::fmt::Display for PairingCipherError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKey => formatter.write_str("Moonlight pairing key must be 128 bits"),
            Self::InvalidBlockLength => formatter.write_str("Moonlight pairing data must contain complete AES blocks"),
            Self::PayloadTooLarge => formatter.write_str("Moonlight pairing data exceeds the configured limit"),
        }
    }
}

impl std::error::Error for PairingCipherError {}

/// Encrypt one raw Moonlight pairing payload using AES-128-ECB.
///
/// Input and output lengths are identical. The caller must provide protocol
/// fields that are already block aligned; no padding is added.
pub fn encrypt_pairing_blocks(
    key: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, PairingCipherError> {
    transform_blocks(key, plaintext, true)
}

/// Decrypt one raw Moonlight pairing payload using AES-128-ECB.
///
/// Rejects empty, non-block-aligned, and oversized input before doing crypto.
pub fn decrypt_pairing_blocks(
    key: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, PairingCipherError> {
    transform_blocks(key, ciphertext, false)
}

fn transform_blocks(
    key: &[u8],
    input: &[u8],
    encrypt: bool,
) -> Result<Vec<u8>, PairingCipherError> {
    if key.len() != AES_BLOCK_BYTES {
        return Err(PairingCipherError::InvalidKey);
    }
    if input.is_empty() || input.len() % AES_BLOCK_BYTES != 0 {
        return Err(PairingCipherError::InvalidBlockLength);
    }
    if input.len() > MAX_PAIRING_CIPHERTEXT_BYTES {
        return Err(PairingCipherError::PayloadTooLarge);
    }

    let cipher = Aes128::new_from_slice(key).map_err(|_| PairingCipherError::InvalidKey)?;
    let mut output = Vec::with_capacity(input.len());
    for chunk in input.chunks_exact(AES_BLOCK_BYTES) {
        let mut block = GenericArray::clone_from_slice(chunk);
        if encrypt {
            cipher.encrypt_block(&mut block);
        } else {
            cipher.decrypt_block(&mut block);
        }
        output.extend_from_slice(block.as_slice());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_aes128_ecb_known_answer_without_padding() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ];
        let plaintext = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let expected = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30,
            0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a,
        ];
        let encrypted = encrypt_pairing_blocks(&key, &plaintext).unwrap();
        assert_eq!(encrypted, expected);
        assert_eq!(decrypt_pairing_blocks(&key, &encrypted).unwrap(), plaintext);
    }

    #[test]
    fn processes_multiple_complete_blocks_and_round_trips() {
        let key = [0x5a; AES_BLOCK_BYTES];
        let payload: Vec<u8> = (0..64).map(|value| value as u8).collect();
        let encrypted = encrypt_pairing_blocks(&key, &payload).unwrap();
        assert_eq!(encrypted.len(), payload.len());
        assert_eq!(decrypt_pairing_blocks(&key, &encrypted).unwrap(), payload);
    }

    #[test]
    fn rejects_invalid_key_and_malformed_or_oversized_payloads() {
        assert_eq!(
            encrypt_pairing_blocks(&[0; 15], &[0; 16]).unwrap_err(),
            PairingCipherError::InvalidKey
        );
        for payload in [&[][..], &[0; 1][..], &[0; 15][..], &[0; 17][..]] {
            assert_eq!(
                decrypt_pairing_blocks(&[0; 16], payload).unwrap_err(),
                PairingCipherError::InvalidBlockLength
            );
        }
        let oversized = vec![0; MAX_PAIRING_CIPHERTEXT_BYTES + AES_BLOCK_BYTES];
        assert_eq!(
            encrypt_pairing_blocks(&[0; 16], &oversized).unwrap_err(),
            PairingCipherError::PayloadTooLarge
        );
    }
}
