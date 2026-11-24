//! Blocking HTTPS listener for one-request-per-connection GameStream control routes.
//! Run this on a dedicated control thread. The handler owns pairing, authentication,
//! discovery, and launch policy; this adapter only performs TLS, bounded HTTP parsing,
//! fixed response framing, and finite socket waits.
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    crypto::{self, WebPkiSupportedAlgorithms},
    pki_types::UnixTime,
    server::{danger::{ClientCertVerified, ClientCertVerifier}, NoServerSessionStorage, ServerConfig},
    CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme,
    ServerConnection, StreamOwned,
};

use crate::{
    http::{HttpError, HttpRequest, HttpRequestDecoder},
    moonlight_pairing::PairedClient,
    moonlight_pairing_store::MoonlightPairingStore,
};

const HTTP_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpStatus { Ok, BadRequest, Forbidden, NotFound, TooManyRequests, ServiceUnavailable, InternalServerError }
impl HttpStatus {
    fn code(self) -> u16 {
        match self {
            Self::Ok => 200, Self::BadRequest => 400, Self::Forbidden => 403, Self::NotFound => 404,
            Self::TooManyRequests => 429, Self::ServiceUnavailable => 503, Self::InternalServerError => 500,
        }
    }
    fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK", Self::BadRequest => "Bad Request", Self::Forbidden => "Forbidden",
            Self::NotFound => "Not Found", Self::TooManyRequests => "Too Many Requests",
            Self::ServiceUnavailable => "Service Unavailable", Self::InternalServerError => "Internal Server Error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: HttpStatus,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}
impl HttpResponse {
    pub fn xml(status: HttpStatus, body: impl Into<Vec<u8>>) -> Self {
        Self { status, content_type: "application/xml; charset=utf-8", body: body.into() }
    }
    pub fn encode(&self) -> Result<Vec<u8>, HttpResponseError> {
        if self.body.len() > MAX_RESPONSE_BYTES { return Err(HttpResponseError::ResponseTooLarge); }
        let header = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.status.code(), self.status.reason(), self.content_type, self.body.len(),
        );
        let mut bytes = Vec::with_capacity(header.len() + self.body.len());
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpResponseError { ResponseTooLarge }
impl std::fmt::Display for HttpResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HTTP response exceeds the 64 KiB limit")
    }
}
impl std::error::Error for HttpResponseError {}

#[derive(Debug)]
pub enum TlsHttpError {
    Io(io::Error),
    Tls(rustls::Error),
    Request(HttpError),
    ClientAuthorization(rustls::Error),
    TruncatedRequest,
    Response(HttpResponseError),
}
impl std::fmt::Display for TlsHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "HTTPS listener I/O failed: {error}"),
            Self::Tls(error) => write!(f, "HTTPS TLS handshake failed: {error}"),
            Self::ClientAuthorization(error) => write!(f, "HTTPS client authorization failed: {error}"),
            Self::Request(error) => write!(f, "invalid GameStream HTTP request: {error}"),
            Self::TruncatedRequest => f.write_str("HTTPS peer disconnected before completing its request"),
            Self::Response(error) => write!(f, "cannot encode HTTPS response: {error}"),
        }
    }
}
impl std::error::Error for TlsHttpError {}
impl From<io::Error> for TlsHttpError {
    fn from(error: io::Error) -> Self { Self::Io(error) }
}

/// Build a server TLS config from the host's persisted DER certificate and PKCS#8 key.
pub fn server_tls_config(
    certificate_der: &[u8],
    private_key_pkcs8_der: &[u8],
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let certificate = CertificateDer::from(certificate_der.to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_pkcs8_der.to_vec()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)?;
    Ok(Arc::new(config))
}

/// HTTPS boundary for routes that require pairing. The handler runs only after
/// TLS proves possession of the private key for a currently paired certificate.
pub struct PairedTlsHttpListener {
    listener: TlsHttpListener,
    clients: Arc<PairedClientVerifier>,
}

impl PairedTlsHttpListener {
    pub fn bind(
        address: impl ToSocketAddrs,
        certificate_der: &[u8],
        private_key_pkcs8_der: &[u8],
        store: MoonlightPairingStore,
    ) -> Result<Self, TlsHttpError> {
        let provider = Arc::new(crypto::ring::default_provider());
        let clients = Arc::new(PairedClientVerifier {
            store,
            algorithms: provider.signature_verification_algorithms,
        });
        let cert = CertificateDer::from(certificate_der.to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_pkcs8_der.to_vec()));
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsHttpError::Tls)?
            .with_client_cert_verifier(clients.clone())
            .with_single_cert(vec![cert], key)
            .map_err(TlsHttpError::Tls)?;
        // Require fresh proof and a current pairing record on every connection.
        config.session_storage = Arc::new(NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
        let listener = TlsHttpListener {
            listener: TcpListener::bind(address)?,
            config: Arc::new(config),
        };
        Ok(Self { listener, clients })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn serve_once(
        &self,
        handler: impl FnOnce(HttpRequest, PairedClient, SocketAddr) -> HttpResponse,
    ) -> Result<SocketAddr, TlsHttpError> {
        self.listener.serve(|request, connection, peer| {
            let leaf = connection
                .peer_certificates()
                .and_then(|chain| chain.first())
                .ok_or(rustls::Error::NoCertificatesPresented)
                .map_err(TlsHttpError::ClientAuthorization)?;
            let client = self.clients.lookup(leaf).map_err(TlsHttpError::ClientAuthorization)?;
            Ok(handler(request, client, peer))
        })
    }
}

