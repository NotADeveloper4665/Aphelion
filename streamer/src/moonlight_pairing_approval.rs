//! Binds operator-entered pairing PINs to one pending Moonlight client identity.
//! The worker owns HTTP/TLS sockets and approval UI. Pending requests expire quickly,
//! and approval consumes a request so one PIN cannot be rebound to another client.
use std::time::{Duration, Instant};
use crate::moonlight_pairing::{MoonlightPairingTransaction, PairingError};

const MAX_PENDING_AGE: Duration = Duration::from_secs(120);
const MAX_ID_BYTES: usize = 128;
const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingPairingError {
    InvalidIdentity, InvalidSalt, InvalidCertificateEncoding, PayloadTooLarge, Expired,
    Transaction(PairingError),
}
impl std::fmt::Display for PendingPairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIdentity => f.write_str("pending Moonlight client identity is invalid"),
            Self::InvalidSalt => f.write_str("Moonlight pairing salt must be 16 bytes encoded as 32 hex digits"),
            Self::InvalidCertificateEncoding => f.write_str("Moonlight client certificate is not valid hexadecimal DER"),
            Self::PayloadTooLarge => f.write_str("pending Moonlight client certificate exceeds the configured size limit"),
            Self::Expired => f.write_str("Moonlight pairing request expired before operator approval"),
            Self::Transaction(error) => write!(f, "cannot begin approved Moonlight pairing: {error}"),
        }
    }
}
impl std::error::Error for PendingPairingError {}

/// One bounded pairing request, fixed to its identity, salt and certificate.
pub struct PendingMoonlightPairing {
    client_id: String,
    client_name: String,
    salt: [u8; 16],
    client_certificate_der: Vec<u8>,
    created_at: Instant,
}
impl PendingMoonlightPairing {
    pub fn new(
        client_id: impl Into<String>, client_name: impl Into<String>,
        salt_hex: &str, client_certificate_hex: &str,
    ) -> Result<Self, PendingPairingError> {
        let client_id = client_id.into();
        let client_name = client_name.into();
        if !valid_identity(&client_id) || !valid_identity(&client_name) {
            return Err(PendingPairingError::InvalidIdentity);
        }
        let salt_bytes = decode_hex(salt_hex).map_err(|_| PendingPairingError::InvalidSalt)?;
        let salt: [u8; 16] = salt_bytes.try_into().map_err(|_| PendingPairingError::InvalidSalt)?;
        if client_certificate_hex.len() > MAX_CERTIFICATE_BYTES * 2 {
            return Err(PendingPairingError::PayloadTooLarge);
        }
        let client_certificate_der = decode_hex(client_certificate_hex)
            .map_err(|_| PendingPairingError::InvalidCertificateEncoding)?;
        if client_certificate_der.is_empty() || client_certificate_der.len() > MAX_CERTIFICATE_BYTES {
            return Err(PendingPairingError::PayloadTooLarge);
        }
        Ok(Self { client_id, client_name, salt, client_certificate_der, created_at: Instant::now() })
    }

    pub fn is_expired(&self) -> bool { self.created_at.elapsed() > MAX_PENDING_AGE }

    /// Consume this request and bind the supplied PIN to exactly this client identity.
    pub fn approve(
        self, pin: &str, server_certificate_der: &[u8], server_private_key_pkcs8_der: &[u8],
    ) -> Result<MoonlightPairingTransaction, PendingPairingError> {
        self.approve_at(Instant::now(), pin, server_certificate_der, server_private_key_pkcs8_der)
    }

    fn approve_at(
        self, now: Instant, pin: &str,
        server_certificate_der: &[u8], server_private_key_pkcs8_der: &[u8],
    ) -> Result<MoonlightPairingTransaction, PendingPairingError> {
        if now.saturating_duration_since(self.created_at) > MAX_PENDING_AGE {
            return Err(PendingPairingError::Expired);
        }
        MoonlightPairingTransaction::new(
            &self.salt, pin, server_certificate_der, server_private_key_pkcs8_der,
            self.client_id, self.client_name, &self.client_certificate_der,
        ).map_err(PendingPairingError::Transaction)
    }
}
fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}
fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    if value.is_empty() || value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    let mut result = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        result.push((nibble(pair[0]).ok_or(())? << 4) | nibble(pair[1]).ok_or(())?);
    }
    Ok(result)
}
fn nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SERVER_CERT: &[u8] = include_bytes!("../tests/fixtures/server-cert.der");
    const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/server-key.der");
    const CLIENT_CERT: &[u8] = include_bytes!("../tests/fixtures/client-cert.der");
    fn hex(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02X}")).collect() }

    #[test]
    fn approval_consumes_and_binds_pending_identity() {
        let pending = PendingMoonlightPairing::new("phone-uuid", "Phone", &hex(&[0x44; 16]), &hex(CLIENT_CERT)).unwrap();
        assert!(!pending.is_expired());
        let mut transaction = pending.approve("0427", SERVER_CERT, SERVER_KEY).unwrap();
        assert_eq!(transaction.get_server_certificate().unwrap(), SERVER_CERT);
    }

    #[test]
    fn rejects_invalid_identity_salt_certificate_and_expired_approval() {
        assert_eq!(PendingMoonlightPairing::new("", "Phone", &hex(&[0; 16]), &hex(CLIENT_CERT)).err(), Some(PendingPairingError::InvalidIdentity));
        assert_eq!(PendingMoonlightPairing::new("id", "Phone", "xyz", &hex(CLIENT_CERT)).err(), Some(PendingPairingError::InvalidSalt));
        assert_eq!(PendingMoonlightPairing::new("id", "Phone", &hex(&[0; 16]), "xz").err(), Some(PendingPairingError::InvalidCertificateEncoding));
        let pending = PendingMoonlightPairing::new("id", "Phone", &hex(&[0x44; 16]), &hex(CLIENT_CERT)).unwrap();
        let expired = pending.created_at + MAX_PENDING_AGE + Duration::from_millis(1);
        assert_eq!(pending.approve_at(expired, "0427", SERVER_CERT, SERVER_KEY).err(), Some(PendingPairingError::Expired));
    }
}
