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
    server::ServerConfig,
    ServerConnection, StreamOwned,
};

use crate::http::{HttpError, HttpRequest, HttpRequestDecoder};

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
    TruncatedRequest,
    Response(HttpResponseError),
}
impl std::fmt::Display for TlsHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "HTTPS listener I/O failed: {error}"),
            Self::Tls(error) => write!(f, "HTTPS TLS handshake failed: {error}"),
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
        let (mut socket, peer) = self.listener.accept()?;
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
        let response = handler(request);
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
        assert!(headers.contains("Connection: close\r\n"));
        assert_eq!(&bytes[split + 4..], b"<root>ready</root>");
    }

    #[test]
    fn refuses_oversized_response_bodies() {
        let response = HttpResponse::xml(HttpStatus::Ok, vec![b'x'; MAX_RESPONSE_BYTES + 1]);
        assert_eq!(response.encode(), Err(HttpResponseError::ResponseTooLarge));
    }
}
