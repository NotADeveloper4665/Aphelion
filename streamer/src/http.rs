//! Bounded HTTP/1.x request decoder for Moonlight GameStream control requests.
//! It accepts one bodyless request per connection. TLS, authentication, query decoding,
//! and response policy belong to the embedding server.
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_HEADER_COUNT: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub version: HttpVersion,
    headers: Vec<(String, String)>,
}
impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let mut matches = self.headers.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name));
        let value = matches.next()?.1.as_str();
        if matches.next().is_some() { None } else { Some(value) }
    }
    pub fn headers(&self) -> &[(String, String)] { &self.headers }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion { V10, V11 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
    RequestTooLarge, InvalidEncoding, MalformedRequestLine, UnsupportedMethod,
    UnsupportedVersion, InvalidTarget, TooManyHeaders, InvalidHeader, DuplicateHeader,
    RequestBodyUnsupported, TrailingData,
}
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RequestTooLarge => "HTTP request exceeds the 16 KiB limit",
            Self::InvalidEncoding => "HTTP request must contain printable ASCII only",
            Self::MalformedRequestLine => "malformed HTTP request line",
            Self::UnsupportedMethod => "GameStream control requests must use GET",
            Self::UnsupportedVersion => "only HTTP/1.0 and HTTP/1.1 are supported",
            Self::InvalidTarget => "invalid HTTP request target",
            Self::TooManyHeaders => "HTTP request exceeds the header-count limit",
            Self::InvalidHeader => "malformed or oversized HTTP header",
            Self::DuplicateHeader => "HTTP request contains an ambiguous duplicate header",
            Self::RequestBodyUnsupported => "GameStream control requests must not contain a body",
            Self::TrailingData => "HTTP decoder accepts exactly one request per connection",
        })
    }
}
impl std::error::Error for HttpError {}

#[derive(Debug, Default)]
pub struct HttpRequestDecoder { buffer: Vec<u8> }
impl HttpRequestDecoder {
    pub fn new() -> Self { Self::default() }
    pub fn buffered_bytes(&self) -> usize { self.buffer.len() }
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<HttpRequest>, HttpError> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_REQUEST_BYTES {
            return Err(HttpError::RequestTooLarge);
        }
        self.buffer.extend_from_slice(bytes);
        if self.buffer.iter().any(|byte| !(0x20..=0x7e).contains(byte) && *byte != b'\r' && *byte != b'\n' && *byte != b'\t') {
            return Err(HttpError::InvalidEncoding);
        }
        let Some(end) = self.buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            return Ok(None);
        };
        if end + 4 != self.buffer.len() { return Err(HttpError::TrailingData); }
        parse_headers(&self.buffer[..end])
    }
}

