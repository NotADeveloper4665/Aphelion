//! Bounded RTSP/TCP connection adapter for the Moonlight session state machine.
//!
//! A caller owns the listener, authentication, and socket timeouts. This layer only
//! frames requests, applies the existing session policy, writes ordered responses,
//! and reports lifecycle events after the response has reached the socket.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::moonlight_session::{MoonlightRtspSession, SessionEvent, SessionState};
use crate::rtsp::{RtspDecoder, RtspError};

const READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub enum RtspConnectionError {
    Io(io::Error),
    Protocol(RtspError),
    TruncatedMessage(usize),
}

impl std::fmt::Display for RtspConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "RTSP connection I/O failed: {error}"),
            Self::Protocol(error) => write!(f, "invalid RTSP connection: {error}"),
            Self::TruncatedMessage(bytes) => write!(f, "RTSP peer disconnected with {bytes} buffered message bytes"),
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
    let mut decoder = RtspDecoder::default();
    let mut input = [0; READ_CHUNK_BYTES];
    let mut events = Vec::new();

    loop {
        let read = match stream.read(&mut input) {
            Ok(0) => {
                let buffered = decoder.buffered_bytes();
                return if buffered == 0 {
                    Ok(events)
                } else {
                    Err(RtspConnectionError::TruncatedMessage(buffered))
                };
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(RtspConnectionError::Io(error)),
        };

        let requests = decoder.push(&input[..read]).map_err(RtspConnectionError::Protocol)?;
        for request in requests {
            let result = session.handle(request);
            stream.write_all(&result.response.encode()).map_err(RtspConnectionError::Io)?;
            if result.event != SessionEvent::None {
                on_event(&result.event);
                events.push(result.event);
            }
            if session.state() == SessionState::Closed {
                return Ok(events);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread, time::Duration};

    use crate::{
        moonlight_session::{MediaPortPair, MoonlightMediaPorts},
    };

    use super::*;

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
    fn dispatches_fragmented_play_and_teardown_then_reports_events() {
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
            let result = serve_rtsp_connection(&mut stream, &mut session, |event| callbacks.push(event.clone()));
            (result.unwrap(), callbacks, session.state())
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

        let setup = request("SETUP", 3, "Transport: RTP/AVP/UDP;unicast;client_port=6000-6001\r\n");
        client.write_all(setup.as_bytes()).unwrap();
        let setup_response = read_response(&mut client);
        assert!(setup_response.contains("server_port=5000-5001"));

        let play = request("PLAY", 4, "Session: session-id\r\n");
        client.write_all(play.as_bytes()).unwrap();
        assert!(read_response(&mut client).starts_with("RTSP/1.0 200 OK\r\nCSeq: 4"));

        let teardown = request("TEARDOWN", 5, "Session: session-id\r\n");
        client.write_all(teardown.as_bytes()).unwrap();
        assert!(read_response(&mut client).contains("CSeq: 5"));

        let (events, callbacks, state) = server.join().unwrap();
        assert_eq!(events, callbacks);
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
}