struct PairedClientVerifier {
    store: MoonlightPairingStore,
    algorithms: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PairedClientVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PairedClientVerifier").finish_non_exhaustive()
    }
}

impl PairedClientVerifier {
    fn lookup(&self, certificate: &CertificateDer<'_>) -> Result<PairedClient, rustls::Error> {
        let clients = self.store.load_clients().map_err(|error| {
            rustls::Error::General(format!("cannot read paired client records: {error}"))
        })?;
        let mut matches = clients.into_iter().filter(|client| client.certificate_der() == certificate.as_ref());
        let client = matches.next().ok_or(CertificateError::UnknownIssuer)?;
        if matches.next().is_some() {
            return Err(rustls::Error::General("client certificate has multiple pairing records".into()));
        }
        Ok(client)
    }
}

impl ClientCertVerifier for PairedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // Paired leaves are pinned directly and need no CA issuer hint.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.lookup(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub struct TlsHttpListener {
    listener: TcpListener,
    config: Arc<ServerConfig>,
}
impl TlsHttpListener {
    pub fn bind(
        address: impl ToSocketAddrs,
        certificate_der: &[u8],
        private_key_pkcs8_der: &[u8],
    ) -> Result<Self, TlsHttpError> {
        let config = server_tls_config(certificate_der, private_key_pkcs8_der).map_err(TlsHttpError::Tls)?;
        Ok(Self { listener: TcpListener::bind(address)?, config })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> { self.listener.local_addr() }

    /// Accept and serve one TLS-protected GET request, then close the connection.
    /// Set explicit route authorization in the handler; TLS transport alone does not
    /// prove that a Moonlight client has been paired.
    pub fn serve_once(
        &self,
        handler: impl FnOnce(HttpRequest) -> HttpResponse,
    ) -> Result<SocketAddr, TlsHttpError> {
        self.serve(|request, _, _| Ok(handler(request)))
    }

    fn serve(
        &self,
        handler: impl FnOnce(HttpRequest, &ServerConnection, SocketAddr) -> Result<HttpResponse, TlsHttpError>,
    ) -> Result<SocketAddr, TlsHttpError> {
        let (socket, peer) = self.listener.accept()?;
        socket.set_read_timeout(Some(HTTP_SOCKET_TIMEOUT))?;
        socket.set_write_timeout(Some(HTTP_SOCKET_TIMEOUT))?;
        let connection = ServerConnection::new(self.config.clone()).map_err(TlsHttpError::Tls)?;
        let mut stream = StreamOwned::new(connection, socket);
        let mut decoder = HttpRequestDecoder::new();
        let mut input = [0_u8; 4096];
        let request = loop {
            let read = stream.read(&mut input).map_err(map_io)?;
            if read == 0 { return Err(TlsHttpError::TruncatedRequest); }
            if let Some(request) = decoder.push(&input[..read]).map_err(TlsHttpError::Request)? {
                break request;
            }
        };
        let response = handler(request, &stream.conn, peer)?;
        let encoded = response.encode().map_err(TlsHttpError::Response)?;
        stream.write_all(&encoded).map_err(map_io)?;
        stream.flush().map_err(map_io)?;
        stream.conn.send_close_notify();
        let _ = stream.conn.complete_io(&mut stream.sock);
        Ok(peer)
    }
}
fn map_io(error: io::Error) -> TlsHttpError {
    if error.kind() == io::ErrorKind::InvalidData {
        TlsHttpError::Io(error)
    } else {
        TlsHttpError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER_CERT: &[u8] = include_bytes!("../tests/fixtures/server-cert.der");
    const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/server-key.der");
    const CLIENT_KEY: &[u8] = include_bytes!("../tests/fixtures/client-key.der");

    #[derive(Debug)]
    struct TestServerCertVerifier;

    impl rustls::client::danger::ServerCertVerifier for TestServerCertVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            certificate: &CertificateDer<'_>,
            signature: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            let provider = rustls::crypto::ring::default_provider();
            rustls::crypto::verify_tls12_signature(
                message, certificate, signature, &provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            certificate: &CertificateDer<'_>,
            signature: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            let provider = rustls::crypto::ring::default_provider();
            rustls::crypto::verify_tls13_signature(
                message, certificate, signature, &provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    #[test]
    fn config_requires_a_valid_matching_certificate_and_pkcs8_key() {
        assert!(server_tls_config(SERVER_CERT, SERVER_KEY).is_ok());
        assert!(server_tls_config(SERVER_CERT, CLIENT_KEY).is_err());
        assert!(server_tls_config(SERVER_CERT, b"not a key").is_err());
    }

    #[test]
    fn frames_xml_http_response_with_exact_byte_length_and_close_policy() {
        let response = HttpResponse::xml(HttpStatus::Ok, "<root>ready</root>");
        let bytes = response.encode().unwrap();
        let split = bytes.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let headers = std::str::from_utf8(&bytes[..split]).unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Content-Length: 18\r\n"));
        assert!(headers.contains("Connection: close"));
        assert_eq!(&bytes[split + 4..], b"<root>ready</root>");
    }

    #[test]
    fn serves_a_real_tls_http_request_and_closes_the_response() {
        use std::{net::TcpStream, thread};
        use rustls::{pki_types::ServerName, ClientConfig, ClientConnection, RootCertStore};

        let mut client_config = ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        client_config
            .dangerous()
            .set_certificate_verifier(Arc::new(TestServerCertVerifier));
        let listener = TlsHttpListener::bind("127.0.0.1:0", SERVER_CERT, SERVER_KEY).unwrap();
        let address = listener.local_addr().unwrap();
        let serving = thread::spawn(move || {
            listener.serve_once(|request| {
                assert_eq!(request.path, "/serverinfo");
                HttpResponse::xml(HttpStatus::Ok, "<root><status>ready</status></root>")
            }).unwrap()
        });

        let socket = TcpStream::connect(address).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let server_name = ServerName::try_from("localhost").unwrap().to_owned();
        let connection = ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        let mut stream = StreamOwned::new(connection, socket);
        stream.write_all(b"GET /serverinfo HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        stream.flush().unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(b"<root><status>ready</status></root>"));
        let peer = serving.join().unwrap();
        assert!(peer.ip().is_loopback());
        assert_ne!(peer.port(), 0);
    }

    #[test]
    fn paired_tls_requires_a_current_certificate_and_reports_the_stored_identity() {
        use rustls::{pki_types::ServerName, ClientConfig, ClientConnection};
        use std::{fs, thread};

        const CLIENT_CERT: &[u8] = include_bytes!("../tests/fixtures/client-cert.der");
        let root = std::env::temp_dir().join(format!("aphelion-paired-tls-{:016x}", rand::random::<u64>()));
        fs::create_dir(&root).unwrap();
        struct TestDir(std::path::PathBuf);
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = TestDir(root);
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        store.authorize_client(&PairedClient {
            client_id: "stored-id".into(),
            client_name: "paired phone".into(),
            certificate_der: CLIENT_CERT.to_vec(),
        }).unwrap();
        let listener = Arc::new(PairedTlsHttpListener::bind(
            "127.0.0.1:0", SERVER_CERT, SERVER_KEY, MoonlightPairingStore::open(&dir.0).unwrap(),
        ).unwrap());

        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for (identity, allowed) in [
                (Some((CLIENT_CERT, CLIENT_KEY)), true),
                (Some((SERVER_CERT, SERVER_KEY)), false),
                (None, false),
            ] {
                exchange(&listener, version, identity, allowed);
            }
        }
        assert!(store.remove_client("stored-id").unwrap());
        exchange(&listener, &rustls::version::TLS13, Some((CLIENT_CERT, CLIENT_KEY)), false);

        fn exchange(
            listener: &Arc<PairedTlsHttpListener>,
            version: &'static rustls::SupportedProtocolVersion,
            identity: Option<(&[u8], &[u8])>,
            allowed: bool,
        ) {
            let builder = ClientConfig::builder_with_protocol_versions(&[version])
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(TestServerCertVerifier));
            let config = match identity {
                Some((cert, key)) => builder.with_client_auth_cert(
                    vec![CertificateDer::from(cert.to_vec())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
                ).unwrap(),
                None => builder.with_no_client_auth(),
            };

            let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let local_addr = socket.local_addr().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let listener = listener.clone();
            let serving = thread::spawn(move || listener.serve_once(|request, client, peer| {
                assert!(allowed, "unauthorized request reached the application");
                assert_eq!(client.client_id(), "stored-id");
                assert_eq!(client.client_name(), "paired phone");
                assert_eq!(peer, local_addr);
                assert_eq!(request.path, "/launch");
                HttpResponse::xml(HttpStatus::Ok, "<root/>")
            }));

            let name = ServerName::try_from("localhost").unwrap();
            let mut stream = StreamOwned::new(ClientConnection::new(Arc::new(config), name).unwrap(), socket);
            let mut response = Vec::new();
            let result = stream.write_all(b"GET /launch?uniqueid=claimed-id HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .and_then(|_| stream.read_to_end(&mut response));
            assert_eq!(serving.join().unwrap().is_ok(), allowed);
            assert_eq!(result.is_ok(), allowed);
            assert_eq!(response.starts_with(b"HTTP/1.1 200 OK\r\n"), allowed);
        }
    }

    #[test]
    fn refuses_oversized_response_bodies() {
        let response = HttpResponse::xml(HttpStatus::Ok, vec![b'x'; MAX_RESPONSE_BYTES + 1]);
        assert_eq!(response.encode(), Err(HttpResponseError::ResponseTooLarge));
    }
}
