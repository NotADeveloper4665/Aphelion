//! In-memory coordinator for Moonlight PIN approval and the four proof stages.
//! It bridges bounded /pair queries to a short-lived operator approval request, keeps
//! transactions per client identity, and persists authorization only after verified proof.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    http::{parse_query, QueryError},
    moonlight_pairing::{MoonlightPairingTransaction, PairedClient},
    moonlight_pairing_approval::{PendingMoonlightPairing, PendingPairingError},
    moonlight_pairing_http::{PairingEndpointError, PairingEndpointRequest, PairingEndpointResponse},
    moonlight_pairing_store::{MoonlightPairingStore, PairingStoreError},
};

const MAX_PENDING_REQUESTS: usize = 256;
const MAX_ACTIVE_TRANSACTIONS: usize = 256;
const REQUEST_LIFETIME: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPairingSummary {
    /// Random short-lived token shown only to the local operator interface.
    pub approval_id: String,
    pub client_id: String,
    pub client_name: String,
}
pub enum PairingAction {
    ApprovalRequired(PendingPairingSummary),
    Response(PairingEndpointResponse),
}

struct PendingEntry {
    summary: PendingPairingSummary,
    request: PendingMoonlightPairing,
    fingerprint: [u8; 32],
    created_at: Instant,
}
struct ActiveEntry {
    transaction: MoonlightPairingTransaction,
    expires_at: Instant,
}

#[derive(Debug)]
pub enum PairingCoordinatorError {
    Store(PairingStoreError),
    Pending(PendingPairingError),
    Endpoint(PairingEndpointError),
    Query(QueryError),
    MissingParameter(&'static str),
    AlreadyInProgress,
    TooManyRequests,
    UnknownApproval,
    InvalidPin,
    MissingServerIdentity,
}
impl std::fmt::Display for PairingCoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "Moonlight pairing store failed: {error}"),
            Self::Pending(error) => write!(f, "invalid pending Moonlight pairing: {error}"),
            Self::Endpoint(error) => write!(f, "invalid Moonlight pairing request: {error:?}"),
            Self::Query(error) => write!(f, "invalid Moonlight pairing query: {error}"),
            Self::MissingParameter(name) => write!(f, "Moonlight pairing query is missing {name}"),
            Self::AlreadyInProgress => f.write_str("this Moonlight client already has a pairing exchange in progress"),
            Self::TooManyRequests => f.write_str("too many Moonlight pairing exchanges are pending"),
            Self::UnknownApproval => f.write_str("Moonlight pairing approval is unknown or expired"),
            Self::InvalidPin => f.write_str("Moonlight pairing PIN must contain exactly four ASCII digits"),
            Self::MissingServerIdentity => f.write_str("Moonlight server certificate and private key are not initialized"),
        }
    }
}
impl std::error::Error for PairingCoordinatorError {}
impl From<PairingStoreError> for PairingCoordinatorError {
    fn from(error: PairingStoreError) -> Self { Self::Store(error) }
}
impl From<PendingPairingError> for PairingCoordinatorError {
    fn from(error: PendingPairingError) -> Self { Self::Pending(error) }
}
impl From<PairingEndpointError> for PairingCoordinatorError {
    fn from(error: PairingEndpointError) -> Self { Self::Endpoint(error) }
}
impl From<QueryError> for PairingCoordinatorError {
    fn from(error: QueryError) -> Self { Self::Query(error) }
}

/// Owns all mutable Moonlight pairing state for one host process.
pub struct MoonlightPairingCoordinator {
    store: MoonlightPairingStore,
    server_certificate_der: Vec<u8>,
    server_private_key_pkcs8_der: Zeroizing<Vec<u8>>,
    pending: HashMap<String, PendingEntry>,
    active: HashMap<String, ActiveEntry>,
}
impl MoonlightPairingCoordinator {
    pub fn open(store: MoonlightPairingStore) -> Result<Self, PairingCoordinatorError> {
        let identity = store.load_server_identity()?.ok_or(PairingCoordinatorError::MissingServerIdentity)?;
        let server_private_key_pkcs8_der = Zeroizing::new(identity.private_key_pkcs8_der().to_vec());
        let server_certificate_der = identity.certificate_der;
        Ok(Self {
            store,
            server_certificate_der,
            server_private_key_pkcs8_der,
            pending: HashMap::new(),
            active: HashMap::new(),
        })
    }

    pub fn server_certificate_der(&self) -> &[u8] { &self.server_certificate_der }

