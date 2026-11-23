//! Joins a capture source, software H.264 encoder, Moonlight video packetizer, and UDP
//! sender. The caller must complete client authentication/session negotiation first and
//! supply the negotiated peer, cipher, and client capabilities.

use std::{net::IpAddr, time::{Duration, Instant}};

use crate::{
    capture::{CaptureError, FrameSource},
    encoder::{EncodedAccessUnit, EncoderError, FfmpegH264Encoder},
    moonlight_video::{
        MoonlightSendError, MoonlightSendStats, MoonlightUdpSender, MoonlightVideoConfig,
        ReservedUdpPortPair,
    },
    moonlight_sdp::{MoonlightStreamProfile, VideoCodec},
    moonlight_session::MediaPortPair,
    moonlight_launch::NegotiatedMoonlightCiphers,
    encoder::EncoderConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStep {
    Buffered,
    Sent { frame_index: u32, stats: MoonlightSendStats },
}

/// Measured counters from the software video path. Timestamps currently begin when Rust receives a complete raw frame. These values describe only this
/// worker-local pipeline and do not imply end-to-end client latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipelineHealth {
    pub captured_frames: u64,
    pub sent_frames: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub capture_failures: u64,
    pub encoder_failures: u64,
    pub send_failures: u64,
    pub last_frame_index: Option<u32>,
    pub last_frame_age_micros: Option<u64>,
    pub last_frame_received_to_send_micros: Option<u64>,
    pub max_frame_received_to_send_micros: Option<u64>,
}

