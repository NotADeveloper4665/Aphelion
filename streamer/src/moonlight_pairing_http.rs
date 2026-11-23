//! Bounded XML/query adapter around the authenticated Moonlight pairing transaction.
//! The embedding worker owns HTTP/TLS sockets and operator approval; only a successful
//! final client proof returns a certificate record for persistence.
use crate::moonlight_pairing::{MoonlightPairingTransaction, PairedClient, PairingError};

const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_PAIRING_HEX_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingEndpointError {
    QueryTooLarge, MalformedQuery, DuplicateParameter, MissingPhrase,
    UnknownPhrase, MissingPayload, UnexpectedPayload, InvalidHex, PayloadTooLarge,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingPhrase { GetServerCert, PairChallenge, ClientChallenge, ServerChallengeResponse, ClientPairingSecret }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingEndpointRequest { phrase: PairingPhrase, payload: Option<Vec<u8>> }

impl PairingEndpointRequest {
    /// Parse a GameStream /pair query. Unknown metadata parameters are ignored.
    pub fn parse_query(query: &str) -> Result<Self, PairingEndpointError> {
        if query.len() > MAX_QUERY_BYTES { return Err(PairingEndpointError::QueryTooLarge); }
        let parameters = crate::http::parse_query(query.trim_start_matches('?')).map_err(|error| match error {
            crate::http::QueryError::TooManyParameters => PairingEndpointError::QueryTooLarge,
            crate::http::QueryError::DuplicateParameter => PairingEndpointError::DuplicateParameter,
            crate::http::QueryError::MalformedEscape
            | crate::http::QueryError::InvalidUtf8
            | crate::http::QueryError::InvalidControl => PairingEndpointError::MalformedQuery,
        })?;
        let mut phrase = None;
        let mut payloads = Vec::new();
        for (name, value) in &parameters {
            match name.as_str() {
                "phrase" => phrase = Some(match value.as_str() {
                    "getservercert" => PairingPhrase::GetServerCert,
                    "pairchallenge" => PairingPhrase::PairChallenge,
                    "clientchallenge" => PairingPhrase::ClientChallenge,
                    "serverchallengeresp" => PairingPhrase::ServerChallengeResponse,
                    "clientpairingsecret" => PairingPhrase::ClientPairingSecret,
                    _ => return Err(PairingEndpointError::UnknownPhrase),
                }),
                "clientchallenge" | "serverchallengeresp" | "clientpairingsecret" => payloads.push((name.as_str(), value.as_str())),
                _ => {}
            }
        }
        let phrase = phrase.ok_or(PairingEndpointError::MissingPhrase)?;
        let expected = match phrase {
            PairingPhrase::GetServerCert | PairingPhrase::PairChallenge => None,
            PairingPhrase::ClientChallenge => Some("clientchallenge"),
            PairingPhrase::ServerChallengeResponse => Some("serverchallengeresp"),
            PairingPhrase::ClientPairingSecret => Some("clientpairingsecret"),
        };
        let payload = match (expected, payloads.as_slice()) {
            (None, []) => None,
            (None, _) => return Err(PairingEndpointError::UnexpectedPayload),
            (Some(_), []) => return Err(PairingEndpointError::MissingPayload),
            (Some(expected), [(actual, value)]) if expected == *actual => Some(decode_hex(value)?),
            (Some(_), _) => return Err(PairingEndpointError::UnexpectedPayload),
        };
        Ok(Self { phrase, payload })
    }
}

pub struct PairingEndpointResponse {
    pub status_code: u16,
    pub xml: String,
    /// Returned only after the final client proof passes.
    pub paired_client: Option<PairedClient>,
}
impl PairingEndpointResponse {
    pub fn handle(transaction: &mut MoonlightPairingTransaction, request: PairingEndpointRequest) -> Self {
        let result: Result<(Option<(&'static str, Vec<u8>)>, Option<PairedClient>), PairingError> =
            match request.phrase {
                PairingPhrase::GetServerCert => transaction.get_server_certificate().map(|cert| (Some(("plaincert", cert)), None)),
                PairingPhrase::PairChallenge => Ok((None, None)),
                PairingPhrase::ClientChallenge => transaction.client_challenge(request.payload.as_deref().expect("parser enforces payload"))
                    .map(|value| (Some(("challengeresponse", value)), None)),
                PairingPhrase::ServerChallengeResponse => transaction.server_challenge_response(request.payload.as_deref().expect("parser enforces payload"))
                    .map(|value| (Some(("pairingsecret", value)), None)),
                PairingPhrase::ClientPairingSecret => transaction.client_pairing_secret(request.payload.as_deref().expect("parser enforces payload"))
                    .map(|client| (None, Some(client))),
            };
        match result {
            Ok((field, paired_client)) => Self {
                status_code: 200,
                xml: response_xml(200, true, field.as_ref().map(|(name, value)| (*name, value.as_slice()))),
                paired_client,
            },
            Err(_) => Self { status_code: 400, xml: response_xml(400, false, None), paired_client: None },
        }
    }
}

fn response_xml(status: u16, paired: bool, field: Option<(&str, &[u8])>) -> String {
    let mut xml = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"{status}\"><paired>{}</paired>", u8::from(paired));
    if let Some((name, bytes)) = field {
        xml.push('<'); xml.push_str(name); xml.push('>');
        for byte in bytes { xml.push_str(&format!("{byte:02X}")); }
        xml.push_str("</"); xml.push_str(name); xml.push('>');
    }
    xml.push_str("</root>");
    xml
}
fn decode_hex(value: &str) -> Result<Vec<u8>, PairingEndpointError> {
    if value.len() > MAX_PAIRING_HEX_BYTES { return Err(PairingEndpointError::PayloadTooLarge); }
    if value.is_empty() || value.len() % 2 != 0 { return Err(PairingEndpointError::InvalidHex); }
    let mut output = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        output.push((nibble(pair[0]).ok_or(PairingEndpointError::InvalidHex)? << 4)
            | nibble(pair[1]).ok_or(PairingEndpointError::InvalidHex)?);
    }
    Ok(output)
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
    #[test]
    fn parses_all_pairing_stages_and_payload_hex() {
        assert_eq!(PairingEndpointRequest::parse_query("phrase=getservercert&salt=0011").unwrap(),
            PairingEndpointRequest { phrase: PairingPhrase::GetServerCert, payload: None });
        assert_eq!(PairingEndpointRequest::parse_query("phrase=pairchallenge").unwrap().phrase, PairingPhrase::PairChallenge);\n        assert_eq!(PairingEndpointRequest::parse_query("phrase=clientchallenge&clientchallenge=00aF").unwrap().payload, Some(vec![0, 0xaf]));
        assert_eq!(PairingEndpointRequest::parse_query("?phrase=serverchallengeresp&serverchallengeresp=0102").unwrap().payload, Some(vec![1, 2]));
        assert_eq!(PairingEndpointRequest::parse_query("phrase=clientpairingsecret&clientpairingsecret=ff").unwrap().payload, Some(vec![255]));
    }
    #[test]
    fn rejects_ambiguous_malformed_and_oversized_queries() {
        assert_eq!(PairingEndpointRequest::parse_query("phrase=unknown"), Err(PairingEndpointError::UnknownPhrase));
        assert_eq!(PairingEndpointRequest::parse_query("phrase=clientchallenge"), Err(PairingEndpointError::MissingPayload));
        assert_eq!(PairingEndpointRequest::parse_query("phrase=clientchallenge&clientchallenge=0x"), Err(PairingEndpointError::InvalidHex));
        assert_eq!(PairingEndpointRequest::parse_query("phrase=getservercert&phrase=getservercert"), Err(PairingEndpointError::DuplicateParameter));
        assert_eq!(PairingEndpointRequest::parse_query("phrase=getservercert&clientchallenge=00"), Err(PairingEndpointError::UnexpectedPayload));
        assert_eq!(PairingEndpointRequest::parse_query(&format!("phrase=getservercert&x={}", "a".repeat(MAX_QUERY_BYTES))), Err(PairingEndpointError::QueryTooLarge));
    }
    #[test]
    fn serializes_xml_status_and_hex_fields() {
        assert_eq!(response_xml(200, true, Some(("plaincert", &[0x0a, 0xff]))),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"200\"><paired>1</paired><plaincert>0AFF</plaincert></root>");
        assert_eq!(response_xml(400, false, None),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"400\"><paired>0</paired></root>");
    }
}