    /// Begin the normal GameStream PIN flow or advance an already-approved exchange.
    pub fn handle_pair_query(&mut self, query: &str) -> Result<PairingAction, PairingCoordinatorError> {
        self.expire_old_requests();
        if query.len() > 64 * 1024 { return Err(PairingCoordinatorError::Endpoint(PairingEndpointError::QueryTooLarge)); }
        let parameters = parse_query(query.trim_start_matches('?'))?;
        let phrase = field(&parameters, "phrase").ok_or(PairingCoordinatorError::MissingParameter("phrase"))?;
        if phrase == "pairchallenge" {
            return Ok(PairingAction::Response(PairingEndpointResponse::pair_challenge()));
        }
        if phrase == "getservercert" {
            return self.begin_pairing(&parameters);
        }

        let client_id = field(&parameters, "uniqueid").ok_or(PairingCoordinatorError::MissingParameter("uniqueid"))?;
        let Some(active) = self.active.get_mut(client_id) else {
            return Ok(PairingAction::Response(PairingEndpointResponse {
                status_code: 400,
                xml: "<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"400\"><paired>0</paired></root>".into(),
                paired_client: None,
            }));
        };
        let endpoint = PairingEndpointRequest::parse_query(query)?;
        // The transaction is keyed by the uniqueid from this query, so malformed routing
        // parameters can never select a different client's PIN/session state.
        if field(&parameters, "uniqueid") != Some(client_id) {
            return Err(PairingCoordinatorError::MissingParameter("uniqueid"));
        }
        let response = PairingEndpointResponse::handle(&mut active.transaction, endpoint);
        if let Some(client) = response.paired_client.as_ref() {
            self.store.authorize_client(client)?;
        }
        Ok(PairingAction::Response(response))
    }

    fn begin_pairing(&mut self, parameters: &[(String, String)]) -> Result<PairingAction, PairingCoordinatorError> {
        let client_id = field(parameters, "uniqueid").filter(|value| !value.is_empty())
            .ok_or(PairingCoordinatorError::MissingParameter("uniqueid"))?.to_owned();
        let mut client_name = field(parameters, "devicename").unwrap_or("Moonlight client").to_owned();
        if client_name == "roth" { client_name = "Legacy Moonlight Client".to_owned(); }
        let salt = field(parameters, "salt").ok_or(PairingCoordinatorError::MissingParameter("salt"))?;
        let certificate = field(parameters, "clientcert").ok_or(PairingCoordinatorError::MissingParameter("clientcert"))?;
        let fingerprint = fingerprint(&client_id, &client_name, salt, certificate);

        if self.active.contains_key(&client_id) { return Err(PairingCoordinatorError::AlreadyInProgress); }
        if let Some((_, existing)) = self.pending.iter().find(|(_, entry)| entry.summary.client_id == client_id) {
            if existing.fingerprint == fingerprint {
                return Ok(PairingAction::ApprovalRequired(existing.summary.clone()));
            }
            return Err(PairingCoordinatorError::AlreadyInProgress);
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS || self.active.len() >= MAX_ACTIVE_TRANSACTIONS {
            return Err(PairingCoordinatorError::TooManyRequests);
        }

        let request = PendingMoonlightPairing::new(&client_id, &client_name, salt, certificate)?;
        let approval_id = random_token();
        let summary = PendingPairingSummary { approval_id: approval_id.clone(), client_id, client_name };
        self.pending.insert(approval_id.clone(), PendingEntry {
            summary: summary.clone(), request, fingerprint, created_at: Instant::now(),
        });
        Ok(PairingAction::ApprovalRequired(summary))
    }

    /// Approve one exact pending client. The PIN is consumed into its transaction.
    pub fn approve(&mut self, approval_id: &str, pin: &str) -> Result<PairingEndpointResponse, PairingCoordinatorError> {
        self.expire_old_requests();
        if pin.len() != 4 || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(PairingCoordinatorError::InvalidPin);
        }
        let entry = self.pending.remove(approval_id).ok_or(PairingCoordinatorError::UnknownApproval)?;
        let client_id = entry.summary.client_id;
        if self.active.contains_key(&client_id) { return Err(PairingCoordinatorError::AlreadyInProgress); }
        let mut transaction = entry.request.approve(
            pin, &self.server_certificate_der, &self.server_private_key_pkcs8_der,
        )?;
        let request = PairingEndpointRequest::parse_query("phrase=getservercert")?;
        let response = PairingEndpointResponse::handle(&mut transaction, request);
        self.active.insert(client_id, ActiveEntry {
            transaction, expires_at: Instant::now() + REQUEST_LIFETIME,
        });
        Ok(response)
    }

    /// Only nonsecret identity labels are available to the local approval UI.
    pub fn pending_requests(&mut self) -> Vec<PendingPairingSummary> {
        self.expire_old_requests();
        let mut entries: Vec<_> = self.pending.values().map(|entry| entry.summary.clone()).collect();
        entries.sort_by(|left, right| left.approval_id.cmp(&right.approval_id));
        entries
    }

