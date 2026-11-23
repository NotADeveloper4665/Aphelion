//! Host-side Moonlight PIN pairing transaction.
//!
//! This is a protocol state machine bound to one pending client identity and one
//! operator-approved PIN. It verifies both challenge transcripts and the client's
//! RSA signature before returning a client record for durable authorization.
//! Certificate persistence, HTTP/XML routing, TLS setup, and approval UI remain
//! responsibilities of the embedding worker.

use rand::{rngs::OsRng, RngCore};
use rsa::{
    pkcs1::DecodeRsaPublicKey,
    pkcs1v15::{Signature as RsaSignature, SigningKey, VerifyingKey},
    pkcs8::DecodePrivateKey,
    signature::{SignatureEncoding, Signer, Verifier},
    traits::PublicKeyParts,
    RsaPrivateKey, RsaPublicKey,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x509_parser::prelude::{FromDer, X509Certificate};
use zeroize::Zeroize;

use crate::{
    moonlight_pairing_cipher::{decrypt_pairing_blocks, encrypt_pairing_blocks, PairingCipherError},
    moonlight_pairing_key::{MoonlightPairingKey, PairingKeyError},
};

const CHALLENGE_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;
const MIN_RSA_BITS: usize = 2048;
const MAX_RSA_BITS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingPhase {
    AwaitingServerCertificate,
    AwaitingClientChallenge,
    AwaitingServerChallengeResponse,
    AwaitingClientPairingSecret,
    Paired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingError {
    InvalidPinOrSalt,
    InvalidIdentity,
    InvalidCertificate,
    InvalidPrivateKey,
    IdentityKeyMismatch,
    UnexpectedStage,
    InvalidChallenge,
    InvalidClientProof,
    CryptoFailure,
    PayloadTooLarge,
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPinOrSalt => f.write_str("invalid Moonlight pairing PIN or salt"),
            Self::InvalidIdentity => f.write_str("invalid pending Moonlight client identity"),
            Self::InvalidCertificate => f.write_str("invalid or unsupported Moonlight client/server certificate"),
            Self::InvalidPrivateKey => f.write_str("invalid or unsupported Moonlight server private key"),
            Self::IdentityKeyMismatch => f.write_str("Moonlight server certificate does not match its private key"),
            Self::UnexpectedStage => f.write_str("Moonlight pairing request is out of order or the transaction is closed"),
            Self::InvalidChallenge => f.write_str("invalid Moonlight pairing challenge payload"),
            Self::InvalidClientProof => f.write_str("Moonlight client proof did not verify"),
            Self::CryptoFailure => f.write_str("Moonlight pairing cryptographic operation failed"),
            Self::PayloadTooLarge => f.write_str("Moonlight pairing identity or payload exceeds the configured limit"),
        }
    }
}

impl std::error::Error for PairingError {}

impl From<PairingKeyError> for PairingError {
    fn from(_: PairingKeyError) -> Self { Self::InvalidPinOrSalt }
}

impl From<PairingCipherError> for PairingError {
    fn from(error: PairingCipherError) -> Self {
        match error {
            PairingCipherError::PayloadTooLarge => Self::PayloadTooLarge,
            PairingCipherError::InvalidKey => Self::CryptoFailure,
            PairingCipherError::InvalidBlockLength => Self::InvalidChallenge,
        }
    }
}

/// Certificate and identity information the host may persist only after pairing succeeds.
#[derive(Clone, PartialEq, Eq)]
pub struct PairedClient {
    pub client_id: String,
    pub client_name: String,
    pub certificate_der: Vec<u8>,
}

pub struct MoonlightPairingTransaction {
    phase: PairingPhase,
    pairing_key: MoonlightPairingKey,
    server_certificate_der: Vec<u8>,
    server_certificate_signature: Vec<u8>,
    server_private_key: RsaPrivateKey,
    client_id: String,
    client_name: String,
    client_certificate_der: Vec<u8>,
    client_certificate_signature: Vec<u8>,
    client_public_key: RsaPublicKey,
    server_secret: Option<[u8; CHALLENGE_BYTES]>,
    server_challenge: Option<[u8; CHALLENGE_BYTES]>,
    client_hash: Option<[u8; HASH_BYTES]>,
    completed_proof_digest: Option<[u8; HASH_BYTES]>,
    paired_client: Option<PairedClient>,
}

