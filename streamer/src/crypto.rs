//! Moonlight video-shard AES-128-GCM envelope.
//!
//! The session key must come from a completed authenticated Moonlight session.
//! This module does not implement pairing, key derivation, or any plaintext
//! fallback. The nonce layout follows the current video-stream domain: a
//! little-endian 64-bit counter, two zero bytes, and `V` as the final byte.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes128Gcm, Nonce,
};

pub const AES_GCM_TAG_BYTES: usize = 16;
pub const MOONLIGHT_VIDEO_PREFIX_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    InvalidKey,
    NonceExhausted,
    EncryptionFailed,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKey => formatter.write_str("Moonlight video key must be 128 bits"),
            Self::NonceExhausted => formatter.write_str("Moonlight video nonce counter exhausted; refusing nonce reuse"),
            Self::EncryptionFailed => formatter.write_str("AES-128-GCM encryption failed"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Encrypts packet shards for a single authenticated video session.
/// Construct a new instance only with a fresh per-session key. The generated
/// wire layout is IV[12], frame number LE[4], tag[16], ciphertext.
pub struct MoonlightVideoCipher {
    cipher: Aes128Gcm,
    next_counter: u64,
}

impl MoonlightVideoCipher {
    pub fn new(session_key: &[u8], initial_counter: u64) -> Result<Self, CryptoError> {
        let cipher = Aes128Gcm::new_from_slice(session_key).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { cipher, next_counter: initial_counter })
    }

    pub fn seal_shard(&mut self, frame_number: u32, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        // Consume a counter before invoking the cipher. Errors must not allow a
        // later packet to retry the same IV under this key.
        let counter = self.next_counter;
        self.next_counter = counter.checked_add(1).ok_or(CryptoError::NonceExhausted)?;

        let mut iv = [0_u8; 12];
        iv[..8].copy_from_slice(&counter.to_le_bytes());
        iv[11] = b'V';
        let sealed = self.cipher.encrypt(Nonce::from_slice(&iv), plaintext)
            .map_err(|_| CryptoError::EncryptionFailed)?;
        let ciphertext_length = sealed.len().checked_sub(AES_GCM_TAG_BYTES)
            .ok_or(CryptoError::EncryptionFailed)?;

        let mut wire = Vec::with_capacity(MOONLIGHT_VIDEO_PREFIX_BYTES + ciphertext_length);
        wire.extend_from_slice(&iv);
        wire.extend_from_slice(&frame_number.to_le_bytes());
        wire.extend_from_slice(&sealed[ciphertext_length..]); // GCM tag precedes the encrypted shard.
        wire.extend_from_slice(&sealed[..ciphertext_length]);
        Ok(wire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;

    #[test]
    fn aes128_gcm_matches_empty_plaintext_known_answer() {
        let cipher = Aes128Gcm::new_from_slice(&[0_u8; 16]).unwrap();
        let sealed = cipher.encrypt(Nonce::from_slice(&[0_u8; 12]), b"".as_ref()).unwrap();
        assert_eq!(sealed.as_slice(), &[
            0x58, 0xe2, 0xfc, 0xce, 0xfa, 0x7e, 0x30, 0x61,
            0x36, 0x7f, 0x1d, 0x57, 0xa4, 0xe7, 0x45, 0x5a,
        ]);
    }

    #[test]
    fn video_envelope_has_protocol_prefix_and_unique_counter_nonces() {
        let mut cipher = MoonlightVideoCipher::new(&[7_u8; 16], 9).unwrap();
        let first = cipher.seal_shard(0x1234_5678, b"frame shard").unwrap();
        let second = cipher.seal_shard(0x1234_5678, b"frame shard").unwrap();
        assert_eq!(&first[..8], &9_u64.to_le_bytes());
        assert_eq!(&first[8..11], &[0, 0, 0]);
        assert_eq!(first[11], b'V');
        assert_eq!(&first[12..16], &0x1234_5678_u32.to_le_bytes());
        assert_ne!(&first[..12], &second[..12]);
        assert_eq!(first.len(), MOONLIGHT_VIDEO_PREFIX_BYTES + b"frame shard".len());
    }

    #[test]
    fn modified_ciphertext_fails_authentication() {
        let key = [3_u8; 16];
        let mut sealer = MoonlightVideoCipher::new(&key, 1).unwrap();
        let mut wire = sealer.seal_shard(10, b"authenticated payload").unwrap();
        let nonce: [u8; 12] = wire[..12].try_into().unwrap();
        let tag = wire[16..32].to_vec();
        wire[MOONLIGHT_VIDEO_PREFIX_BYTES] ^= 1;
        let mut ciphertext_and_tag = wire[MOONLIGHT_VIDEO_PREFIX_BYTES..].to_vec();
        ciphertext_and_tag.extend_from_slice(&tag);
        let cipher = Aes128Gcm::new_from_slice(&key).unwrap();
        assert!(cipher.decrypt(Nonce::from_slice(&nonce), ciphertext_and_tag.as_ref()).is_err());
    }

    #[test]
    fn refuses_bad_key_size_and_counter_wrap() {
        assert!(matches!(MoonlightVideoCipher::new(&[0_u8; 15], 0), Err(CryptoError::InvalidKey)));
        let mut cipher = MoonlightVideoCipher::new(&[0_u8; 16], u64::MAX).unwrap();
        assert_eq!(cipher.seal_shard(0, b"x").unwrap_err(), CryptoError::NonceExhausted);
    }
}
