//! Experimental Rust media-plane building blocks.
//!
//! Experimental capture, software encoding, RTSP session transport, Moonlight video
//! framing/FEC/encryption, and standard-RTP building blocks. These components are not
//! yet an authenticated, interoperable Moonlight host. The supervisor control socket is
//! not used for media.

use std::{
    io,
    net::{SocketAddr, UdpSocket},
};

pub mod rtsp;
pub mod crypto;
pub mod fec;
pub mod capture;
pub mod moonlight_session;
pub mod encoder;
pub mod moonlight_video;
pub mod pipeline;
pub mod moonlight_sdp;
pub mod rtsp_connection;
pub mod moonlight_pairing_key;
pub mod moonlight_pairing_cipher;
pub mod moonlight_pairing;
pub mod moonlight_rtsp_crypto;

const RTP_HEADER_LEN: usize = 12;
const H264_FU_A_TYPE: u8 = 28;
const MAX_UDP_DATAGRAM_SIZE: usize = 65_507;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpConfig {
    /// Dynamic RTP payload type, in the 0..=127 range.
    pub payload_type: u8,
    pub ssrc: u32,
    pub initial_sequence: u16,
    /// Maximum complete RTP packet size, including the 12-byte RTP header.
    pub max_packet_size: usize,
}

impl Default for RtpConfig {
    fn default() -> Self {
        Self {
            payload_type: 96,
            ssrc: 0,
            initial_sequence: 0,
            max_packet_size: 1200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketizeError {
    InvalidPayloadType(u8),
    PacketSizeTooSmall(usize),
    PacketSizeTooLarge(usize),
    EmptyAccessUnit,
    EmptyNalUnit,
    ForbiddenZeroBitSet,
    UnsupportedNalType(u8),
}

impl std::fmt::Display for PacketizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPayloadType(value) => write!(formatter, "RTP payload type {value} exceeds 127"),
            Self::PacketSizeTooSmall(value) => write!(formatter, "RTP packet size {value} cannot hold an RTP header and H.264 payload"),
            Self::PacketSizeTooLarge(value) => write!(formatter, "RTP packet size {value} exceeds the supported UDP datagram limit"),
            Self::EmptyAccessUnit => formatter.write_str("H.264 access unit contains no NAL units"),
            Self::EmptyNalUnit => formatter.write_str("H.264 NAL unit is empty"),
            Self::ForbiddenZeroBitSet => formatter.write_str("H.264 NAL unit has its forbidden_zero_bit set"),
            Self::UnsupportedNalType(value) => write!(formatter, "H.264 NAL unit type {value} cannot be sent as a single NAL or FU-A"),
        }
    }
}

impl std::error::Error for PacketizeError {}

/// Packetizes one H.264 access unit. Input NAL units exclude Annex-B start codes.
/// The caller supplies the RTP sampling timestamp (normally a 90 kHz clock).
#[derive(Debug)]
pub struct H264RtpPacketizer {
    config: RtpConfig,
    next_sequence: u16,
}

impl H264RtpPacketizer {
    pub fn new(config: RtpConfig) -> Result<Self, PacketizeError> {
        if config.payload_type > 127 {
            return Err(PacketizeError::InvalidPayloadType(config.payload_type));
        }
        if config.max_packet_size < RTP_HEADER_LEN + 3 {
            return Err(PacketizeError::PacketSizeTooSmall(config.max_packet_size));
        }
        if config.max_packet_size > MAX_UDP_DATAGRAM_SIZE {
            return Err(PacketizeError::PacketSizeTooLarge(config.max_packet_size));
        }
        Ok(Self {
            next_sequence: config.initial_sequence,
            config,
        })
    }

    pub fn packetize(&mut self, timestamp: u32, nals: &[&[u8]]) -> Result<Vec<Vec<u8>>, PacketizeError> {
        if nals.is_empty() {
            return Err(PacketizeError::EmptyAccessUnit);
        }
        let mut packets = Vec::new();
        let last_nal = nals.len() - 1;
        for (index, nal) in nals.iter().enumerate() {
            if nal.is_empty() {
                return Err(PacketizeError::EmptyNalUnit);
            }
            if nal[0] & 0x80 != 0 {
                return Err(PacketizeError::ForbiddenZeroBitSet);
            }
            let nal_type = nal[0] & 0x1f;
            if nal_type == 0 || nal_type >= 24 {
                return Err(PacketizeError::UnsupportedNalType(nal_type));
            }
            let is_last_nal = index == last_nal;
            if nal.len() <= self.max_payload_size() {
                packets.push(self.packet(timestamp, is_last_nal, nal));
            } else {
                self.fragment_nal(timestamp, nal, is_last_nal, &mut packets);
            }
        }
        Ok(packets)
    }

