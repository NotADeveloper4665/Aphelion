//! Bounded RTSP/1.0 request framing for the future Moonlight session layer.
//!
//! This module is only a framing/parser primitive. It does not authenticate a
//! client, pair certificates, negotiate Moonlight capabilities, or implement
//! a streaming session state machine.

pub const MAX_RTSP_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_INPUT_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub cseq: u32,
}

impl RtspRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspError {
    InputChunkTooLarge,
    MessageTooLarge,
    InvalidHeaderEncoding,
    InvalidRequestLine,
    UnsupportedVersion,
    InvalidHeader,
    DuplicateCriticalHeader(&'static str),
    InvalidContentLength,
    UnsupportedTransferEncoding,
    MissingCseq,
    InvalidCseq,
}

impl std::fmt::Display for RtspError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputChunkTooLarge => formatter.write_str("RTSP input chunk exceeds parser limit"),
            Self::MessageTooLarge => formatter.write_str("RTSP message exceeds parser limit"),
            Self::InvalidHeaderEncoding => formatter.write_str("RTSP request headers are not valid UTF-8/ASCII"),
            Self::InvalidRequestLine => formatter.write_str("malformed RTSP request line"),
            Self::UnsupportedVersion => formatter.write_str("only RTSP/1.0 requests are supported"),
            Self::InvalidHeader => formatter.write_str("malformed RTSP header"),
            Self::DuplicateCriticalHeader(name) => write!(formatter, "RTSP request has duplicate {name} header"),
            Self::InvalidContentLength => formatter.write_str("invalid RTSP Content-Length"),
            Self::UnsupportedTransferEncoding => formatter.write_str("RTSP Transfer-Encoding is unsupported"),
            Self::MissingCseq => formatter.write_str("RTSP request is missing CSeq"),
            Self::InvalidCseq => formatter.write_str("RTSP CSeq must be an unsigned 32-bit integer"),
        }
    }
}

impl std::error::Error for RtspError {}

/// Incremental decoder. It retains only an incomplete bounded message and
/// returns all complete requests, including pipelined requests, in wire order.
#[derive(Debug, Default)]
pub struct RtspDecoder {
    buffer: Vec<u8>,
}

impl RtspDecoder {
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<RtspRequest>, RtspError> {
        if bytes.len() > MAX_INPUT_CHUNK_BYTES {
            return Err(RtspError::InputChunkTooLarge);
        }
        self.buffer.extend_from_slice(bytes);
        let mut requests = Vec::new();

        loop {
            let Some(headers_end) = find_header_end(&self.buffer) else {
                if self.buffer.len() > MAX_RTSP_MESSAGE_BYTES {
                    return Err(RtspError::MessageTooLarge);
                }
                break;
            };
            let header_bytes = headers_end + 4;
            if header_bytes > MAX_RTSP_MESSAGE_BYTES {
                return Err(RtspError::MessageTooLarge);
            }
            let header_text = std::str::from_utf8(&self.buffer[..headers_end])
                .map_err(|_| RtspError::InvalidHeaderEncoding)?;
            if !header_text.is_ascii() {
                return Err(RtspError::InvalidHeaderEncoding);
            }
            let headers = parse_headers(header_text)?;
            let content_length = unique_header(&headers, "content-length")?
                .map(|value| value.parse::<usize>().map_err(|_| RtspError::InvalidContentLength))
                .transpose()?
                .unwrap_or(0);
            if unique_header(&headers, "transfer-encoding")?.is_some() {
                return Err(RtspError::UnsupportedTransferEncoding);
            }
            let total_length = header_bytes.checked_add(content_length)
                .filter(|length| *length <= MAX_RTSP_MESSAGE_BYTES)
                .ok_or(RtspError::MessageTooLarge)?;
            if self.buffer.len() < total_length {
                if self.buffer.len() > MAX_RTSP_MESSAGE_BYTES {
                    return Err(RtspError::MessageTooLarge);
                }
                break;
            }

            let request = parse_request(header_text, headers, self.buffer[header_bytes..total_length].to_vec())?;
            self.buffer.drain(..total_length);
            requests.push(request);
        }
        Ok(requests)
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_headers(header_text: &str) -> Result<Vec<(String, String)>, RtspError> {
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or(RtspError::InvalidRequestLine)?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(RtspError::InvalidRequestLine)?;
    let target = parts.next().ok_or(RtspError::InvalidRequestLine)?;
    let version = parts.next().ok_or(RtspError::InvalidRequestLine)?;
    if parts.next().is_some() || method.is_empty() || target.is_empty() {
        return Err(RtspError::InvalidRequestLine);
    }
    if version != "RTSP/1.0" {
        return Err(RtspError::UnsupportedVersion);
    }
    if !method.bytes().all(is_token_byte) || target.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RtspError::InvalidRequestLine);
    }

    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(RtspError::InvalidHeader)?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || !name.bytes().all(is_token_byte) || value.bytes().any(|byte| byte.is_ascii_control() && byte != b'\t') {
            return Err(RtspError::InvalidHeader);
        }
        headers.push((name.to_ascii_lowercase(), value.to_owned()));
    }
    Ok(headers)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn unique_header<'a>(headers: &'a [(String, String)], name: &'static str) -> Result<Option<&'a str>, RtspError> {
    let mut values = headers.iter().filter(|(header_name, _)| header_name == name);
    let first = values.next().map(|(_, value)| value.as_str());
    if values.next().is_some() {
        return Err(RtspError::DuplicateCriticalHeader(name));
    }
    Ok(first)
}