impl MoonlightPairingTransaction {
    /// Create a transaction for the client whose PIN prompt is being approved.
    ///
    /// Client identity and its certificate are fixed here so an approval cannot be
    /// applied to a different pending client. Keys are RSA-2048 through RSA-4096.
    pub fn new(
        salt: &[u8; 16],
        pin: &str,
        server_certificate_der: &[u8],
        server_private_key_pkcs8_der: &[u8],
        client_id: impl Into<String>,
        client_name: impl Into<String>,
        client_certificate_der: &[u8],
    ) -> Result<Self, PairingError> {
        let client_id = client_id.into();
        let client_name = client_name.into();
        if !valid_identity(&client_id, 128) || !valid_identity(&client_name, 128) {
            return Err(PairingError::InvalidIdentity);
        }
        if server_certificate_der.len() > MAX_CERTIFICATE_BYTES
            || client_certificate_der.len() > MAX_CERTIFICATE_BYTES
        {
            return Err(PairingError::PayloadTooLarge);
        }

        let (server_public_key, server_certificate_signature) = parse_certificate(server_certificate_der)?;
        let (client_public_key, client_certificate_signature) = parse_certificate(client_certificate_der)?;
        validate_rsa_size(&server_public_key).map_err(|_| PairingError::InvalidCertificate)?;
        validate_rsa_size(&client_public_key).map_err(|_| PairingError::InvalidCertificate)?;
        let server_private_key = RsaPrivateKey::from_pkcs8_der(server_private_key_pkcs8_der)
            .map_err(|_| PairingError::InvalidPrivateKey)?;
        if server_private_key.n() != server_public_key.n()
            || server_private_key.e() != server_public_key.e()
        {
            return Err(PairingError::IdentityKeyMismatch);
        }
        let pairing_key = MoonlightPairingKey::from_pin(salt, pin)?;

        Ok(Self {
            phase: PairingPhase::AwaitingServerCertificate,
            pairing_key,
            server_certificate_der: server_certificate_der.to_vec(),
            server_certificate_signature,
            server_private_key,
            client_id,
            client_name,
            client_certificate_der: client_certificate_der.to_vec(),
            client_certificate_signature,
            client_public_key,
            server_secret: None,
            server_challenge: None,
            client_hash: None,
            completed_proof_digest: None,
            paired_client: None,
        })
    }

    pub fn phase(&self) -> PairingPhase { self.phase }

    /// Complete getservercert and return the server identity bound to this exchange.
    pub fn get_server_certificate(&mut self) -> Result<Vec<u8>, PairingError> {
        self.require_phase(PairingPhase::AwaitingServerCertificate)?;
        self.phase = PairingPhase::AwaitingClientChallenge;
        Ok(self.server_certificate_der.clone())
    }

    /// Process the client's AES-encrypted 16-byte challenge.
    /// Returns AES-encrypted SHA256(client challenge || server certificate signature ||
    /// server secret) followed by the 16-byte server challenge.
    pub fn client_challenge(&mut self, encrypted_challenge: &[u8]) -> Result<Vec<u8>, PairingError> {
        self.require_phase(PairingPhase::AwaitingClientChallenge)?;
        let mut challenge = match decrypt_pairing_blocks(
            self.pairing_key.expose_for_crypto(),
            encrypted_challenge,
        ) {
            Ok(value) if value.len() == CHALLENGE_BYTES => value,
            _ => return self.fail(PairingError::InvalidChallenge),
        };

        let mut server_secret = [0_u8; CHALLENGE_BYTES];
        let mut server_challenge = [0_u8; CHALLENGE_BYTES];
        OsRng.fill_bytes(&mut server_secret);
        OsRng.fill_bytes(&mut server_challenge);

        let mut transcript = Vec::with_capacity(
            challenge.len() + self.server_certificate_signature.len() + server_secret.len(),
        );
        transcript.extend_from_slice(&challenge);
        transcript.extend_from_slice(&self.server_certificate_signature);
        transcript.extend_from_slice(&server_secret);
        let hash = Sha256::digest(&transcript);
        transcript.zeroize();

        let mut response = Vec::with_capacity(HASH_BYTES + CHALLENGE_BYTES);
        response.extend_from_slice(&hash);
        response.extend_from_slice(&server_challenge);
        let encrypted = encrypt_pairing_blocks(self.pairing_key.expose_for_crypto(), &response);
        response.zeroize();
        challenge.zeroize();
        let encrypted = match encrypted {
            Ok(value) => value,
            Err(_) => return self.fail(PairingError::CryptoFailure),
        };

        self.server_secret = Some(server_secret);
        self.server_challenge = Some(server_challenge);
        self.phase = PairingPhase::AwaitingServerChallengeResponse;
        Ok(encrypted)
    }

