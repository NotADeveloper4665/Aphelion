//! Validates launch-provided Moonlight session keys and creates per-session ciphers.
//! The caller must authenticate the GameStream launch request before using this module.
use zeroize::{Zeroize, Zeroizing};

use crate::{
    crypto::{CryptoError, MoonlightVideoCipher},
    moonlight_rtsp_crypto::{MoonlightRtspCipher, RtspCryptoError},
    moonlight_sdp::MoonlightStreamProfile,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchCryptoError {
    InvalidKeyEncoding,
    InvalidKeyLength(usize),
    Rtsp(RtspCryptoError),
    Video(CryptoError),
}
impl std::fmt::Display for LaunchCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKeyEncoding => f.write_str("Moonlight rikey must contain only hexadecimal digits"),
            Self::InvalidKeyLength(n) => write!(f, "Moonlight rikey decoded to {n} bytes; expected 16"),
            Self::Rtsp(error) => write!(f, "cannot initialize encrypted RTSP: {error}"),
            Self::Video(error) => write!(f, "cannot initialize encrypted video: {error}"),
        }
    }
}
impl std::error::Error for LaunchCryptoError {}

/// Parsed per-launch Moonlight key. Secret bytes are cleared when dropped.
pub struct MoonlightLaunchCrypto {
    key: Zeroizing<[u8; 16]>,
}
impl MoonlightLaunchCrypto {
    /// Parse the 32-hex-character `rikey` launch parameter.
    pub fn from_rikey_hex(encoded: &str) -> Result<Self, LaunchCryptoError> {
        if encoded.len() % 2 != 0 || !encoded.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(LaunchCryptoError::InvalidKeyEncoding);
        }
        let mut decoded = Vec::with_capacity(encoded.len() / 2);
        for pair in encoded.as_bytes().chunks_exact(2) {
            decoded.push((hex_nibble(pair[0]).ok_or(LaunchCryptoError::InvalidKeyEncoding)? << 4)
                | hex_nibble(pair[1]).ok_or(LaunchCryptoError::InvalidKeyEncoding)?);
        }
        let key_result: Result<[u8; 16], _> = decoded.as_slice().try_into();
        let decoded_len = decoded.len();
        decoded.zeroize();
        let key = key_result.map_err(|_| LaunchCryptoError::InvalidKeyLength(decoded_len))?;
        Ok(Self { key: Zeroizing::new(key) })
    }

    /// Create only the ciphers enabled by launch negotiation, each scoped to this key.
    /// Follow the encryption flags negotiated in the client's ANNOUNCE SDP.
    pub fn establish_for_profile(
        self,
        encrypted_rtsp: bool,
        profile: &MoonlightStreamProfile,
    ) -> Result<NegotiatedMoonlightCiphers, LaunchCryptoError> {
        self.establish(encrypted_rtsp, profile.video_encryption_enabled())
    }

    pub fn establish(
        self,
        encrypted_rtsp: bool,
        encrypted_video: bool,
    ) -> Result<NegotiatedMoonlightCiphers, LaunchCryptoError> {
        let rtsp = encrypted_rtsp.then(|| MoonlightRtspCipher::new(self.key.as_ref()))
            .transpose().map_err(LaunchCryptoError::Rtsp)?;
        let video = encrypted_video.then(|| MoonlightVideoCipher::new(self.key.as_ref(), 0))
            .transpose().map_err(LaunchCryptoError::Video)?;
        Ok(NegotiatedMoonlightCiphers { rtsp, video })
    }
}
fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
pub struct NegotiatedMoonlightCiphers {
    pub rtsp: Option<MoonlightRtspCipher>,
    pub video: Option<MoonlightVideoCipher>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launch_key_and_obeys_encryption_negotiation() {
        let launch = MoonlightLaunchCrypto::from_rikey_hex("00112233445566778899aAbBcCdDeEfF").unwrap();
        let plain = launch.establish(false, false).unwrap();
        assert!(plain.rtsp.is_none());
        assert!(plain.video.is_none());
        let encrypted = MoonlightLaunchCrypto::from_rikey_hex("00112233445566778899aAbBcCdDeEfF")
            .unwrap().establish(true, true).unwrap();
        assert!(encrypted.rtsp.is_some());
        assert!(encrypted.video.is_some());
    }

    #[test]
    fn rejects_malformed_and_wrong_length_keys_and_clears_decode_buffer() {
        assert_eq!(MoonlightLaunchCrypto::from_rikey_hex("xyz").err(), Some(LaunchCryptoError::InvalidKeyEncoding));
        assert_eq!(MoonlightLaunchCrypto::from_rikey_hex("001").err(), Some(LaunchCryptoError::InvalidKeyEncoding));
        assert_eq!(MoonlightLaunchCrypto::from_rikey_hex("0011").err(), Some(LaunchCryptoError::InvalidKeyLength(2)));
        assert_eq!(MoonlightLaunchCrypto::from_rikey_hex("").err(), Some(LaunchCryptoError::InvalidKeyLength(0)));
    }

    #[test]
    fn different_launch_keys_produce_different_video_ciphertexts() {
        let first = MoonlightLaunchCrypto::from_rikey_hex(&"11".repeat(16)).unwrap();
        let second = MoonlightLaunchCrypto::from_rikey_hex(&"22".repeat(16)).unwrap();
        let mut first_video = first.establish(false, true).unwrap().video.unwrap();
        let mut second_video = second.establish(false, true).unwrap().video.unwrap();
        let a = first_video.seal_shard(1, b"payload").unwrap();
        let b = second_video.seal_shard(1, b"payload").unwrap();
        assert_ne!(&a[16..32], &b[16..32]);
    }
}
