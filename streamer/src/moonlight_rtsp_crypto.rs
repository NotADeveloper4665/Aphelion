//! AES-GCM framing for encrypted Moonlight RTSP records.
use aes_gcm::{aead::{Aead, KeyInit}, Aes128Gcm, Nonce};

const HEADER: usize = 24;
const TAG: usize = 16;
const ENCRYPTED: u32 = 0x8000_0000;
pub const MAX_RTSP_PLAINTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspCryptoError {
    InvalidKey, InvalidFrame, CleartextFrame, PayloadTooLarge,
    ReplayOrReorderedSequence, SequenceExhausted, AuthenticationFailed,
}
impl std::fmt::Display for RtspCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidKey => "encrypted RTSP key must be 128 bits",
            Self::InvalidFrame => "malformed encrypted RTSP record",
            Self::CleartextFrame => "expected encrypted RTSP record",
            Self::PayloadTooLarge => "encrypted RTSP record exceeds the configured limit",
            Self::ReplayOrReorderedSequence => "encrypted RTSP sequence is repeated or out of order",
            Self::SequenceExhausted => "encrypted RTSP sequence exhausted; refusing nonce reuse",
            Self::AuthenticationFailed => "encrypted RTSP AES-GCM authentication failed",
        })
    }
}
impl std::error::Error for RtspCryptoError {}

/// Per-launch cipher with distinct client and host nonce domains.
pub struct MoonlightRtspCipher {
    cipher: Aes128Gcm,
    next_host_sequence: u32,
    last_client_sequence: Option<u32>,
}
impl MoonlightRtspCipher {
    pub fn new(key: &[u8]) -> Result<Self, RtspCryptoError> {
        let cipher = Aes128Gcm::new_from_slice(key).map_err(|_| RtspCryptoError::InvalidKey)?;
        Ok(Self { cipher, next_host_sequence: 0, last_client_sequence: None })
    }

    /// Authenticate/decrypt one complete client record: header, tag, ciphertext.
    pub fn open_client_record(&mut self, record: &[u8]) -> Result<Vec<u8>, RtspCryptoError> {
        if record.len() < 4 { return Err(RtspCryptoError::InvalidFrame); }
        let tagged_len = u32::from_be_bytes(record[..4].try_into().map_err(|_| RtspCryptoError::InvalidFrame)?);
        if tagged_len & ENCRYPTED == 0 { return Err(RtspCryptoError::CleartextFrame); }
        if record.len() < HEADER { return Err(RtspCryptoError::InvalidFrame); }
        let length = (tagged_len & !ENCRYPTED) as usize;
        if length == 0 { return Err(RtspCryptoError::InvalidFrame); }
        if length > MAX_RTSP_PLAINTEXT_BYTES { return Err(RtspCryptoError::PayloadTooLarge); }
        if record.len() != HEADER + length { return Err(RtspCryptoError::InvalidFrame); }
        let sequence = u32::from_be_bytes(record[4..8].try_into().map_err(|_| RtspCryptoError::InvalidFrame)?);
        if self.last_client_sequence.is_some_and(|last| sequence <= last) {
            return Err(RtspCryptoError::ReplayOrReorderedSequence);
        }
        let mut nonce = [0; 12];
        nonce[..4].copy_from_slice(&sequence.to_be_bytes());
        nonce[10] = b'C'; nonce[11] = b'R';
        let mut ciphertext_and_tag = Vec::with_capacity(length + TAG);
        ciphertext_and_tag.extend_from_slice(&record[HEADER..]);
        ciphertext_and_tag.extend_from_slice(&record[8..HEADER]);
        let plaintext = self.cipher.decrypt(Nonce::from_slice(&nonce), ciphertext_and_tag.as_ref())
            .map_err(|_| RtspCryptoError::AuthenticationFailed)?;
        self.last_client_sequence = Some(sequence);
        Ok(plaintext)
    }

