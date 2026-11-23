//! Bounded RTSP/TCP connection adapter for the Moonlight session state machine.
//!
//! A caller owns the listener, authentication, and socket timeouts. This layer only
//! frames requests, applies the existing session policy, writes ordered responses,
//! and reports lifecycle events after the response has reached the socket.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::moonlight_session::{MoonlightRtspSession, SessionEvent, SessionState};
use crate::rtsp::{RtspDecoder, RtspError};
use crate::moonlight_rtsp_crypto::{MoonlightRtspCipher, RtspCryptoError, MAX_RTSP_PLAINTEXT_BYTES};

const READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub enum RtspConnectionError {
    Io(io::Error),
    Protocol(RtspError),
    TruncatedMessage(usize),
    TruncatedEncryptedRecord,
    InvalidEncryptedPayload,
    Crypto(RtspCryptoError),
}

impl std::fmt::Display for RtspConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "RTSP connection I/O failed: {error}"),
            Self::Protocol(error) => write!(f, "invalid RTSP connection: {error}"),
            Self::TruncatedMessage(bytes) => write!(f, "RTSP peer disconnected with {bytes} buffered message bytes"),
            Self::TruncatedEncryptedRecord => f.write_str("RTSP peer disconnected during an encrypted record"),
            Self::InvalidEncryptedPayload => f.write_str("encrypted RTSP record must contain exactly one complete request"),
            Self::Crypto(error) => write!(f, "encrypted RTSP processing failed: {error}"),
        }
    }
}

impl std::error::Error for RtspConnectionError {}

/// Serve one already-accepted RTSP/TCP connection until EOF or TEARDOWN.
///
/// Set a finite read timeout on `stream` before calling this function. The callback
/// runs after each response write, so `PlaybackStarted` cannot start media before the
/// RTSP PLAY response is sent. Authentication and client certificate pairing are
/// intentionally not implied by this transport adapter.
pub fn serve_rtsp_connection(
    stream: &mut TcpStream,
    session: &mut MoonlightRtspSession,
    mut on_event: impl FnMut(&SessionEvent),
) -> Result<Vec<SessionEvent>, RtspConnectionError> {
    serve_rtsp_connection_with_session_callback(stream, session, |event, _session| on_event(event))
}