#[derive(Debug)]
pub enum PipelineError {
    Capture(CaptureError),
    Encoder(EncoderError),
    Sender(MoonlightSendError),
    MissingCaptureTimestamp,
    ConfigurationMismatch,
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capture(error) => write!(f, "capture failed: {error}"),
            Self::Encoder(error) => write!(f, "encoding failed: {error}"),
            Self::Sender(error) => write!(f, "video send failed: {error}"),
            Self::MissingCaptureTimestamp => f.write_str("encoder output has no capture timestamp"),
            Self::ConfigurationMismatch => f.write_str("capture and encoder dimensions do not match"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// Synchronous producer path. Keep `pump_once` on a dedicated media thread.
pub struct SoftwareVideoPipeline<S: FrameSource> {
    source: S,
    encoder: FfmpegH264Encoder,
    sender: MoonlightUdpSender,
    stream_epoch: Instant,
    submitted_frames: u64,
    next_frame_index: u32,
    health: PipelineHealth,
    latest_frame_received_at: Option<Instant>,
}

impl<S: FrameSource> SoftwareVideoPipeline<S> {
    /// Build the H.264 media path from a completed authenticated and negotiated session.
    /// The caller must derive ciphers from that session's validated launch key and
    /// reserve the RTP/RTCP pair before advertising it in RTSP SETUP.
    pub fn from_negotiated_session(
        source: S,
        profile: &MoonlightStreamProfile,
        ciphers: NegotiatedMoonlightCiphers,
        server_bind_ip: IpAddr,
        peer_ip: IpAddr,
        client_video_ports: MediaPortPair,
        multi_fec_supported: bool,
    ) -> Result<(Self, MediaPortPair), Box<dyn std::error::Error + Send + Sync>> {
        if profile.codec != VideoCodec::H264 {
            return Err(Box::new(crate::moonlight_video::MoonlightPacketError::UnsupportedCodec));
        }
        let reserved_ports = ReservedUdpPortPair::bind(server_bind_ip)?;
        let video_config = MoonlightVideoConfig {
            shard_size: profile.packet_size as usize,
            ..MoonlightVideoConfig::default()
        };
        // Validate negotiated encryption, FEC, and port ownership before spawning FFmpeg.
        let (sender, advertised_ports) = MoonlightUdpSender::bind_for_session(
            reserved_ports,
            peer_ip,
            client_video_ports,
            video_config,
            profile,
            multi_fec_supported,
            ciphers.video,
        )?;
        let encoder = FfmpegH264Encoder::spawn(EncoderConfig {
            width: profile.width,
            height: profile.height,
            frames_per_second: profile.frames_per_second,
            bitrate_kbps: profile.bitrate_kbps,
        })?;
        let pipeline = Self::new(
            source, encoder, sender,
            profile.width, profile.height, profile.width, profile.height,
        )?;
        Ok((pipeline, advertised_ports))
    }

    pub fn new(
        source: S,
        encoder: FfmpegH264Encoder,
        sender: MoonlightUdpSender,
        capture_width: u32,
        capture_height: u32,
        encoder_width: u32,
        encoder_height: u32,
    ) -> Result<Self, PipelineError> {
        if capture_width != encoder_width || capture_height != encoder_height {
            return Err(PipelineError::ConfigurationMismatch);
        }
        Ok(Self {
            source, encoder, sender, stream_epoch: Instant::now(),
            submitted_frames: 0, next_frame_index: 0,
            health: PipelineHealth::default(), latest_frame_received_at: None,
        })
    }

    /// Captures and submits one frame, then sends an access unit if FFmpeg has completed
    /// one. AUD framing means the first output commonly becomes available after frame 2.
    pub fn pump_once(&mut self) -> Result<PipelineStep, PipelineError> {
        let frame = match self.source.next_frame() {
            Ok(frame) => frame,
            Err(error) => {
                self.health.capture_failures = self.health.capture_failures.saturating_add(1);
                return Err(PipelineError::Capture(error));
            }
        };
        self.health.captured_frames = self.health.captured_frames.saturating_add(1);
        self.latest_frame_received_at = Some(frame.received_at);
        if let Err(error) = self.encoder.submit(&frame) {
            self.health.encoder_failures = self.health.encoder_failures.saturating_add(1);
            return Err(PipelineError::Encoder(error));
        }
        self.submitted_frames = self.submitted_frames.saturating_add(1);
        if self.submitted_frames < 2 { return Ok(PipelineStep::Buffered); }
        match self.encoder.receive_timeout(Duration::from_millis(250)) {
            Ok(Some(unit)) => self.send(unit),
            Ok(None) => Ok(PipelineStep::Buffered),
            Err(error) => {
                self.health.encoder_failures = self.health.encoder_failures.saturating_add(1);
                Err(PipelineError::Encoder(error))
            }
        }
    }

    /// Return a measured snapshot. Latest frame age is recalculated when this is called.
    pub fn health(&self) -> PipelineHealth {
        let mut snapshot = self.health;
        snapshot.last_frame_age_micros = self.latest_frame_received_at.map(|frame_received_at| {
            duration_micros(Instant::now().saturating_duration_since(frame_received_at))
        });
        snapshot
    }

    /// Closes encoder input, sends any final buffered access units, and returns send stats.
    pub fn finish(self) -> Result<Vec<(u32, MoonlightSendStats)>, PipelineError> {
        let SoftwareVideoPipeline { encoder, sender, stream_epoch, next_frame_index, .. } = self;
        let mut sender = sender;
        let mut next_frame_index = next_frame_index;
        let mut sent = Vec::new();
        for unit in encoder.finish().map_err(PipelineError::Encoder)? {
            let frame_index = next_frame_index;
            let stats = send_unit(&mut sender, stream_epoch, frame_index, unit)?;
            sent.push((frame_index, stats));
            next_frame_index = next_frame_index.wrapping_add(1);
        }
        Ok(sent)
    }

    fn send(&mut self, unit: EncodedAccessUnit) -> Result<PipelineStep, PipelineError> {
        let frame_received_at = unit.received_at.ok_or(PipelineError::MissingCaptureTimestamp)?;
        let frame_index = self.next_frame_index;
        let stats = match send_unit(&mut self.sender, self.stream_epoch, frame_index, unit) {
            Ok(stats) => stats,
            Err(PipelineError::Sender(error)) => {
                self.health.send_failures = self.health.send_failures.saturating_add(1);
                return Err(PipelineError::Sender(error));
            }
            Err(error) => return Err(error),
        };
        let age = duration_micros(Instant::now().saturating_duration_since(frame_received_at));
        self.health.sent_frames = self.health.sent_frames.saturating_add(1);
        self.health.packets_sent = self.health.packets_sent.saturating_add(stats.packets as u64);
        self.health.bytes_sent = self.health.bytes_sent.saturating_add(stats.bytes as u64);
        self.health.last_frame_index = Some(frame_index);
        self.health.last_frame_received_to_send_micros = Some(age);
        self.health.max_frame_received_to_send_micros = Some(
            self.health.max_frame_received_to_send_micros.unwrap_or(0).max(age)
        );
        self.next_frame_index = self.next_frame_index.wrapping_add(1);
        Ok(PipelineStep::Sent { frame_index, stats })
    }
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn send_unit(
    sender: &mut MoonlightUdpSender,
    stream_epoch: Instant,
    frame_index: u32,
    unit: EncodedAccessUnit,
) -> Result<MoonlightSendStats, PipelineError> {
    let frame_received_at = unit.received_at.ok_or(PipelineError::MissingCaptureTimestamp)?;
    let elapsed = frame_received_at.saturating_duration_since(stream_epoch);
    let timestamp_90khz = (elapsed.as_nanos().saturating_mul(90_000) / 1_000_000_000) as u32;
    let processing_latency = Instant::now().saturating_duration_since(frame_received_at);
    let latency_tenths_ms = (processing_latency.as_micros() / 100).min(u16::MAX as u128) as u16;
    let is_idr = unit.nals.iter().any(|nal| nal.first().is_some_and(|header| *header & 0x1f == 5));
    let nals: Vec<&[u8]> = unit.nals.iter().map(Vec::as_slice).collect();
    sender.send_access_unit(frame_index, timestamp_90khz, is_idr, latency_tenths_ms, &nals)
        .map_err(PipelineError::Sender)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, net::UdpSocket};

    use crate::{
        capture::CapturedFrame,
        encoder::EncoderConfig,
        moonlight_video::MoonlightVideoConfig,
        moonlight_sdp::{MoonlightStreamProfile, VideoCodec},
        moonlight_session::MediaPortPair,
        moonlight_launch::MoonlightLaunchCrypto,
    };

    use super::*;

    struct Frames(VecDeque<CapturedFrame>);

    impl FrameSource for Frames {
        fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
            self.0.pop_front().ok_or(CaptureError::ProcessExited(Some(0)))
        }
    }

    #[test]
    fn builds_and_sends_a_negotiated_encrypted_h264_session() {
        use std::net::{IpAddr, Ipv4Addr};

        let profile = MoonlightStreamProfile {
            width: 16, height: 16, frames_per_second: 30, bitrate_kbps: 500,
            packet_size: 1200, codec: VideoCodec::H264, minimum_fec_packets: 0,
            moonlight_feature_flags: 0, nvidia_feature_flags: 0,
            encryption_flags: crate::moonlight_sdp::SS_ENC_VIDEO,
        };
        let source = Frames(VecDeque::from([0x24, 0x8c].map(|fill| CapturedFrame {
            width: 16, height: 16, received_at: Instant::now(), bgra: vec![fill; 16 * 16 * 4],
        })));
        let (receiver, client_rtp) = loop {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            let port = socket.local_addr().unwrap().port();
            if port % 2 == 0 && port < u16::MAX { break (socket, port); }
        };
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let ciphers = MoonlightLaunchCrypto::from_rikey_hex(&"66".repeat(16)).unwrap()
            .establish_for_profile(false, &profile).unwrap();
        let (mut pipeline, server_ports) = SoftwareVideoPipeline::from_negotiated_session(
            source, &profile, ciphers,
            IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V4(Ipv4Addr::LOCALHOST),
            MediaPortPair::new(client_rtp, client_rtp + 1).unwrap(), false,
        ).expect("negotiated session should create its H.264 media path");
        assert_eq!(server_ports.rtp() % 2, 0);
        assert_eq!(server_ports.rtcp(), server_ports.rtp() + 1);
        pipeline.pump_once().unwrap();
        pipeline.pump_once().unwrap();
        pipeline.finish().unwrap();

        let mut packet = [0; 1400];
        let (bytes, _) = receiver.recv_from(&mut packet).unwrap();
        assert_eq!(bytes, 1200 + crate::crypto::MOONLIGHT_VIDEO_PREFIX_BYTES);
        assert_eq!(packet[11], b'V', "negotiated video encryption must protect every sent shard");
    }

    #[test]
    fn sends_launch_encrypted_h264_from_capture_pipeline_over_udp() {
        let config = EncoderConfig { width: 16, height: 16, frames_per_second: 30, bitrate_kbps: 500 };
        let source = Frames(VecDeque::from([0x30, 0xa0].map(|fill| CapturedFrame {
            width: config.width,
            height: config.height,
            received_at: Instant::now(),
            bgra: vec![fill; 16 * 16 * 4],
        })));
        let encoder = FfmpegH264Encoder::spawn(config).expect("CI installs FFmpeg with libx264");
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_millis(250))).unwrap();
        let launch = crate::moonlight_launch::MoonlightLaunchCrypto::from_rikey_hex(&"33".repeat(16)).unwrap();
        let video_cipher = launch.establish(false, true).unwrap().video;
        let sender = MoonlightUdpSender::bind(
            "127.0.0.1:0".parse().unwrap(),
            receiver.local_addr().unwrap(),
            MoonlightVideoConfig { encryption_required: true, ..MoonlightVideoConfig::default() },
            video_cipher,
        ).unwrap();
        let mut pipeline = SoftwareVideoPipeline::new(source, encoder, sender, 16, 16, 16, 16).unwrap();
        pipeline.pump_once().unwrap();
        pipeline.pump_once().unwrap();
        pipeline.finish().unwrap();