    /// Check a client identity using the persisted exact certificate bytes.
    pub fn is_client_authorized(&self, client_id: &str, certificate_der: &[u8]) -> Result<bool, PairingCoordinatorError> {
        let clients = self.store.load_clients()?;
        Ok(clients.iter().any(|client| {
            client.client_id() == client_id && client.certificate_der().len() == certificate_der.len()
                && client.certificate_der().ct_eq(certificate_der).unwrap_u8() == 1
        }))
    }

    fn expire_old_requests(&mut self) {
        let now = Instant::now();
        self.pending.retain(|_, entry| now.saturating_duration_since(entry.created_at) <= REQUEST_LIFETIME);
        self.active.retain(|_, entry| entry.expires_at > now);
    }
}
fn field<'a>(parameters: &'a [(String, String)], name: &str) -> Option<&'a str> {
    parameters.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}
fn fingerprint(client_id: &str, name: &str, salt: &str, certificate: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    for value in [client_id, name, salt, certificate] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    digest.finalize().into()
}
fn random_token() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    const SERVER_CERT: &[u8] = include_bytes!("../tests/fixtures/server-cert.der");
    const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/server-key.der");
    const CLIENT_CERT: &[u8] = include_bytes!("../tests/fixtures/client-cert.der");

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            Self(std::env::temp_dir().join(format!("aphelion-pair-coordinator-{}-{suffix}", std::process::id())))
        }
    }
    impl Drop for TestDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn hex(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02X}")).collect() }

    #[test]
    fn holds_pending_pairing_until_identity_bound_operator_approval() {
        let dir = TestDir::new();
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        store.save_server_identity(SERVER_CERT, SERVER_KEY).unwrap();
        let mut coordinator = MoonlightPairingCoordinator::open(store).unwrap();
        let query = format!("phrase=getservercert&uniqueid=phone-uuid&devicename=Pixel+9a&salt={}&clientcert={}",
            hex(&[0x44; 16]), hex(CLIENT_CERT));
        let PairingAction::ApprovalRequired(pending) = coordinator.handle_pair_query(&query).unwrap() else { panic!("approval should be requested"); };
        assert_eq!(pending.client_id, "phone-uuid");
        assert_eq!(pending.client_name, "Pixel 9a");
        assert_eq!(coordinator.pending_requests(), vec![pending.clone()]);
        assert!(!coordinator.is_client_authorized("phone-uuid", CLIENT_CERT).unwrap());

        let cert_response = coordinator.approve(&pending.approval_id, "0427").unwrap();
        assert_eq!(cert_response.status_code, 200);
        assert!(cert_response.xml.contains(&hex(SERVER_CERT)));
        assert!(coordinator.pending_requests().is_empty());
        assert!(!coordinator.is_client_authorized("phone-uuid", CLIENT_CERT).unwrap());
    }

    #[test]
    fn rejects_invalid_pin_and_duplicate_client_request_mutation() {
        let dir = TestDir::new();
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        store.save_server_identity(SERVER_CERT, SERVER_KEY).unwrap();
        let mut coordinator = MoonlightPairingCoordinator::open(store).unwrap();
        let query = format!("phrase=getservercert&uniqueid=phone-uuid&devicename=Phone&salt={}&clientcert={}",
            hex(&[0x44; 16]), hex(CLIENT_CERT));
        let PairingAction::ApprovalRequired(pending) = coordinator.handle_pair_query(&query).unwrap() else { panic!("approval should be requested"); };
        let retry = coordinator.handle_pair_query(&query).unwrap();
        let PairingAction::ApprovalRequired(retry) = retry else { panic!("same request should reuse pending approval"); };
        assert_eq!(retry.approval_id, pending.approval_id);
        assert!(matches!(coordinator.approve(&pending.approval_id, "abcd"), Err(PairingCoordinatorError::InvalidPin)));
        let mutated = query.replace("devicename=Phone", "devicename=Different");
        assert!(matches!(coordinator.handle_pair_query(&mutated), Err(PairingCoordinatorError::AlreadyInProgress)));
    }

    #[test]
    fn pairing_challenge_preflight_does_not_allocate_approval_state() {
        let dir = TestDir::new();
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        store.save_server_identity(SERVER_CERT, SERVER_KEY).unwrap();
        let mut coordinator = MoonlightPairingCoordinator::open(store).unwrap();
        let PairingAction::Response(response) = coordinator.handle_pair_query("phrase=pairchallenge").unwrap() else { panic!("probe should get immediate response"); };
        assert!(response.xml.contains("<paired>1</paired>"));
        assert!(coordinator.pending_requests().is_empty());
    }
}