    /// Process the client's AES-encrypted 32-byte transcript hash.
    /// Returns server-secret || RSA-SHA256(server-secret), as required by pairing.
    pub fn server_challenge_response(
        &mut self,
        encrypted_client_hash: &[u8],
    ) -> Result<Vec<u8>, PairingError> {
        self.require_phase(PairingPhase::AwaitingServerChallengeResponse)?;
        let mut client_hash = match decrypt_pairing_blocks(
            self.pairing_key.expose_for_crypto(),
            encrypted_client_hash,
        ) {
            Ok(value) if value.len() == HASH_BYTES => value,
            _ => return self.fail(PairingError::InvalidChallenge),
        };
        let Some(server_secret) = self.server_secret else {
            client_hash.zeroize();
            return self.fail(PairingError::UnexpectedStage);
        };

        let signing_key = SigningKey::<Sha256>::new(self.server_private_key.clone());
        let signature = signing_key.sign(&server_secret);
        let mut pairing_secret = Vec::with_capacity(CHALLENGE_BYTES + signature.to_bytes().len());
        pairing_secret.extend_from_slice(&server_secret);
        pairing_secret.extend_from_slice(&signature.to_bytes());
        self.client_hash = Some(client_hash.as_slice().try_into().map_err(|_| PairingError::InvalidChallenge)?);
        client_hash.zeroize();
        self.phase = PairingPhase::AwaitingClientPairingSecret;
        Ok(pairing_secret)
    }

    /// Verify the clear client secret and RSA signature, then return its certificate
    /// record for persistence. No record is returned unless both transcript and
    /// signature checks pass.
    pub fn client_pairing_secret(
        &mut self,
        client_pairing_secret: &[u8],
    ) -> Result<PairedClient, PairingError> {
        if self.phase == PairingPhase::Paired {
            let Some(paired_client) = self.paired_client.as_ref() else {
                return Err(PairingError::UnexpectedStage);
            };
            if client_pairing_secret.len() != CHALLENGE_BYTES + self.client_public_key.size() {
                return Err(PairingError::UnexpectedStage);
            }
            let retry_digest: [u8; HASH_BYTES] = Sha256::digest(client_pairing_secret).into();
            if self.completed_proof_digest.as_ref()
                .is_some_and(|digest| retry_digest.ct_eq(digest).unwrap_u8() == 1)
            {
                return Ok(paired_client.clone());
            }
            return Err(PairingError::UnexpectedStage);
        }
        self.require_phase(PairingPhase::AwaitingClientPairingSecret)?;
        let Some(server_challenge) = self.server_challenge else {
            return self.fail(PairingError::UnexpectedStage);
        };
        let Some(expected_client_hash) = self.client_hash else {
            return self.fail(PairingError::UnexpectedStage);
        };
        let signature_bytes = self.client_public_key.size();
        if client_pairing_secret.len() != CHALLENGE_BYTES + signature_bytes {
            return self.fail(PairingError::InvalidClientProof);
        }
        let secret = &client_pairing_secret[..CHALLENGE_BYTES];
        let signature = match RsaSignature::try_from(&client_pairing_secret[CHALLENGE_BYTES..]) {
            Ok(value) => value,
            Err(_) => return self.fail(PairingError::InvalidClientProof),
        };
        let verifying_key = VerifyingKey::<Sha256>::new(self.client_public_key.clone());
        if verifying_key.verify(secret, &signature).is_err() {
            return self.fail(PairingError::InvalidClientProof);
        }

        let mut transcript = Vec::with_capacity(
            server_challenge.len() + self.client_certificate_signature.len() + secret.len(),
        );
        transcript.extend_from_slice(&server_challenge);
        transcript.extend_from_slice(&self.client_certificate_signature);
        transcript.extend_from_slice(secret);
        let calculated_hash = Sha256::digest(&transcript);
        transcript.zeroize();
        if calculated_hash.as_slice().ct_eq(&expected_client_hash).unwrap_u8() != 1 {
            return self.fail(PairingError::InvalidClientProof);
        }

        let paired_client = PairedClient {
            client_id: self.client_id.clone(),
            client_name: self.client_name.clone(),
            certificate_der: self.client_certificate_der.clone(),
        };
        self.completed_proof_digest = Some(Sha256::digest(client_pairing_secret).into());
        self.paired_client = Some(paired_client.clone());
        self.client_hash.zeroize();
        self.server_secret.zeroize();
        self.server_challenge.zeroize();
        self.phase = PairingPhase::Paired;
        Ok(paired_client)
    }