        let mut encrypted_packets = Vec::new();
        let mut packet = [0; 2048];
        while let Ok((bytes, _)) = receiver.recv_from(&mut packet) {
            encrypted_packets.push(packet[..bytes].to_vec());
        }
        assert!(!encrypted_packets.is_empty(), "the encoded video pipeline should emit UDP shards");
        for packet in &encrypted_packets {
            assert!(packet.len() > crate::crypto::MOONLIGHT_VIDEO_PREFIX_BYTES);
            assert_eq!(packet[11], b'V');
        }
        assert_eq!(&encrypted_packets[0][..8], &0_u64.to_le_bytes());
    }

    #[test]
    fn sends_captured_encoded_h264_over_moonlight_udp_path() {
        let config = EncoderConfig { width: 16, height: 16, frames_per_second: 30, bitrate_kbps: 500 };
        let source = Frames(VecDeque::from([0x20, 0x90].map(|fill| CapturedFrame {
            width: config.width,
            height: config.height,
            received_at: Instant::now(),
            bgra: vec![fill; 16 * 16 * 4],
        })));
        let encoder = FfmpegH264Encoder::spawn(config).expect("CI installs FFmpeg with libx264");
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let sender = MoonlightUdpSender::bind(
            "127.0.0.1:0".parse().unwrap(), receiver.local_addr().unwrap(),
            MoonlightVideoConfig::default(), None,
        ).unwrap();
        let mut pipeline = SoftwareVideoPipeline::new(source, encoder, sender, 16, 16, 16, 16).unwrap();
        assert_eq!(pipeline.pump_once().unwrap(), PipelineStep::Buffered);
        let second = pipeline.pump_once().unwrap();
        let sent_before_finish = usize::from(matches!(second, PipelineStep::Sent { .. }));
        let health = pipeline.health();
        assert_eq!(health.captured_frames, 2);
        assert_eq!(health.sent_frames, sent_before_finish as u64);
        assert!(health.last_frame_age_micros.is_some());
        assert_eq!(health.send_failures, 0);
        let flushed = pipeline.finish().unwrap();
        assert_eq!(sent_before_finish + flushed.len(), 2);

        let mut packet = [0; 1400];
        let (bytes, _) = receiver.recv_from(&mut packet).unwrap();
        assert_eq!(bytes, 1200);
        assert_eq!(&packet[..2], &[0x90, 96]);
        let (bytes, _) = receiver.recv_from(&mut packet).unwrap();
        assert_eq!(bytes, 1200);
        assert_eq!(&packet[..2], &[0x90, 96]);
    }
}