    fn max_payload_size(&self) -> usize {
        self.config.max_packet_size - RTP_HEADER_LEN
    }

    fn packet(&mut self, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        packet.push(0x80); // RTP v2, no padding, extension, or CSRC list.
        packet.push((u8::from(marker) << 7) | self.config.payload_type);
        packet.extend_from_slice(&self.next_sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.config.ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        self.next_sequence = self.next_sequence.wrapping_add(1);
        packet
    }

    fn fragment_nal(&mut self, timestamp: u32, nal: &[u8], marker: bool, packets: &mut Vec<Vec<u8>>) {
        let indicator = (nal[0] & 0xe0) | H264_FU_A_TYPE;
        let nal_type = nal[0] & 0x1f;
        let fragment_capacity = self.max_payload_size() - 2;
        let fragment_count = (nal.len() - 1).div_ceil(fragment_capacity);
        let mut remaining = &nal[1..];
        for index in 0..fragment_count {
            let length = remaining.len().min(fragment_capacity);
            let (fragment, rest) = remaining.split_at(length);
            remaining = rest;
            let start = index == 0;
            let end = index + 1 == fragment_count;
            let fu_header = (u8::from(start) << 7) | (u8::from(end) << 6) | nal_type;
            let mut payload = Vec::with_capacity(fragment.len() + 2);
            payload.extend_from_slice(&[indicator, fu_header]);
            payload.extend_from_slice(fragment);
            packets.push(self.packet(timestamp, marker && end, &payload));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendStats {
    pub packets: usize,
    pub bytes: usize,
}

/// Simple synchronous UDP output for an experimental RTP destination.
/// Pacing, retransmission, FEC, encryption, and congestion adaptation are not
/// provided; callers must not treat successful socket writes as client delivery.
pub struct UdpRtpSender {
    socket: UdpSocket,
    destination: SocketAddr,
    packetizer: H264RtpPacketizer,
}

impl UdpRtpSender {
    pub fn bind(
        local: SocketAddr,
        destination: SocketAddr,
        config: RtpConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let socket = UdpSocket::bind(local)?;
        Ok(Self {
            socket,
            destination,
            packetizer: H264RtpPacketizer::new(config)?,
        })
    }

    pub fn send_access_unit(&mut self, timestamp: u32, nals: &[&[u8]]) -> Result<SendStats, SendError> {
        let packets = self.packetizer.packetize(timestamp, nals).map_err(SendError::Packetize)?;
        let mut stats = SendStats::default();
        for packet in packets {
            let sent = self.socket.send_to(&packet, self.destination).map_err(SendError::Io)?;
            if sent != packet.len() {
                return Err(SendError::Io(io::Error::new(io::ErrorKind::WriteZero, "partial UDP datagram send")));
            }
            stats.packets += 1;
            stats.bytes += sent;
        }
        Ok(stats)
    }
}

#[derive(Debug)]
pub enum SendError {
    Packetize(PacketizeError),
    Io(io::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Packetize(error) => write!(formatter, "cannot packetize access unit: {error}"),
            Self::Io(error) => write!(formatter, "cannot send RTP datagram: {error}"),
        }
    }
}

impl std::error::Error for SendError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_packet_size: usize) -> RtpConfig {
        RtpConfig { max_packet_size, ssrc: 0x1234_5678, initial_sequence: u16::MAX, ..RtpConfig::default() }
    }

    #[test]
    fn packetizes_single_nal_and_marks_access_unit_end() {
        let nal = [0x65, 1, 2, 3];
        let mut packetizer = H264RtpPacketizer::new(config(64)).unwrap();
        let packets = packetizer.packetize(0x0102_0304, &[&nal]).unwrap();
        assert_eq!(packets.len(), 1);
        let packet = &packets[0];
        assert_eq!(&packet[..2], &[0x80, 0xe0]); // marker and dynamic payload type 96.
        assert_eq!(&packet[2..4], &u16::MAX.to_be_bytes());
        assert_eq!(&packet[4..8], &0x0102_0304_u32.to_be_bytes());
        assert_eq!(&packet[8..12], &0x1234_5678_u32.to_be_bytes());
        assert_eq!(&packet[12..], &nal);
    }

    #[test]
    fn packetizes_multiple_nals_and_wraps_sequence_numbers() {
        let first = [0x67, 0xaa];
        let second = [0x68, 0xbb];
        let mut packetizer = H264RtpPacketizer::new(config(64)).unwrap();
        let packets = packetizer.packetize(7, &[&first, &second]).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0][1] & 0x80, 0);
        assert_eq!(packets[1][1] & 0x80, 0x80);
        assert_eq!(&packets[0][2..4], &u16::MAX.to_be_bytes());
        assert_eq!(&packets[1][2..4], &0_u16.to_be_bytes());
        assert_eq!(&packets[0][4..8], &packets[1][4..8]);
    }

    #[test]
    fn fragments_large_nal_as_fu_a_and_preserves_reassembly_bytes() {
        let nal: Vec<u8> = std::iter::once(0x65).chain(0..40).collect();
        let mut packetizer = H264RtpPacketizer::new(config(24)).unwrap(); // 12 RTP + 10 payload.
        let packets = packetizer.packetize(99, &[&nal]).unwrap();
        assert!(packets.len() > 1);
        let mut reassembled = vec![packets[0][12] & 0xe0 | (packets[0][13] & 0x1f)];
        for (index, packet) in packets.iter().enumerate() {
            assert!(packet.len() <= 24);
            assert_eq!(packet[12] & 0x1f, H264_FU_A_TYPE);
            assert_eq!(packet[13] & 0x80 != 0, index == 0);
            assert_eq!(packet[13] & 0x40 != 0, index + 1 == packets.len());
            assert_eq!(packet[1] & 0x80 != 0, index + 1 == packets.len());
            assert_eq!(&packet[4..8], &99_u32.to_be_bytes());
            reassembled.extend_from_slice(&packet[14..]);
        }
        assert_eq!(reassembled, nal);
    }

    #[test]
    fn rejects_empty_access_units_nals_and_invalid_nal_types() {
        let mut packetizer = H264RtpPacketizer::new(config(64)).unwrap();
        assert_eq!(packetizer.packetize(0, &[]), Err(PacketizeError::EmptyAccessUnit));
        assert_eq!(packetizer.packetize(0, &[&[]]), Err(PacketizeError::EmptyNalUnit));
        assert_eq!(packetizer.packetize(0, &[&[0]]), Err(PacketizeError::UnsupportedNalType(0)));
        assert_eq!(packetizer.packetize(0, &[&[24]]), Err(PacketizeError::UnsupportedNalType(24)));
        assert_eq!(packetizer.packetize(0, &[&[0xe5]]), Err(PacketizeError::ForbiddenZeroBitSet));
        assert_eq!(H264RtpPacketizer::new(RtpConfig { payload_type: 128, ..RtpConfig::default() }).unwrap_err(), PacketizeError::InvalidPayloadType(128));
        assert_eq!(H264RtpPacketizer::new(config(14)).unwrap_err(), PacketizeError::PacketSizeTooSmall(14));
        assert_eq!(H264RtpPacketizer::new(config(MAX_UDP_DATAGRAM_SIZE + 1)).unwrap_err(), PacketizeError::PacketSizeTooLarge(MAX_UDP_DATAGRAM_SIZE + 1));
    }

    #[test]
    fn udp_sender_transmits_rtp_packets_to_local_receiver() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        let destination = receiver.local_addr().unwrap();
        let mut sender = UdpRtpSender::bind(
            "127.0.0.1:0".parse().unwrap(), destination, config(1200),
        ).unwrap();
        let nal = [0x65, 1, 2, 3];
        let stats = sender.send_access_unit(9000, &[&nal]).unwrap();
        let mut packet = [0; 64];
        let (received, _) = receiver.recv_from(&mut packet).unwrap();
        assert_eq!(stats, SendStats { packets: 1, bytes: received });
        assert_eq!(&packet[..received][12..], &nal);
        assert_eq!(&packet[4..8], &9000_u32.to_be_bytes());
    }
}