fn parse_request(
    header_text: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<RtspRequest, RtspError> {
    let request_line = header_text.split("\r\n").next().ok_or(RtspError::InvalidRequestLine)?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(RtspError::InvalidRequestLine)?.to_owned();
    let target = parts.next().ok_or(RtspError::InvalidRequestLine)?.to_owned();
    let cseq = unique_header(&headers, "cseq")?
        .ok_or(RtspError::MissingCseq)?
        .parse::<u32>()
        .map_err(|_| RtspError::InvalidCseq)?;
    Ok(RtspRequest { method, target, headers, body, cseq })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fragmented_request_and_binary_body() {
        let mut decoder = RtspDecoder::default();
        assert!(decoder.push(b"ANNOUNCE rtsp://host/session RTSP/1.0\r\nCSeq: 4\r\nContent-Length: 3\r\n\r\n").unwrap().is_empty());
        let decoded = decoder.push(&[0, 0xff, b'x']).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].method, "ANNOUNCE");
        assert_eq!(decoded[0].target, "rtsp://host/session");
        assert_eq!(decoded[0].cseq, 4);
        assert_eq!(decoded[0].body.as_slice(), &[0, 0xff, b'x']);
        assert_eq!(decoder.buffered_bytes(), 0);
    }

    #[test]
    fn decodes_coalesced_requests_in_order() {
        let mut decoder = RtspDecoder::default();
        let decoded = decoder.push(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\nGET_PARAMETER rtsp://host/session RTSP/1.0\r\nCSeq: 2\r\n\r\n").unwrap();
        assert_eq!(decoded.iter().map(|request| request.cseq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(decoded[0].method, "OPTIONS");
        assert_eq!(decoded[1].method, "GET_PARAMETER");
    }

    #[test]
    fn rejects_missing_duplicate_or_invalid_cseq() {
        let mut decoder = RtspDecoder::default();
        assert_eq!(decoder.push(b"OPTIONS * RTSP/1.0\r\n\r\n").unwrap_err(), RtspError::MissingCseq);
        let mut decoder = RtspDecoder::default();
        assert_eq!(decoder.push(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nCSeq: 2\r\n\r\n").unwrap_err(), RtspError::DuplicateCriticalHeader("cseq"));
        let mut decoder = RtspDecoder::default();
        assert_eq!(decoder.push(b"OPTIONS * RTSP/1.0\r\nCSeq: x\r\n\r\n").unwrap_err(), RtspError::InvalidCseq);
    }

    #[test]
    fn rejects_oversized_content_length_before_waiting_for_body() {
        let mut decoder = RtspDecoder::default();
        let request = format!("ANNOUNCE rtsp://host/session RTSP/1.0\r\nCSeq: 4\r\nContent-Length: {}\r\n\r\n", MAX_RTSP_MESSAGE_BYTES);
        assert_eq!(decoder.push(request.as_bytes()).unwrap_err(), RtspError::MessageTooLarge);
    }

    #[test]
    fn rejects_unsupported_versions_transfer_encoding_and_bad_headers() {
        let mut decoder = RtspDecoder::default();
        assert_eq!(decoder.push(b"OPTIONS * RTSP/2.0\r\nCSeq: 1\r\n\r\n").unwrap_err(), RtspError::UnsupportedVersion);
        let mut decoder = RtspDecoder::default();
        assert_eq!(decoder.push(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap_err(), RtspError::UnsupportedTransferEncoding);
        let mut decoder = RtspDecoder::default();
        assert_eq!(decoder.push(b"OPTIONS * RTSP/1.0\r\nnot-a-header\r\n\r\n").unwrap_err(), RtspError::InvalidHeader);
    }
}