    fn require_phase(&mut self, expected: PairingPhase) -> Result<(), PairingError> {
        if self.phase != expected {
            if self.phase != PairingPhase::Paired {
                self.phase = PairingPhase::Failed;
            }
            return Err(PairingError::UnexpectedStage);
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: PairingError) -> Result<T, PairingError> {
        self.phase = PairingPhase::Failed;
        self.server_secret.zeroize();
        self.server_challenge.zeroize();
        self.client_hash.zeroize();
        self.completed_proof_digest.zeroize();
        self.paired_client = None;
        Err(error)
    }
}

impl Drop for MoonlightPairingTransaction {
    fn drop(&mut self) {
        self.server_secret.zeroize();
        self.server_challenge.zeroize();
        self.client_hash.zeroize();
        self.completed_proof_digest.zeroize();
    }
}

fn valid_identity(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

fn validate_rsa_size(key: &RsaPublicKey) -> Result<(), PairingError> {
    let bits = key.n().bits();
    if (MIN_RSA_BITS..=MAX_RSA_BITS).contains(&bits) {
        Ok(())
    } else {
        Err(PairingError::InvalidCertificate)
    }
}

fn parse_certificate(der: &[u8]) -> Result<(RsaPublicKey, Vec<u8>), PairingError> {
    if der.is_empty() || der.len() > MAX_CERTIFICATE_BYTES {
        return Err(PairingError::InvalidCertificate);
    }
    let (remaining, certificate) =
        X509Certificate::from_der(der).map_err(|_| PairingError::InvalidCertificate)?;
    if !remaining.is_empty() {
        return Err(PairingError::InvalidCertificate);
    }
    let public_key = RsaPublicKey::from_pkcs1_der(certificate.public_key().subject_public_key.data.as_ref())
        .map_err(|_| PairingError::InvalidCertificate)?;
    let signature = certificate.signature_value.data.as_ref().to_vec();
    if signature.is_empty() || signature.len() > MAX_CERTIFICATE_BYTES {
        return Err(PairingError::InvalidCertificate);
    }
    Ok((public_key, signature))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::{pkcs1v15::SigningKey, pkcs8::DecodePrivateKey, signature::Signer};

    const SERVER_CERT: &[u8] = include_bytes!("../tests/fixtures/server-cert.der");
    const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/server-key.der");
    const CLIENT_CERT: &[u8] = include_bytes!("../tests/fixtures/client-cert.der");
    const CLIENT_KEY: &[u8] = include_bytes!("../tests/fixtures/client-key.der");

    fn make_transaction() -> MoonlightPairingTransaction {
        MoonlightPairingTransaction::new(
            &[0x33; 16],
            "0427",
            SERVER_CERT,
            SERVER_KEY,
            "client-uuid",
            "Moonlight test client",
            CLIENT_CERT,
        ).unwrap()
    }

    #[test]
    fn completes_the_four_authenticated_pairing_phases() {
        let salt = [0x33; 16];
        let pin_key = MoonlightPairingKey::from_pin(&salt, "0427").unwrap();
        let mut transaction = make_transaction();
        assert_eq!(transaction.phase(), PairingPhase::AwaitingServerCertificate);
        assert_eq!(transaction.get_server_certificate().unwrap(), SERVER_CERT);

        let client_challenge = [0x45; CHALLENGE_BYTES];
        let encrypted_challenge = encrypt_pairing_blocks(pin_key.expose_for_crypto(), &client_challenge).unwrap();
        let encrypted_response = transaction.client_challenge(&encrypted_challenge).unwrap();
        let response = decrypt_pairing_blocks(pin_key.expose_for_crypto(), &encrypted_response).unwrap();
        assert_eq!(response.len(), HASH_BYTES + CHALLENGE_BYTES);

        let (server_public_key, server_cert_signature) = parse_certificate(SERVER_CERT).unwrap();
        let server_secret: [u8; CHALLENGE_BYTES] = transaction.server_secret.unwrap();
        let mut server_transcript = Vec::new();
        server_transcript.extend_from_slice(&client_challenge);
        server_transcript.extend_from_slice(&server_cert_signature);
        server_transcript.extend_from_slice(&server_secret);
        assert_eq!(&response[..HASH_BYTES], Sha256::digest(server_transcript).as_slice());
        let server_challenge: [u8; CHALLENGE_BYTES] = response[HASH_BYTES..].try_into().unwrap();
        assert_eq!(transaction.server_challenge, Some(server_challenge));

        let client_secret = [0x7a; CHALLENGE_BYTES];
        let (_, client_cert_signature) = parse_certificate(CLIENT_CERT).unwrap();
        let mut client_transcript = Vec::new();
        client_transcript.extend_from_slice(&server_challenge);
        client_transcript.extend_from_slice(&client_cert_signature);
        client_transcript.extend_from_slice(&client_secret);
        let client_hash = Sha256::digest(client_transcript);
        let encrypted_hash = encrypt_pairing_blocks(pin_key.expose_for_crypto(), &client_hash).unwrap();
        let pairing_secret = transaction.server_challenge_response(&encrypted_hash).unwrap();
        let server_signature = RsaSignature::try_from(&pairing_secret[CHALLENGE_BYTES..]).unwrap();
        let server_verifier = VerifyingKey::<Sha256>::new(server_public_key);
        server_verifier.verify(&server_secret, &server_signature).unwrap();

        let client_private = RsaPrivateKey::from_pkcs8_der(CLIENT_KEY).unwrap();
        let client_signer = SigningKey::<Sha256>::new(client_private);
        let client_signature = client_signer.sign(&client_secret);
        let mut client_pairing_secret = client_secret.to_vec();
        client_pairing_secret.extend_from_slice(&client_signature.to_bytes());
        let paired = transaction.client_pairing_secret(&client_pairing_secret).unwrap();
        assert_eq!(paired.client_id, "client-uuid");
        assert_eq!(paired.client_name, "Moonlight test client");
        assert_eq!(paired.certificate_der, CLIENT_CERT);
        assert_eq!(transaction.phase(), PairingPhase::Paired);
        assert!(transaction.client_pairing_secret(&client_pairing_secret).unwrap() == paired);
        let mut conflicting_proof = client_pairing_secret.clone();
        conflicting_proof[0] ^= 1;
        assert!(matches!(
            transaction.client_pairing_secret(&conflicting_proof),
            Err(PairingError::UnexpectedStage)
        ));
        assert_eq!(transaction.phase(), PairingPhase::Paired);
    }

    #[test]
    fn pairing_http_adapter_runs_four_stages_and_releases_client_only_at_completion() {
        use crate::moonlight_pairing_http::{PairingEndpointRequest, PairingEndpointResponse};

        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|byte| format!("{byte:02X}")).collect()
        }
        fn field(xml: &str, name: &str) -> Vec<u8> {
            let start = xml.find(&format!("<{name}>")).unwrap() + name.len() + 2;
            let end = xml.find(&format!("</{name}>")).unwrap();
            let encoded = &xml[start..end];
            encoded.as_bytes().chunks_exact(2).map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
            }).collect()
        }
        let key = MoonlightPairingKey::from_pin(&[0x33; 16], "0427").unwrap();
        let mut transaction = make_transaction();

        let cert = PairingEndpointResponse::handle(&mut transaction,
            PairingEndpointRequest::parse_query("phrase=getservercert&salt=ignored").unwrap());
        assert_eq!(cert.status_code, 200);
        assert!(cert.xml.contains(&hex(SERVER_CERT)));
        assert!(cert.paired_client.is_none());

        let client_challenge = [0x45; CHALLENGE_BYTES];
        let encrypted = encrypt_pairing_blocks(key.expose_for_crypto(), &client_challenge).unwrap();
        let query = format!("phrase=clientchallenge&clientchallenge={}", hex(&encrypted));
        let challenge = PairingEndpointResponse::handle(&mut transaction, PairingEndpointRequest::parse_query(&query).unwrap());
        assert!(challenge.paired_client.is_none());
        let challenge_response = decrypt_pairing_blocks(key.expose_for_crypto(), &field(&challenge.xml, "challengeresponse")).unwrap();
        let server_challenge: [u8; CHALLENGE_BYTES] = challenge_response[HASH_BYTES..].try_into().unwrap();

        let client_secret = [0x7a; CHALLENGE_BYTES];
        let (_, client_cert_signature) = parse_certificate(CLIENT_CERT).unwrap();
        let mut transcript = Vec::new();
        transcript.extend_from_slice(&server_challenge);
        transcript.extend_from_slice(&client_cert_signature);
        transcript.extend_from_slice(&client_secret);
        let encrypted_hash = encrypt_pairing_blocks(key.expose_for_crypto(), &Sha256::digest(transcript)).unwrap();
        let query = format!("phrase=serverchallengeresp&serverchallengeresp={}", hex(&encrypted_hash));
        let server_response = PairingEndpointResponse::handle(&mut transaction, PairingEndpointRequest::parse_query(&query).unwrap());
        assert!(server_response.xml.contains("<pairingsecret>"));
        assert!(server_response.paired_client.is_none());

        let client_signer = SigningKey::<Sha256>::new(RsaPrivateKey::from_pkcs8_der(CLIENT_KEY).unwrap());
        let signature = client_signer.sign(&client_secret);
        let mut proof = client_secret.to_vec();
        proof.extend_from_slice(&signature.to_bytes());
        let query = format!("phrase=clientpairingsecret&clientpairingsecret={}", hex(&proof));
        let final_response = PairingEndpointResponse::handle(&mut transaction, PairingEndpointRequest::parse_query(&query).unwrap());
        assert_eq!(final_response.status_code, 200);
        let paired = final_response.paired_client.unwrap();
        assert_eq!(paired.client_id, "client-uuid");
        assert_eq!(paired.certificate_der, CLIENT_CERT);
    }

    #[test]
    fn rejects_out_of_order_and_wrong_client_proofs_without_authorizing() {
        let mut transaction = make_transaction();
        assert_eq!(
            transaction.client_challenge(&[0; CHALLENGE_BYTES]),
            Err(PairingError::UnexpectedStage)
        );
        assert_eq!(transaction.phase(), PairingPhase::Failed);

        let mut transaction = make_transaction();
        transaction.get_server_certificate().unwrap();
        assert_eq!(transaction.client_challenge(&[0; 15]), Err(PairingError::InvalidChallenge));
        assert_eq!(transaction.phase(), PairingPhase::Failed);
    }

    #[test]
    fn binds_the_operator_pin_to_the_pending_client_identity() {
        let transaction = make_transaction();
        assert_eq!(
            MoonlightPairingTransaction::new(
                &[0x33; 16], "0427", SERVER_CERT, SERVER_KEY, "client-1", "client 1", CLIENT_CERT
            ).unwrap().get_server_certificate().unwrap(),
            SERVER_CERT
        );
        assert!(matches!(
            MoonlightPairingTransaction::new(
                &[0x33; 16], "0427", SERVER_CERT, SERVER_KEY, "", "client 1", CLIENT_CERT
            ),
            Err(PairingError::InvalidIdentity)
        ));
        assert_eq!(transaction.phase(), PairingPhase::AwaitingServerCertificate);
    }
}
