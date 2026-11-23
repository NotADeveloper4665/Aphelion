//! Thread-safe bridge between the TLS listener and a local pairing approval UI.
//! Serve this listener on a dedicated control thread; another local thread can inspect
//! pending client labels and approve a request while its HTTPS response is held open.
use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use crate::{
    http::HttpRequest,
    moonlight_pairing_coordinator::{
        MoonlightPairingCoordinator, PairingAction, PairingCoordinatorError, PendingPairingSummary,
    },
    tls_http::{HttpResponse, HttpStatus, TlsHttpError, TlsHttpListener},
};

const APPROVAL_WAIT: Duration = Duration::from_secs(120);

struct SharedState {
    pairing: MoonlightPairingCoordinator,
    approved_responses: HashMap<String, HttpResponse>,
}
struct Shared {
    state: Mutex<SharedState>,
    changed: Condvar,
}

/// TLS control-plane HTTP server for bounded Moonlight PIN pairing.
pub struct MoonlightPairingHttpsService {
    listener: TlsHttpListener,
    shared: Arc<Shared>,
}
impl MoonlightPairingHttpsService {
    pub fn bind(
        address: impl ToSocketAddrs,
        pairing: MoonlightPairingCoordinator,
    ) -> Result<Self, TlsHttpError> {
        let listener = TlsHttpListener::bind(
            address, pairing.server_certificate_der(), pairing.server_private_key_pkcs8_der(),
        )?;
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                state: Mutex::new(SharedState { pairing, approved_responses: HashMap::new() }),
                changed: Condvar::new(),
            }),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> { self.listener.local_addr() }

    /// Serve one request. Call repeatedly from a dedicated blocking control thread.
    pub fn serve_once(&self) -> Result<SocketAddr, TlsHttpError> {
        self.listener.serve_once(|request| self.route(request))
    }

    pub fn pending_requests(&self) -> Result<Vec<PendingPairingSummary>, PairingCoordinatorError> {
        let mut state = self.shared.state.lock().map_err(|_| PairingCoordinatorError::TooManyRequests)?;
        Ok(state.pairing.pending_requests())
    }

    /// Approve a queued request. An invalid PIN leaves the request pending so the local
    /// operator UI can correct it without forcing the Moonlight client to start over.
    pub fn approve(&self, approval_id: &str, pin: &str) -> Result<(), PairingCoordinatorError> {
        let mut state = self.shared.state.lock().map_err(|_| PairingCoordinatorError::TooManyRequests)?;
        let response = state.pairing.approve(approval_id, pin)?;
        let http = HttpResponse::xml(http_status(response.status_code), response.xml);
        state.approved_responses.insert(approval_id.to_owned(), http);
        self.shared.changed.notify_all();
        Ok(())
    }

    fn route(&self, request: HttpRequest) -> HttpResponse {
        if request.path != "/pair" {
            return xml_response(HttpStatus::NotFound, 404, "unknown endpoint");
        }
        let Some(query) = request.query.as_deref() else {
            return xml_response(HttpStatus::BadRequest, 400, "missing pairing query");
        };
        let action = {
            let Ok(mut state) = self.shared.state.lock() else {
                return xml_response(HttpStatus::InternalServerError, 500, "pairing state unavailable");
            };
            match state.pairing.handle_pair_query(query) {
                Ok(PairingAction::Response(response)) => {
                    return HttpResponse::xml(http_status(response.status_code), response.xml);
                }
                Ok(PairingAction::ApprovalRequired(pending)) => pending.approval_id,
                Err(error) => {
                    let (status, code) = match error {
                        PairingCoordinatorError::Store(_) => (HttpStatus::InternalServerError, 500),
                        PairingCoordinatorError::TooManyRequests => (HttpStatus::TooManyRequests, 429),
                        _ => (HttpStatus::BadRequest, 400),
                    };
                    return xml_response(status, code, "pairing request rejected");
                }
            }
        };

        let deadline = Instant::now() + APPROVAL_WAIT;
        let Ok(mut state) = self.shared.state.lock() else {
            return xml_response(HttpStatus::InternalServerError, 500, "pairing state unavailable");
        };
        loop {
            if let Some(response) = state.approved_responses.remove(&action) { return response; }
            let now = Instant::now();
            if now >= deadline { return xml_response(HttpStatus::ServiceUnavailable, 503, "pairing approval expired"); }
            let remaining = deadline.saturating_duration_since(now);
            let Ok((next, wait)) = self.shared.changed.wait_timeout(state, remaining) else {
                return xml_response(HttpStatus::InternalServerError, 500, "pairing approval wait failed");
            };
            state = next;
            if wait.timed_out() && !state.approved_responses.contains_key(&action) {
                return xml_response(HttpStatus::ServiceUnavailable, 503, "pairing approval expired");
            }
        }
    }
}
fn http_status(status: u16) -> HttpStatus {
    match status {
        200 => HttpStatus::Ok,
        403 => HttpStatus::Forbidden,
        429 => HttpStatus::TooManyRequests,
        503 => HttpStatus::ServiceUnavailable,
        500 => HttpStatus::InternalServerError,
        _ => HttpStatus::BadRequest,
    }
}
fn xml_response(status: HttpStatus, code: u16, message: &str) -> HttpResponse {
    let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;");
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"{code}\" status_message=\"{escaped}\"><paired>0</paired></root>"
    );
    HttpResponse::xml(status, xml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::Arc, thread, time::{Duration, SystemTime, UNIX_EPOCH}};

    const SERVER_CERT: &[u8] = include_bytes!("../tests/fixtures/server-cert.der");
    const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/server-key.der");
    const CLIENT_CERT: &[u8] = include_bytes!("../tests/fixtures/client-cert.der");

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            Self(std::env::temp_dir().join(format!("aphelion-pair-https-{}-{suffix}", std::process::id())))
        }
    }
    impl Drop for TestDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[test]
    fn local_approval_unblocks_the_held_pairing_http_response() {
        let dir = TestDir::new();
        let store = crate::moonlight_pairing_store::MoonlightPairingStore::open(&dir.0).unwrap();
        store.save_server_identity(SERVER_CERT, SERVER_KEY).unwrap();
        let coordinator = MoonlightPairingCoordinator::open(store).unwrap();
        let service = Arc::new(MoonlightPairingHttpsService::bind("127.0.0.1:0", coordinator).unwrap());
        assert_ne!(service.local_addr().unwrap().port(), 0);

        fn hex(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02X}")).collect() }
        let query = format!("GET /pair?phrase=getservercert&uniqueid=phone&devicename=Pixel+9a&salt={}&clientcert={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            hex(&[0x44; 16]), hex(CLIENT_CERT));
        let mut decoder = crate::http::HttpRequestDecoder::new();
        let request = decoder.push(query.as_bytes()).unwrap().unwrap();
        let serving = service.clone();
        let request_task = thread::spawn(move || serving.route(request));

        let pending = (0..100).find_map(|_| {
            let requests = service.pending_requests().unwrap();
            if requests.is_empty() { thread::sleep(Duration::from_millis(5)); None } else { requests.into_iter().next() }
        }).expect("local UI should observe the pending client");
        assert_eq!(pending.client_id, "phone");
        assert_eq!(pending.client_name, "Pixel 9a");
        service.approve(&pending.approval_id, "0427").unwrap();
        let response = request_task.join().unwrap();
        assert_eq!(response.status, HttpStatus::Ok);
        let xml = String::from_utf8(response.body).unwrap();
        assert!(xml.contains(&hex(SERVER_CERT)));
        assert!(service.pending_requests().unwrap().is_empty());
    }

    #[test]
    fn escapes_pairing_error_messages_before_xml_serialization() {
        let response = xml_response(HttpStatus::BadRequest, 400, "<bad&\"input>");
        assert!(response.body.windows(5).any(|window| window == b"&lt;b"));
        assert!(!String::from_utf8(response.body).unwrap().contains("<bad&"));
    }
}
