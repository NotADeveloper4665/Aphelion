//! Conservative RTSP session sequencing for the Moonlight adapter.
//!
//! This implements request ordering, CSeq correlation, duplicate replay, and the
//! ANNOUNCE/SETUP/PLAY/TEARDOWN lifecycle. Authentication, pairing, client capability
//! parsing, UDP ping association, and session-key negotiation are intentionally not
//! implied by accepting a request here.

use std::collections::VecDeque;

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
}

impl MoonlightRtspSession {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), state: SessionState::AwaitingAnnounce, tracks: 0, cache: VecDeque::new(), highest_cseq: None }
    }

    pub fn state(&self) -> SessionState { self.state }

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
            if !transport.to_ascii_uppercase().contains("RTP/AVP/UDP") || !transport.to_ascii_lowercase().contains("client_port=") {
                return result(request.cseq, 461, "Unsupported Transport", SessionEvent::None, false);
            }
            if request.header("session").is_some_and(|session| !session_matches(session, &self.session_id)) {
                return result(request.cseq, 454, "Session Not Found", SessionEvent::None, false);
            }
            if self.tracks & track.bit() != 0 {
                return result(request.cseq, 455, "Track Already Configured", SessionEvent::None, false);
            }
            self.tracks |= track.bit();
            self.state = if self.tracks & MediaTrack::Video.bit() != 0 { SessionState::Ready } else { SessionState::Configuring };
            let mut output = result(request.cseq, 200, "OK", SessionEvent::TrackConfigured(track), false);
            output.response.headers.push(("Session".into(), self.session_id.clone()));
            output.response.headers.push(("Transport".into(), transport.to_owned()));
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

fn result(cseq: u32, status: u16, reason: &'static str, event: SessionEvent, replayed: bool) -> SessionResult {
    SessionResult { response: RtspResponse::new(cseq, status, reason), event, replayed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::RtspDecoder;

    fn parse(method: &str, target: &str, cseq: u32, headers: &str, body: &[u8]) -> RtspRequest {
        let wire = format!("{method} {target} RTSP/1.0\r\nCSeq: {cseq}\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = wire.into_bytes();
        bytes.extend_from_slice(body);
        RtspDecoder::default().push(&bytes).unwrap().remove(0)
    }

    #[test]
    fn runs_announce_setup_play_and_teardown_in_order() {
        let mut session = MoonlightRtspSession::new("test-session");
        let announce = session.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", b"v=0\r\n"));
        assert_eq!(announce.response.status, 200);
        let setup = session.handle(parse("SETUP", "rtsp://host/streamid=video", 2, "Transport: RTP/AVP/UDP;unicast;client_port=47998-47999\r\n", b""));
        assert_eq!(setup.response.status, 200);
        assert_eq!(setup.event, SessionEvent::TrackConfigured(MediaTrack::Video));
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
        let mut session = MoonlightRtspSession::new("expected");
        session.handle(parse("ANNOUNCE", "rtsp://host", 1, "Content-Type: application/sdp\r\n", b"v=0"));
        let setup = session.handle(parse("SETUP", "rtsp://host/streamid=audio", 2, "Transport: RTP/AVP/UDP;unicast;client_port=1-2\r\nSession: other\r\n", b""));
        assert_eq!(setup.response.status, 454);
        let play = session.handle(parse("PLAY", "rtsp://host", 3, "Session: expected\r\n", b""));
        assert_eq!(play.response.status, 455);
    }
}