/// Session-aware callback version for media lifecycle integration.
pub fn serve_rtsp_connection_with_session_callback(
    stream: &mut TcpStream,
    session: &mut MoonlightRtspSession,
    mut on_event: impl FnMut(&SessionEvent, &mut MoonlightRtspSession),
) -> Result<Vec<SessionEvent>, RtspConnectionError> {
    let mut decoder = RtspDecoder::default();
    let mut input = [0; READ_CHUNK_BYTES];
    let mut events = Vec::new();

    loop {
        let read = match stream.read(&mut input) {
            Ok(0) => {
                let buffered = decoder.buffered_bytes();
                if buffered != 0 { return Err(RtspConnectionError::TruncatedMessage(buffered)); }
                if session.state() != SessionState::AwaitingAnnounce && session.state() != SessionState::Closed {
                    on_event(&SessionEvent::ControlDisconnected, session);
                    events.push(SessionEvent::ControlDisconnected);
                }
                return Ok(events);
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(RtspConnectionError::Io(error)),
        };

        let requests = decoder.push(&input[..read]).map_err(RtspConnectionError::Protocol)?;
        for request in requests {
            let result = session.handle(request);
            stream.write_all(&result.response.encode()).map_err(RtspConnectionError::Io)?;
            if !result.replayed && result.event != SessionEvent::None {
                on_event(&result.event, session);
                events.push(result.event);
            }
            if session.state() == SessionState::Closed {
                return Ok(events);
            }
        }
    }
}


/// Serve one already-accepted encrypted RTSP/TCP connection.
///
/// The caller selects this adapter only after launch negotiation enabled encrypted RTSP,
/// supplies the per-launch key/cipher, and sets finite socket timeouts. Every encrypted
/// record is authenticated before RTSP parsing, lifecycle callbacks run after response
/// writes, and cleartext records are rejected.
pub fn serve_encrypted_rtsp_connection(
    stream: &mut TcpStream,
    session: &mut MoonlightRtspSession,
    cipher: &mut MoonlightRtspCipher,
    mut on_event: impl FnMut(&SessionEvent),
) -> Result<Vec<SessionEvent>, RtspConnectionError> {
    serve_encrypted_rtsp_connection_with_session_callback(stream, session, cipher, |event, _session| on_event(event))
}

/// Session-aware callback version for media lifecycle integration.
pub fn serve_encrypted_rtsp_connection_with_session_callback(
    stream: &mut TcpStream,
    session: &mut MoonlightRtspSession,
    cipher: &mut MoonlightRtspCipher,
    mut on_event: impl FnMut(&SessionEvent, &mut MoonlightRtspSession),
) -> Result<Vec<SessionEvent>, RtspConnectionError> {
    const HEADER_BYTES: usize = 24;
    let mut events = Vec::new();
    loop {
        let mut header = [0_u8; HEADER_BYTES];
        match stream.read(&mut header[..1]) {
            Ok(0) => {
                if session.state() != SessionState::AwaitingAnnounce && session.state() != SessionState::Closed {
                    on_event(&SessionEvent::ControlDisconnected, session);
                    events.push(SessionEvent::ControlDisconnected);
                }
                return Ok(events);
            }
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read cannot return more than one byte"),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(RtspConnectionError::Io(error)),
        }
        stream.read_exact(&mut header[1..]).map_err(|_| RtspConnectionError::TruncatedEncryptedRecord)?;
        let type_and_length = u32::from_be_bytes(header[..4].try_into().map_err(|_| RtspConnectionError::InvalidEncryptedPayload)?);
        if type_and_length & 0x8000_0000 == 0 {
            return Err(RtspConnectionError::Crypto(RtspCryptoError::CleartextFrame));
        }
        let payload_len = (type_and_length & 0x7fff_ffff) as usize;
        if payload_len == 0 {
            return Err(RtspConnectionError::InvalidEncryptedPayload);
        }
        if payload_len > MAX_RTSP_PLAINTEXT_BYTES {
            return Err(RtspConnectionError::Crypto(RtspCryptoError::PayloadTooLarge));
        }

        let mut record = Vec::with_capacity(HEADER_BYTES + payload_len);
        record.extend_from_slice(&header);
        record.resize(HEADER_BYTES + payload_len, 0);
        stream.read_exact(&mut record[HEADER_BYTES..])
            .map_err(|_| RtspConnectionError::TruncatedEncryptedRecord)?;
        let plaintext = cipher.open_client_record(&record).map_err(RtspConnectionError::Crypto)?;

        let mut decoder = RtspDecoder::default();
        let mut requests = decoder.push(&plaintext).map_err(RtspConnectionError::Protocol)?;
        if requests.len() != 1 || decoder.buffered_bytes() != 0 {
            return Err(RtspConnectionError::InvalidEncryptedPayload);
        }
        let request = requests.pop().expect("one complete RTSP request checked");
        let result = session.handle(request);
        let encrypted_response = cipher.seal_host_record(&result.response.encode()).map_err(RtspConnectionError::Crypto)?;
        stream.write_all(&encrypted_response).map_err(RtspConnectionError::Io)?;
        if !result.replayed && result.event != SessionEvent::None {
            on_event(&result.event, session);
            events.push(result.event);
        }
        if session.state() == SessionState::Closed {
            return Ok(events);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread, time::Duration};
    use aes_gcm::{aead::{Aead, KeyInit}, Aes128Gcm, Nonce};

    use crate::{
        moonlight_session::{MediaPortPair, MoonlightMediaPorts},
    };

    use super::*;

    fn client_encrypted_record(key: &[u8; 16], sequence: u32, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0_u8; 12];
        nonce[..4].copy_from_slice(&sequence.to_be_bytes());
        nonce[10] = b'C';
        nonce[11] = b'R';
        let cipher = Aes128Gcm::new_from_slice(key).unwrap();
        let sealed = cipher.encrypt(Nonce::from_slice(&nonce), plaintext).unwrap();
        let split = sealed.len() - 16;
        let mut record = Vec::new();
        record.extend_from_slice(&(0x8000_0000_u32 | plaintext.len() as u32).to_be_bytes());
        record.extend_from_slice(&sequence.to_be_bytes());
        record.extend_from_slice(&sealed[split..]);
        record.extend_from_slice(&sealed[..split]);
        record
    }

    fn read_encrypted_response(stream: &mut TcpStream) -> Vec<u8> {
        let mut record = vec![0_u8; 24];
        stream.read_exact(&mut record).unwrap();
        let payload_len = (u32::from_be_bytes(record[..4].try_into().unwrap()) & 0x7fff_ffff) as usize;
        record.resize(24 + payload_len, 0);
        stream.read_exact(&mut record[24..]).unwrap();
        record
    }

    fn decrypt_host_record(key: &[u8; 16], record: &[u8]) -> Vec<u8> {
        let sequence = u32::from_be_bytes(record[4..8].try_into().unwrap());
        let mut nonce = [0_u8; 12];
        nonce[..8].copy_from_slice(&(sequence as u64).to_le_bytes());
        nonce[10] = b'H';
        nonce[11] = b'R';
        let mut sealed = record[24..].to_vec();
        sealed.extend_from_slice(&record[8..24]);
        Aes128Gcm::new_from_slice(key).unwrap()
            .decrypt(Nonce::from_slice(&nonce), sealed.as_ref()).unwrap()
    }

    fn request(method: &str, cseq: u32, extra: &str) -> String {
        format!("{method} rtsp://127.0.0.1/session RTSP/1.0\r\nCSeq: {cseq}\r\n{extra}\r\n")
    }

    fn read_response(stream: &mut TcpStream) -> String {
        let mut response = Vec::new();
        let mut chunk = [0; 512];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            assert_ne!(read, 0, "peer closed before completing an RTSP response");
            response.extend_from_slice(&chunk[..read]);
            let Some(headers_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else { continue };
            let headers = String::from_utf8_lossy(&response[..headers_end]);
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
            }).unwrap_or(0);
            if response.len() >= headers_end + 4 + content_length { break }
        }
        String::from_utf8(response).unwrap()
    }


    #[test]
    fn serves_authenticated_encrypted_rtsp_record_and_disconnect_lifecycle() {
        let key = [0x52; 16];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut session = MoonlightRtspSession::new("encrypted-session");
            let launch = crate::moonlight_launch::MoonlightLaunchCrypto::from_rikey_hex(&"52".repeat(16)).unwrap();
            let mut ciphers = launch.establish(true, false).unwrap();
            let mut cipher = ciphers.rtsp.take().unwrap();
            let mut callbacks = Vec::new();
            let result = serve_encrypted_rtsp_connection(
                &mut stream, &mut session, &mut cipher, |event| callbacks.push(event.clone())
            );
            (result.unwrap(), callbacks, session.state())
        });

        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let sdp = b"v=0\r\ns=Aphelion\r\na=x-nv-video[0].clientViewportWd:1280\r\na=x-nv-video[0].clientViewportHt:720\r\na=x-nv-video[0].maxFPS:60\r\na=x-nv-video[0].packetSize:1200\r\na=x-nv-vqos[0].bw.maximumBitrateKbps:10000\r\na=x-nv-vqos[0].bitStreamFormat:0\r\n";
        let request = format!(
            "ANNOUNCE rtsp://127.0.0.1/session RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            sdp.len()
        );
        let mut plaintext = request.into_bytes();
        plaintext.extend_from_slice(sdp);
        client.write_all(&client_encrypted_record(&key, 1, &plaintext)).unwrap();
        let response = decrypt_host_record(&key, &read_encrypted_response(&mut client));
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("RTSP/1.0 200 OK\r\nCSeq: 1\r\n"));
        // A client may retry after losing the first response. Give the replay a fresh
        // encryption sequence while preserving the RTSP request identity.
        client.write_all(&client_encrypted_record(&key, 2, &plaintext)).unwrap();
        let replay = decrypt_host_record(&key, &read_encrypted_response(&mut client));
        assert_eq!(replay, response.as_bytes());
        drop(client);

        let (events, callbacks, state) = server.join().unwrap();
        assert_eq!(events, vec![SessionEvent::Announced, SessionEvent::ControlDisconnected]);
        assert_eq!(events, callbacks);
        assert_eq!(state, SessionState::Configuring);
    }

    #[test]
    fn serves_rtsp_session_over_tcp_and_reports_ordered_lifecycle_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let ports = MoonlightMediaPorts {
                video: MediaPortPair::new(5000, 5001).unwrap(),
                audio: MediaPortPair::new(5002, 5003).unwrap(),
                control: MediaPortPair::new(5004, 5005).unwrap(),
            };
            let mut session = MoonlightRtspSession::with_media_ports("session-id", ports);
            let mut callbacks = Vec::new();
            let mut media_ready_at_play = false;
            let result = serve_rtsp_connection_with_session_callback(
                &mut stream, &mut session, |event, session| {
                    callbacks.push(event.clone());
                    if *event == SessionEvent::PlaybackStarted {
                        media_ready_at_play = session.stream_profile().is_some()
                            && session.negotiated_track_ports(crate::moonlight_session::MediaTrack::Video).is_some();
                    }
                },
            );
            (result.unwrap(), callbacks, session.state(), media_ready_at_play)
        });

        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client.write_all(request("OPTIONS", 1, "").as_bytes()).unwrap();
        assert!(read_response(&mut client).contains("CSeq: 1"));

        let sdp = b"v=0\r\ns=Aphelion\r\na=x-nv-video[0].clientViewportWd:1280\r\na=x-nv-video[0].clientViewportHt:720\r\na=x-nv-video[0].maxFPS:60\r\na=x-nv-video[0].packetSize:1200\r\na=x-nv-vqos[0].bw.maximumBitrateKbps:10000\r\na=x-nv-vqos[0].bitStreamFormat:0\r\n";
        let announce = format!("ANNOUNCE rtsp://127.0.0.1/session RTSP/1.0\r\nCSeq: 2\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n", sdp.len());
        client.write_all(announce.as_bytes()).unwrap();
        client.write_all(sdp).unwrap();
        assert!(read_response(&mut client).contains("CSeq: 2"));

        let setup = format!("SETUP rtsp://127.0.0.1/streamid=video RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/UDP;unicast;client_port=6000-6001\r\n\r\n");
        client.write_all(setup.as_bytes()).unwrap();
        let setup_response = read_response(&mut client);
        assert!(setup_response.contains("server_port=5000-5001"));

        let play = request("PLAY", 4, "Session: session-id\r\n");
        client.write_all(play.as_bytes()).unwrap();
        assert!(read_response(&mut client).starts_with("RTSP/1.0 200 OK\r\nCSeq: 4"));

        let teardown = request("TEARDOWN", 5, "Session: session-id\r\n");
        client.write_all(teardown.as_bytes()).unwrap();
        assert!(read_response(&mut client).contains("CSeq: 5"));

        let (events, callbacks, state, media_ready_at_play) = server.join().unwrap();
        assert_eq!(events, callbacks);
        assert!(media_ready_at_play, "PLAY callback must see negotiated profile and video ports");
        assert_eq!(events, vec![SessionEvent::Announced, SessionEvent::TrackConfigured(crate::moonlight_session::MediaTrack::Video), SessionEvent::PlaybackStarted, SessionEvent::Teardown]);
        assert_eq!(state, SessionState::Closed);
    }

    #[test]
    fn rejects_a_peer_that_disconnects_mid_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut session = MoonlightRtspSession::new("session-id");
            serve_rtsp_connection(&mut stream, &mut session, |_| {})
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n").unwrap();
        drop(client);
        assert!(matches!(server.join().unwrap(), Err(RtspConnectionError::TruncatedMessage(_))));
    }

    #[test]
    fn reports_clean_control_disconnect_after_session_setup() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut session = MoonlightRtspSession::new("session-id");
            let mut events = Vec::new();
            let result = serve_rtsp_connection(&mut stream, &mut session, |event| events.push(event.clone()));
            (result.unwrap(), events, session.state())
        });

        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let sdp = b"v=0\r\ns=Aphelion\r\na=x-nv-video[0].clientViewportWd:1280\r\na=x-nv-video[0].clientViewportHt:720\r\na=x-nv-video[0].maxFPS:60\r\na=x-nv-video[0].packetSize:1200\r\na=x-nv-vqos[0].bw.maximumBitrateKbps:10000\r\na=x-nv-vqos[0].bitStreamFormat:0\r\n";
        let announce = format!("ANNOUNCE rtsp://127.0.0.1/session RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n", sdp.len());
        client.write_all(announce.as_bytes()).unwrap();
        client.write_all(sdp).unwrap();
        assert!(read_response(&mut client).contains("CSeq: 1"));
        drop(client);

        let (events, callbacks, state) = server.join().unwrap();
        assert_eq!(events, vec![SessionEvent::Announced, SessionEvent::ControlDisconnected]);
        assert_eq!(events, callbacks);
        assert_eq!(state, SessionState::Configuring);
    }
}