fn parse_headers(bytes: &[u8]) -> Result<Option<HttpRequest>, HttpError> {
    let text = std::str::from_utf8(bytes).map_err(|_| HttpError::InvalidEncoding)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(HttpError::MalformedRequestLine)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(HttpError::MalformedRequestLine)?;
    let target = parts.next().ok_or(HttpError::MalformedRequestLine)?;
    let version = parts.next().ok_or(HttpError::MalformedRequestLine)?;
    if parts.next().is_some() || method.is_empty() || target.is_empty() { return Err(HttpError::MalformedRequestLine); }
    if method != "GET" { return Err(HttpError::UnsupportedMethod); }
    let version = match version {
        "HTTP/1.0" => HttpVersion::V10,
        "HTTP/1.1" => HttpVersion::V11,
        _ => return Err(HttpError::UnsupportedVersion),
    };
    if !target.starts_with('/') || target.contains('#') || target.bytes().any(|byte| byte.is_ascii_control() || byte == b' ') {
        return Err(HttpError::InvalidTarget);
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query.to_owned())),
        None => (target, None),
    };
    if path.is_empty() { return Err(HttpError::InvalidTarget); }

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() { continue; }
        if headers.len() == MAX_HEADER_COUNT { return Err(HttpError::TooManyHeaders); }
        if line.starts_with(' ') || line.starts_with('\t') { return Err(HttpError::InvalidHeader); }
        let (name, value) = line.split_once(':').ok_or(HttpError::InvalidHeader)?;
        if name.is_empty() || name.len() > MAX_HEADER_NAME_BYTES || !name.bytes().all(is_token) {
            return Err(HttpError::InvalidHeader);
        }
        let value = value.trim_matches([' ', '\t']);
        if value.len() > MAX_HEADER_VALUE_BYTES || value.bytes().any(|byte| byte < 0x20 && byte != b'\t') {
            return Err(HttpError::InvalidHeader);
        }
        let name = name.to_ascii_lowercase();
        if headers.iter().any(|(existing, _)| existing == &name) { return Err(HttpError::DuplicateHeader); }
        headers.push((name, value.to_owned()));
    }
    if headers.iter().any(|(name, value)| {
        (name == "transfer-encoding" && !value.eq_ignore_ascii_case("identity"))
            || (name == "content-length" && value != "0")
    }) {
        return Err(HttpError::RequestBodyUnsupported);
    }
    Ok(Some(HttpRequest { method: method.to_owned(), path: path.to_owned(), query, version, headers }))
}
fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'|' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_fragmented_gamestream_get_and_normalizes_headers() {
        let mut decoder = HttpRequestDecoder::new();
        assert_eq!(decoder.push(b"GET /serverinfo?uniqueid=phone HTTP/1.1\r\nHo"), Ok(None));
        let request = decoder.push(b"st: host\r\nX-Client: moonlight\r\n\r\n").unwrap().unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/serverinfo");
        assert_eq!(request.query.as_deref(), Some("uniqueid=phone"));
        assert_eq!(request.header("host"), Some("host"));
        assert_eq!(request.header("X-CLIENT"), Some("moonlight"));
        assert_eq!(request.version, HttpVersion::V11);
    }
    #[test]
    fn rejects_ambiguous_malformed_oversized_and_body_requests() {
        let mut duplicate = HttpRequestDecoder::new();
        assert_eq!(duplicate.push(b"GET /pair HTTP/1.1\r\nHost: a\r\nhost: b\r\n\r\n"), Err(HttpError::DuplicateHeader));
        let mut malformed = HttpRequestDecoder::new();
        assert_eq!(malformed.push(b"POST /launch HTTP/1.1\r\n\r\n"), Err(HttpError::UnsupportedMethod));
        let mut body = HttpRequestDecoder::new();
        assert_eq!(body.push(b"GET /launch HTTP/1.1\r\nContent-Length: 1\r\n\r\n"), Err(HttpError::RequestBodyUnsupported));
        let mut large = HttpRequestDecoder::new();
        assert_eq!(large.push(&vec![b'a'; MAX_REQUEST_BYTES + 1]), Err(HttpError::RequestTooLarge));
        let mut target = HttpRequestDecoder::new();
        assert_eq!(target.push(b"GET https://host/ HTTP/1.1\r\n\r\n"), Err(HttpError::InvalidTarget));
    }
    #[test]
    fn incomplete_or_disconnected_request_stays_bounded() {
        let mut decoder = HttpRequestDecoder::new();
        assert_eq!(decoder.push(b"GET /pair HTTP/1.1\r\nHost: local\r\n"), Ok(None));
        assert!(decoder.buffered_bytes() < MAX_REQUEST_BYTES);
    }
    #[test]
    fn rejects_duplicate_framing_headers_and_header_injection() {
        let mut duplicate_length = HttpRequestDecoder::new();
        assert_eq!(duplicate_length.push(b"GET / HTTP/1.1\r\nContent-Length: 0\r\ncontent-length: 0\r\n\r\n"), Err(HttpError::DuplicateHeader));
        let mut control = HttpRequestDecoder::new();
        assert_eq!(control.push(b"GET / HTTP/1.1\r\nX: a\x01b\r\n\r\n"), Err(HttpError::InvalidEncoding));
    }
}
