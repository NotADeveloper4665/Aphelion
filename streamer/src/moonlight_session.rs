//! Conservative RTSP session sequencing for the Moonlight adapter.
//!
//! This implements request ordering, CSeq correlation, duplicate replay, and the
//! ANNOUNCE/SETUP/PLAY/TEARDOWN lifecycle. Bounded SDP extraction and UDP RTP/RTCP
//! port negotiation are implemented; authentication, pairing, control-channel
//! association, UDP ping handling, and session-key negotiation remain separate work.

use std::{collections::VecDeque, net::{IpAddr, SocketAddr}};

use crate::moonlight_sdp::{parse_moonlight_sdp, MoonlightStreamProfile};
use crate::rtsp::RtspRequest;

const RESPONSE_CACHE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    AwaitingAnnounce,
    Configuring,
    Ready,
    Playing,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTrack {
    Video,
    Audio,
    Control,
}

impl MediaTrack {
    fn from_target(target: &str) -> Option<Self> {
        let lower = target.to_ascii_lowercase();
        if lower.contains("streamid=video") || lower.ends_with("/video") { Some(Self::Video) }
        else if lower.contains("streamid=audio") || lower.ends_with("/audio") { Some(Self::Audio) }
        else if lower.contains("streamid=control") || lower.ends_with("/control") { Some(Self::Control) }
        else { None }
    }

    fn bit(self) -> u8 {
        match self { Self::Video => 1, Self::Audio => 2, Self::Control => 4 }
    }
}

/// Consecutive RTP/RTCP ports. RTP starts on an even UDP port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPortPair { rtp: u16, rtcp: u16 }

impl MediaPortPair {
    pub fn new(rtp: u16, rtcp: u16) -> Option<Self> {
        (rtp >= 1024 && rtp % 2 == 0 && rtcp == rtp.checked_add(1)?).then_some(Self { rtp, rtcp })
    }
    pub fn rtp(self) -> u16 { self.rtp }
    pub fn rtcp(self) -> u16 { self.rtcp }
}