    /// Encrypt one complete host RTSP response, consuming a fresh sequence number.
    pub fn seal_host_record(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, RtspCryptoError> {
        if plaintext.is_empty() { return Err(RtspCryptoError::InvalidFrame); }
        if plaintext.len() > MAX_RTSP_PLAINTEXT_BYTES { return Err(RtspCryptoError::PayloadTooLarge); }
        let sequence = self.next_host_sequence.checked_add(1).ok_or(RtspCryptoError::SequenceExhausted)?;
        self.next_host_sequence = sequence;
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&(sequence as u64).to_le_bytes());
        nonce[10] = b'H'; nonce[11] = b'R';
        let sealed = self.cipher.encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| RtspCryptoError::AuthenticationFailed)?;
        let ciphertext_len = sealed.len().checked_sub(TAG).ok_or(RtspCryptoError::InvalidFrame)?;
        let mut record = Vec::with_capacity(HEADER + plaintext.len());
        record.extend_from_slice(&(ENCRYPTED | plaintext.len() as u32).to_be_bytes());
        record.extend_from_slice(&sequence.to_be_bytes());
        record.extend_from_slice(&sealed[ciphertext_len..]);
        record.extend_from_slice(&sealed[..ciphertext_len]);
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;

    fn client_record(key: &[u8;16], sequence: u32, plain: &[u8]) -> Vec<u8> {
        let mut nonce=[0;12]; nonce[..4].copy_from_slice(&sequence.to_be_bytes());
        nonce[10]=b'C'; nonce[11]=b'R';
        let cipher=Aes128Gcm::new_from_slice(key).unwrap();
        let sealed=cipher.encrypt(Nonce::from_slice(&nonce),plain).unwrap();
        let split=sealed.len()-TAG;
        let mut record=Vec::new();
        record.extend_from_slice(&(ENCRYPTED | plain.len() as u32).to_be_bytes());
        record.extend_from_slice(&sequence.to_be_bytes());
        record.extend_from_slice(&sealed[split..]);
        record.extend_from_slice(&sealed[..split]);
        record
    }

    #[test]
    fn opens_client_and_seals_host_with_direction_separated_nonces() {
        let key=[0x41;16]; let mut cipher=MoonlightRtspCipher::new(&key).unwrap();
        let request=b"OPTIONS * RTSP/1.0\r\nCSeq: 9\r\n\r\n";
        assert_eq!(cipher.open_client_record(&client_record(&key,7,request)).unwrap(),request);
        let response=b"RTSP/1.0 200 OK\r\nCSeq: 9\r\n\r\n";
        let record=cipher.seal_host_record(response).unwrap();
        assert_eq!(u32::from_be_bytes(record[..4].try_into().unwrap()),ENCRYPTED | response.len() as u32);
        assert_eq!(u32::from_be_bytes(record[4..8].try_into().unwrap()),1);
        let mut nonce=[0;12]; nonce[..8].copy_from_slice(&1_u64.to_le_bytes()); nonce[10]=b'H'; nonce[11]=b'R';
        let mut sealed=record[HEADER..].to_vec(); sealed.extend_from_slice(&record[8..HEADER]);
        let decryptor=Aes128Gcm::new_from_slice(&key).unwrap();
        assert_eq!(decryptor.decrypt(Nonce::from_slice(&nonce),sealed.as_ref()).unwrap(),response);
    }

    #[test]
    fn rejects_bad_tag_replay_cleartext_and_oversized_records() {
        let key=[0x22;16]; let mut cipher=MoonlightRtspCipher::new(&key).unwrap();
        let valid=client_record(&key,1,b"OPTIONS * RTSP/1.0\r\n\r\n");
        let mut altered=valid.clone(); let last=altered.len()-1; altered[last]^=0x80;
        assert_eq!(cipher.open_client_record(&altered),Err(RtspCryptoError::AuthenticationFailed));
        assert!(cipher.open_client_record(&valid).is_ok());
        assert_eq!(cipher.open_client_record(&valid),Err(RtspCryptoError::ReplayOrReorderedSequence));
        assert_eq!(cipher.open_client_record(b"OPTIONS * RTSP/1.0\r\n\r\n"),Err(RtspCryptoError::CleartextFrame));
        let mut large=Vec::new();
        large.extend_from_slice(&(ENCRYPTED | (MAX_RTSP_PLAINTEXT_BYTES as u32+1)).to_be_bytes());
        large.extend_from_slice(&2_u32.to_be_bytes()); large.extend_from_slice(&[0;TAG]);
        assert_eq!(cipher.open_client_record(&large),Err(RtspCryptoError::PayloadTooLarge));
    }

    #[test]
    fn host_sequence_starts_at_one_and_empty_records_are_rejected() {
        let mut cipher=MoonlightRtspCipher::new(&[0;16]).unwrap();
        assert_eq!(cipher.seal_host_record(&[]),Err(RtspCryptoError::InvalidFrame));
        let record=cipher.seal_host_record(b"x").unwrap();
        assert_eq!(u32::from_be_bytes(record[4..8].try_into().unwrap()),1);
    }
}