/// Ports that the media layer has already reserved for each RTSP track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoonlightMediaPorts { pub video: MediaPortPair, pub audio: MediaPortPair, pub control: MediaPortPair }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedTrackPorts { pub client: MediaPortPair, pub server: MediaPortPair }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspResponse {
    pub cseq: u32,
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RtspResponse {
    fn new(cseq: u32, status: u16, reason: &'static str) -> Self {
        Self { cseq, status, reason, headers: Vec::new(), body: Vec::new() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut output = format!("RTSP/1.0 {} {}\r\nCSeq: {}\r\n", self.status, self.reason, self.cseq).into_bytes();
        for (name, value) in &self.headers {
            output.extend_from_slice(name.as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(value.as_bytes());
            output.extend_from_slice(b"\r\n");
        }
        output.extend_from_slice(format!("Content-Length: {}\r\n\r\n", self.body.len()).as_bytes());
        output.extend_from_slice(&self.body);
        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    None,
    Announced,
    TrackConfigured(MediaTrack),
    PlaybackStarted,
    Teardown,
    ControlDisconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionResult {
    pub response: RtspResponse,
    pub event: SessionEvent,
    pub replayed: bool,
}

#[derive(Debug, Clone)]
struct CachedResponse {
    request: RtspRequest,
    response: RtspResponse,
    event: SessionEvent,
}

pub struct MoonlightRtspSession {
    session_id: String,
    state: SessionState,
    tracks: u8,
    cache: VecDeque<CachedResponse>,
    highest_cseq: Option<u32>,
    stream_profile: Option<MoonlightStreamProfile>,
    media_ports: Option<MoonlightMediaPorts>,
    negotiated_ports: [Option<NegotiatedTrackPorts>; 3],
}

impl MoonlightRtspSession {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), state: SessionState::AwaitingAnnounce, tracks: 0, cache: VecDeque::new(), highest_cseq: None, stream_profile: None, media_ports: None, negotiated_ports: [None; 3] }
    }

    /// Construct a session after the media layer has bound the server-side UDP pairs.
    pub fn with_media_ports(session_id: impl Into<String>, media_ports: MoonlightMediaPorts) -> Self {
        let mut session = Self::new(session_id);
        session.media_ports = Some(media_ports);
        session
    }

    pub fn state(&self) -> SessionState { self.state }

    /// The validated stream parameters from ANNOUNCE, once accepted.
    pub fn stream_profile(&self) -> Option<&MoonlightStreamProfile> { self.stream_profile.as_ref() }

    pub fn negotiated_track_ports(&self, track: MediaTrack) -> Option<NegotiatedTrackPorts> {
        self.negotiated_ports[track_index(track)]
    }

    /// Resolve the client RTP/RTCP endpoints using the address of the accepted RTSP
    /// peer. Moonlight's SETUP Transport carries ports but no trustworthy peer address.
    pub fn negotiated_client_endpoints(&self, track: MediaTrack, rtsp_peer: IpAddr) -> Option<(SocketAddr, SocketAddr)> {
        let client = self.negotiated_ports[track_index(track)]?.client;
        Some((
            SocketAddr::new(rtsp_peer, client.rtp),
            SocketAddr::new(rtsp_peer, client.rtcp),
        ))
    }

    pub fn handle(&mut self, request: RtspRequest) -> SessionResult {
        if let Some(cached) = self.cache.iter().find(|cached| cached.request.cseq == request.cseq) {
            if cached.request == request {
                return SessionResult { response: cached.response.clone(), event: cached.event.clone(), replayed: true };
            }
            return result(request.cseq, 400, "Bad Request", SessionEvent::None, false);
        }
        if self.highest_cseq.is_some_and(|last| request.cseq <= last) {
            return result(request.cseq, 400, "Bad Request", SessionEvent::None, false);
        }

        self.highest_cseq = Some(request.cseq);
        let outcome = self.handle_new(&request);
        self.cache.push_back(CachedResponse { request, response: outcome.response.clone(), event: outcome.event.clone() });
        while self.cache.len() > RESPONSE_CACHE { self.cache.pop_front(); }
        outcome
    }

    fn handle_new(&mut self, request: &RtspRequest) -> SessionResult {
        let method = request.method.to_ascii_uppercase();
        if self.state == SessionState::Closed {
            return result(request.cseq, 454, "Session Not Found", SessionEvent::None, false);
        }
        if method == "OPTIONS" {
            let mut output = result(request.cseq, 200, "OK", SessionEvent::None, false);
            output.response.headers.push(("Public".into(), "OPTIONS, ANNOUNCE, SETUP, PLAY, GET_PARAMETER, TEARDOWN".into()));
            return output;
        }
        if method == "ANNOUNCE" {
            if self.state != SessionState::AwaitingAnnounce || request.body.is_empty() {
                return result(request.cseq, 455, "Method Not Valid in This State", SessionEvent::None, false);
            }
            if request.header("content-type").is_none_or(|value| !value.eq_ignore_ascii_case("application/sdp")) {
                return result(request.cseq, 415, "Unsupported Media Type", SessionEvent::None, false);
            }
            let profile = match parse_moonlight_sdp(&request.body) {
                Ok(profile) => profile,
                Err(_) => return result(request.cseq, 400, "Invalid SDP", SessionEvent::None, false),
            };
            self.stream_profile = Some(profile);
            self.state = SessionState::Configuring;
            return result(request.cseq, 200, "OK", SessionEvent::Announced, false);
        }
        if method == "SETUP" {
            if self.state != SessionState::Configuring && self.state != SessionState::Ready {
                return result(request.cseq, 455, "Method Not Valid in This State", SessionEvent::None, false);
            }
            let Some(track) = MediaTrack::from_target(&request.target) else {
                return result(request.cseq, 404, "Not Found", SessionEvent::None, false);
            };
            let Some(transport) = request.header("transport") else {
                return result(request.cseq, 461, "Unsupported Transport", SessionEvent::None, false);
            };
            let Some(client_ports) = parse_udp_client_ports(transport) else {
                return result(request.cseq, 461, "Unsupported Transport", SessionEvent::None, false);
            };
            if request.header("session").is_some_and(|session| !session_matches(session, &self.session_id)) {
                return result(request.cseq, 454, "Session Not Found", SessionEvent::None, false);
            }
            if self.tracks & track.bit() != 0 {
                return result(request.cseq, 455, "Track Already Configured", SessionEvent::None, false);
            }
            let Some(media_ports) = self.media_ports else {
                return result(request.cseq, 503, "Media Ports Unavailable", SessionEvent::None, false);
            };
            let server_ports = match track { MediaTrack::Video => media_ports.video, MediaTrack::Audio => media_ports.audio, MediaTrack::Control => media_ports.control };
            self.negotiated_ports[track_index(track)] = Some(NegotiatedTrackPorts { client: client_ports, server: server_ports });
            self.tracks |= track.bit();
            self.state = if self.tracks & MediaTrack::Video.bit() != 0 { SessionState::Ready } else { SessionState::Configuring };
            let mut output = result(request.cseq, 200, "OK", SessionEvent::TrackConfigured(track), false);
            output.response.headers.push(("Session".into(), self.session_id.clone()));
            output.response.headers.push(("Transport".into(), format!("RTP/AVP/UDP;unicast;client_port={}-{};server_port={}-{};mode=play", client_ports.rtp, client_ports.rtcp, server_ports.rtp, server_ports.rtcp)));
            return output;
        }
        if method == "PLAY" {
            if self.state != SessionState::Ready && self.state != SessionState::Configuring {
                return result(request.cseq, 455, "Method Not Valid in This State", SessionEvent::None, false);
            }
            if self.tracks & MediaTrack::Video.bit() == 0 {
                return result(request.cseq, 455, "Video Track Not Configured", SessionEvent::None, false);
            }
            if !request.header("session").is_some_and(|session| session_matches(session, &self.session_id)) {
                return result(request.cseq, 454, "Session Not Found", SessionEvent::None, false);
            }
            self.state = SessionState::Playing;
            let mut output = result(request.cseq, 200, "OK", SessionEvent::PlaybackStarted, false);
            output.response.headers.push(("Session".into(), self.session_id.clone()));
            return output;
        }
        if method == "GET_PARAMETER" {
            if self.state != SessionState::Playing || !request.header("session").is_some_and(|session| session_matches(session, &self.session_id)) {
                return result(request.cseq, 454, "Session Not Found", SessionEvent::None, false);
            }
            let mut output = result(request.cseq, 200, "OK", SessionEvent::None, false);
            output.response.headers.push(("Session".into(), self.session_id.clone()));
            return output;
        }
        if method == "TEARDOWN" {
            if !request.header("session").is_some_and(|session| session_matches(session, &self.session_id)) {
                return result(request.cseq, 454, "Session Not Found", SessionEvent::None, false);
            }
            self.state = SessionState::Closed;
            let mut output = result(request.cseq, 200, "OK", SessionEvent::Teardown, false);
            output.response.headers.push(("Session".into(), self.session_id.clone()));
            return output;
        }
        result(request.cseq, 405, "Method Not Allowed", SessionEvent::None, false)
    }
}

fn session_matches(value: &str, expected: &str) -> bool {
    value.split(';').next().is_some_and(|id| id.trim() == expected)
}

fn track_index(track: MediaTrack) -> usize {
    match track { MediaTrack::Video => 0, MediaTrack::Audio => 1, MediaTrack::Control => 2 }
}

fn parse_udp_client_ports(transport: &str) -> Option<MediaPortPair> {
    let mut parts = transport.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/UDP") { return None; }
    let mut unicast = false;
    let mut client_ports = None;
    for parameter in parts {
        if parameter.eq_ignore_ascii_case("unicast") { unicast = true; continue; }
        if let Some((name, value)) = parameter.split_once('=') {
            if name.trim().eq_ignore_ascii_case("client_port") {
                if client_ports.is_some() { return None; }
                let (rtp, rtcp) = value.trim().split_once('-')?;
                client_ports = MediaPortPair::new(rtp.parse().ok()?, rtcp.parse().ok()?);
            }
        }
    }
    if unicast { client_ports } else { None }
}

fn result(cseq: u32, status: u16, reason: &'static str, event: SessionEvent, replayed: bool) -> SessionResult {
    SessionResult { response: RtspResponse::new(cseq, status, reason), event, replayed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::RtspDecoder;

    fn media_ports() -> MoonlightMediaPorts {
        MoonlightMediaPorts {
            video: MediaPortPair::new(48000, 48001).unwrap(),
            audio: MediaPortPair::new(48002, 48003).unwrap(),
            control: MediaPortPair::new(48004, 48005).unwrap(),
        }
    }

    fn parse(method: &str, target: &str, cseq: u32, headers: &str, body: &[u8]) -> RtspRequest {
        let wire = format!("{method} {target} RTSP/1.0\r\nCSeq: {cseq}\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = wire.into_bytes();
        bytes.extend_from_slice(body);
        RtspDecoder::default().push(&bytes).unwrap().remove(0)
    }

    const VALID_SDP: &[u8] = b"v=0\r\na=x-nv-video[0].clientViewportWd:1920\r\na=x-nv-video[0].clientViewportHt:1080\r\na=x-nv-video[0].maxFPS:60\r\na=x-nv-video[0].packetSize:1200\r\na=x-nv-vqos[0].bw.maximumBitrateKbps:30000\r\na=x-nv-vqos[0].bitStreamFormat:0\r\n";

    #[test]
    fn runs_announce_setup_play_and_teardown_in_order() {
        let mut session = MoonlightRtspSession::with_media_ports("test-session", media_ports());
        let announce = session.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", VALID_SDP));
        assert_eq!(announce.response.status, 200);
        assert_eq!(session.stream_profile().unwrap().width, 1920);
        let setup = session.handle(parse("SETUP", "rtsp://host/streamid=video", 2, "Transport: RTP/AVP/UDP;unicast;client_port=47998-47999\r\n", b""));
        assert_eq!(setup.response.status, 200);
        assert_eq!(setup.event, SessionEvent::TrackConfigured(MediaTrack::Video));
        assert_eq!(setup.response.headers.iter().find(|(name, _)| name == "Transport").unwrap().1, "RTP/AVP/UDP;unicast;client_port=47998-47999;server_port=48000-48001;mode=play");
        assert_eq!(session.negotiated_track_ports(MediaTrack::Video).unwrap().server.rtp(), 48000);
        assert_eq!(session.negotiated_client_endpoints(MediaTrack::Video, "192.0.2.12".parse().unwrap()).unwrap(), (
            "192.0.2.12:47998".parse().unwrap(),
            "192.0.2.12:47999".parse().unwrap(),
        ));
        let play = session.handle(parse("PLAY", "rtsp://host", 3, "Session: test-session\r\n", b""));
        assert_eq!(play.response.status, 200);
        assert_eq!(session.state(), SessionState::Playing);
        let teardown = session.handle(parse("TEARDOWN", "rtsp://host", 4, "Session: test-session\r\n", b""));
        assert_eq!(teardown.response.status, 200);
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn correlates_cseq_and_replays_exact_duplicates_but_rejects_conflicts() {
        let mut session = MoonlightRtspSession::new("session-a");
        let request = parse("OPTIONS", "*", 7, "", b"");
        let first = session.handle(request.clone());
        let replay = session.handle(request);
        assert_eq!(first.response, replay.response);
        assert!(replay.replayed);
        let conflicting = session.handle(parse("TEARDOWN", "rtsp://host", 7, "Session: session-a\r\n", b""));
        assert_eq!(conflicting.response.status, 400);
        assert!(!conflicting.replayed);
    }

    #[test]
    fn rejects_play_without_video_and_setup_with_wrong_session() {
        let mut session = MoonlightRtspSession::with_media_ports("expected", media_ports());
        session.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", VALID_SDP));
        let setup = session.handle(parse("SETUP", "rtsp://host/streamid=audio", 2, "Transport: RTP/AVP/UDP;unicast;client_port=47998-47999\r\nSession: other\r\n", b""));
        assert_eq!(setup.response.status, 454);
        let play = session.handle(parse("PLAY", "rtsp://host", 3, "Session: expected\r\n", b""));
        assert_eq!(play.response.status, 455);
    }

    #[test]
    fn rejects_invalid_sdp_without_advancing_session_state() {
        let mut session = MoonlightRtspSession::new("sdp-test");
        let response = session.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", b"v=0\r\n"));
        assert_eq!(response.response.status, 400);
        assert_eq!(session.state(), SessionState::AwaitingAnnounce);
        assert!(session.stream_profile().is_none());
    }

    #[test]
    fn requires_reserved_server_ports_and_valid_unicast_client_port_pairs() {
        assert!(MediaPortPair::new(47998, 48000).is_none());
        assert!(MediaPortPair::new(47999, 48000).is_none());
        let mut no_media = MoonlightRtspSession::new("no-ports");
        no_media.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", VALID_SDP));
        let no_listener = no_media.handle(parse("SETUP", "rtsp://host/streamid=video", 2, "Transport: RTP/AVP/UDP;unicast;client_port=47998-47999\r\n", b""));
        assert_eq!(no_listener.response.status, 503);

        let mut session = MoonlightRtspSession::with_media_ports("bad-client", media_ports());
        session.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", VALID_SDP));
        let bad_pair = session.handle(parse("SETUP", "rtsp://host/streamid=video", 2, "Transport: RTP/AVP/UDP;unicast;client_port=47999-48000\r\n", b""));
        assert_eq!(bad_pair.response.status, 461);
        assert_eq!(session.state(), SessionState::Configuring);
    }
}
