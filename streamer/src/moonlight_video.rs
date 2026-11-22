//! Moonlight H.264 video datagram layout with bounded Reed-Solomon FEC blocks.
//!
//! The packet fields mirror the packed RTP + NV_VIDEO_PACKET layout used by the
//! Syzygy/Moonlight compatibility path. Frames larger than one 255-data-shard FEC block
//! are split across the protocol's four-block limit. Parity is generated over complete
//! plaintext data shards, then each shard receives final sequence/FEC metadata and is
//! optionally encrypted. Differential vectors and a real-client pass are still required
//! before claiming Moonlight interoperability.

use std::{io, net::{IpAddr, SocketAddr, UdpSocket}};

use crate::crypto::{CryptoError, MoonlightVideoCipher, MOONLIGHT_VIDEO_PREFIX_BYTES};
use crate::fec::{FecBlock, FecError};
use crate::moonlight_sdp::{MoonlightStreamProfile, VideoCodec};
use crate::moonlight_session::MediaPortPair;

const RTP_HEADER_BYTES: usize = 12;
const MOONLIGHT_PACKET_HEADER_BYTES: usize = 32;
const FRAME_HEADER_BYTES: usize = 8;
const MAX_DATA_SHARDS_PER_BLOCK: usize = 255;
const MAX_FEC_BLOCKS: usize = 4;
const MAX_DATAGRAM_BYTES: usize = 65_507;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoonlightPacketError {
    PacketSizeOutOfRange(usize),
    TooManyBlocks(usize),
    MultiBlockUnsupported,
    InvalidFecPercentage(u8),
    FecBlockTooLarge,
    UnsupportedCodec,
    EncryptionRequired,
    EmptyAccessUnit,
    EmptyNalUnit,
    InvalidNalHeader,
    EncryptedDatagramTooLarge,
    Crypto(CryptoError),
}

impl std::fmt::Display for MoonlightPacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PacketSizeOutOfRange(value) => write!(f, "Moonlight shard size {value} is outside the supported range"),
            Self::TooManyBlocks(value) => write!(f, "Moonlight frame requires {value} FEC blocks; maximum is {MAX_FEC_BLOCKS}"),
            Self::MultiBlockUnsupported => f.write_str("the negotiated Moonlight client does not support multi-block video frames"),
            Self::InvalidFecPercentage(value) => write!(f, "Moonlight FEC percentage {value} exceeds 100"),
            Self::FecBlockTooLarge => f.write_str("Moonlight FEC parameters cannot fit within the 255-shard block limit"),
            Self::UnsupportedCodec => f.write_str("Moonlight video packetizer currently supports H.264 only"),
            Self::EncryptionRequired => f.write_str("the negotiated Moonlight session requires an established video cipher"),
            Self::EmptyAccessUnit => f.write_str("H.264 access unit contains no NAL units"),
            Self::EmptyNalUnit => f.write_str("H.264 NAL unit is empty"),
            Self::InvalidNalHeader => f.write_str("H.264 NAL unit has an invalid forbidden_zero_bit or type"),
            Self::EncryptedDatagramTooLarge => f.write_str("encrypted Moonlight datagram exceeds the UDP size limit"),
            Self::Crypto(error) => write!(f, "cannot encrypt Moonlight video shard: {error}"),
        }
    }
}

impl std::error::Error for MoonlightPacketError {}

impl From<CryptoError> for MoonlightPacketError {
    fn from(error: CryptoError) -> Self { Self::Crypto(error) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoonlightVideoConfig {
    /// Size of the plaintext media datagram, including RTP and NV_VIDEO_PACKET headers.
    pub shard_size: usize,
    pub initial_sequence: u16,
    pub ssrc: u32,
    /// Set only when the negotiated client advertises multi-FEC block support.
    pub multi_fec_supported: bool,
    /// Requested parity percentage from the authenticated client profile.
    pub fec_percentage: u8,
    /// Minimum parity shards per block from the client profile.
    pub minimum_fec_packets: u8,
    /// Set from validated session negotiation; plaintext sending is refused when true.
    pub encryption_required: bool,
}

impl Default for MoonlightVideoConfig {
    fn default() -> Self { Self { shard_size: 1200, initial_sequence: 0, ssrc: 0, multi_fec_supported: false, fec_percentage: 0, minimum_fec_packets: 0, encryption_required: false } }
}

impl MoonlightVideoConfig {
    /// Apply the validated RTSP profile after the caller has chosen a packet size and
    /// confirmed multi-FEC support from the negotiated client capabilities.
    pub fn apply_session_profile(mut self, profile: &MoonlightStreamProfile, multi_fec_supported: bool) -> Result<Self, MoonlightPacketError> {
        if profile.codec != VideoCodec::H264 { return Err(MoonlightPacketError::UnsupportedCodec); }
        self.minimum_fec_packets = u8::try_from(profile.minimum_fec_packets).map_err(|_| MoonlightPacketError::FecBlockTooLarge)?;
        self.encryption_required = profile.encryption_flags != 0;
        self.multi_fec_supported = multi_fec_supported;
        Ok(self)
    }
}

/// Packetizes one encoded H.264 access unit into Moonlight video datagrams.
///
/// It accepts an optional already-negotiated per-session cipher; passing no cipher emits plaintext.
/// Do not enable the plaintext mode on a real authenticated Moonlight network session.
pub struct MoonlightVideoPacketizer {
    config: MoonlightVideoConfig,
    next_sequence: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MoonlightSendStats {
    pub packets: usize,
    pub bytes: usize,
}

/// Holds an even RTP port and its consecutive RTCP port so RTSP can advertise ports
/// that are already reserved by the OS.
pub struct ReservedUdpPortPair {
    ports: MediaPortPair,
    rtp: UdpSocket,
    rtcp: UdpSocket,
}

impl ReservedUdpPortPair {
    pub fn bind(local_ip: IpAddr) -> io::Result<Self> {
        let mut last_bind_error = None;
        for _ in 0..128 {
            let rtp = UdpSocket::bind(SocketAddr::new(local_ip, 0))?;
            let rtp_port = rtp.local_addr()?.port();
            let Some(ports) = rtp_port.checked_add(1).and_then(|rtcp| MediaPortPair::new(rtp_port, rtcp)) else { continue; };
            match UdpSocket::bind(SocketAddr::new(local_ip, ports.rtcp())) {
                Ok(rtcp) => return Ok(Self { ports, rtp, rtcp }),
                Err(error) => last_bind_error = Some(error),
            }
        }
        Err(last_bind_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "could not reserve an even RTP/RTCP port pair")))
    }

    pub fn ports(&self) -> MediaPortPair { self.ports }

    fn into_sockets(self) -> (MediaPortPair, UdpSocket, UdpSocket) { (self.ports, self.rtp, self.rtcp) }
}

#[derive(Debug)]
pub enum MoonlightSendError {
    Packetize(MoonlightPacketError),
    Io(io::Error),
    PartialDatagram { expected: usize, sent: usize },
}

impl std::fmt::Display for MoonlightSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Packetize(error) => write!(f, "cannot packetize Moonlight video frame: {error}"),
            Self::Io(error) => write!(f, "cannot send Moonlight video datagram: {error}"),
            Self::PartialDatagram { expected, sent } => write!(f, "UDP send accepted {sent} of {expected} datagram bytes"),
        }
    }
}

impl std::error::Error for MoonlightSendError {}

/// UDP delivery for already-negotiated video peer details. RTSP/UDP port negotiation
/// remains the caller's responsibility; no media is sent over control IPC.
pub struct MoonlightUdpSender {
    socket: UdpSocket,
    _rtcp_socket: Option<UdpSocket>,
    destination: SocketAddr,
    packetizer: MoonlightVideoPacketizer,
    cipher: Option<MoonlightVideoCipher>,
}

impl MoonlightUdpSender {
    pub fn bind(
        local: SocketAddr,
        destination: SocketAddr,
        config: MoonlightVideoConfig,
        cipher: Option<MoonlightVideoCipher>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self { socket: UdpSocket::bind(local)?, _rtcp_socket: None, destination, packetizer: MoonlightVideoPacketizer::new(config)?, cipher })
    }

    /// Build a sender using the exact reserved RTP port that the RTSP layer can
    /// advertise. The paired RTCP socket remains owned for the sender's lifetime.
    pub fn bind_reserved(
        pair: ReservedUdpPortPair,
        destination: SocketAddr,
        config: MoonlightVideoConfig,
        cipher: Option<MoonlightVideoCipher>,
    ) -> Result<(Self, MediaPortPair), Box<dyn std::error::Error + Send + Sync>> {
        let (ports, socket, rtcp_socket) = pair.into_sockets();
        let sender = Self { socket, _rtcp_socket: Some(rtcp_socket), destination, packetizer: MoonlightVideoPacketizer::new(config)?, cipher };
        Ok((sender, ports))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> { self.socket.local_addr() }

    pub fn send_access_unit(
        &mut self,
        frame_index: u32,
        timestamp_90khz: u32,
        idr: bool,
        processing_latency_tenths_ms: u16,
        nals: &[&[u8]],
    ) -> Result<MoonlightSendStats, MoonlightSendError> {
        let packets = self.packetizer.packetize(
            frame_index, timestamp_90khz, idr, processing_latency_tenths_ms, nals, self.cipher.as_mut(),
        ).map_err(MoonlightSendError::Packetize)?;
        let mut stats = MoonlightSendStats::default();
        for packet in packets {
            let sent = self.socket.send_to(&packet, self.destination).map_err(MoonlightSendError::Io)?;
            if sent != packet.len() { return Err(MoonlightSendError::PartialDatagram { expected: packet.len(), sent }); }
            stats.packets += 1;
            stats.bytes += sent;
        }
        Ok(stats)
    }
}

impl MoonlightVideoPacketizer {
    pub fn new(config: MoonlightVideoConfig) -> Result<Self, MoonlightPacketError> {
        if !(MOONLIGHT_PACKET_HEADER_BYTES + 1..=MAX_DATAGRAM_BYTES).contains(&config.shard_size) {
            return Err(MoonlightPacketError::PacketSizeOutOfRange(config.shard_size));
        }
        if config.fec_percentage > 100 { return Err(MoonlightPacketError::InvalidFecPercentage(config.fec_percentage)); }
        Ok(Self { next_sequence: config.initial_sequence, config })
    }

    pub fn packetize(
        &mut self,
        frame_index: u32,
        timestamp_90khz: u32,
        idr: bool,
        processing_latency_tenths_ms: u16,
        nals: &[&[u8]],
        mut cipher: Option<&mut MoonlightVideoCipher>,
    ) -> Result<Vec<Vec<u8>>, MoonlightPacketError> {
        if nals.is_empty() { return Err(MoonlightPacketError::EmptyAccessUnit); }
        if cipher.is_some() && self.config.shard_size + MOONLIGHT_VIDEO_PREFIX_BYTES > MAX_DATAGRAM_BYTES {
            return Err(MoonlightPacketError::EncryptedDatagramTooLarge);
        }
        if self.config.encryption_required && cipher.is_none() { return Err(MoonlightPacketError::EncryptionRequired); }
        let mut encoded = Vec::new();
        for nal in nals {
            if nal.is_empty() { return Err(MoonlightPacketError::EmptyNalUnit); }
            let nal_type = nal[0] & 0x1f;
            if nal[0] & 0x80 != 0 || nal_type == 0 || nal_type >= 24 { return Err(MoonlightPacketError::InvalidNalHeader); }
            encoded.extend_from_slice(&[0, 0, 0, 1]);
            encoded.extend_from_slice(nal);
        }

        let shard_payload_size = self.config.shard_size - MOONLIGHT_PACKET_HEADER_BYTES;
        let frame_data_size = FRAME_HEADER_BYTES.checked_add(encoded.len()).ok_or(MoonlightPacketError::PacketSizeOutOfRange(self.config.shard_size))?;
        let total_data_shards = frame_data_size.div_ceil(shard_payload_size);
        let max_data_shards = max_data_shards_per_block(self.config.fec_percentage, self.config.minimum_fec_packets as usize)
            .ok_or(MoonlightPacketError::FecBlockTooLarge)?;
        let block_count = total_data_shards.div_ceil(max_data_shards);
        if block_count > MAX_FEC_BLOCKS { return Err(MoonlightPacketError::TooManyBlocks(block_count)); }
        if block_count > 1 && !self.config.multi_fec_supported { return Err(MoonlightPacketError::MultiBlockUnsupported); }
        let last_payload_len = frame_data_size % shard_payload_size;
        let last_payload_len = if last_payload_len == 0 { shard_payload_size } else { last_payload_len };

        let mut frame_data = Vec::with_capacity(frame_data_size);
        frame_data.push(0x01); // short frame header type
        frame_data.extend_from_slice(&processing_latency_tenths_ms.to_le_bytes());
        frame_data.push(if idr { 2 } else { 1 });
        frame_data.extend_from_slice(&(last_payload_len as u16).to_le_bytes());
        frame_data.extend_from_slice(&[0, 0]);
        frame_data.extend_from_slice(&encoded);

        let last_block_index = if self.config.multi_fec_supported { (block_count - 1) as u8 } else { 0 };
        let mut packets = Vec::with_capacity(total_data_shards + block_count * self.config.minimum_fec_packets as usize + total_data_shards * self.config.fec_percentage as usize / 100 + 1);
        let mut frame_offset = 0;
        let mut low_sequence = self.next_sequence;
        for block_index in 0..block_count {
            let blocks_remaining = block_count - block_index;
            let remaining_data_shards = (frame_data.len() - frame_offset).div_ceil(shard_payload_size);
            // Balance the frame payload across blocks on complete shard boundaries.
            let block_shards = remaining_data_shards.div_ceil(blocks_remaining).min(max_data_shards);
            let mut block_payload = Vec::with_capacity(block_shards * self.config.shard_size);
            for shard_index in 0..block_shards {
                let mut plaintext = vec![0; self.config.shard_size];
                let sequence = low_sequence.wrapping_add(shard_index as u16);

                // RTP fixed header and the four-byte Moonlight extension prefix.
                plaintext[0] = 0x90; // RTP v2 + extension.
                plaintext[1] = 96; // Dynamic RTP payload type used by this adapter.
                plaintext[2..4].copy_from_slice(&sequence.to_be_bytes());
                plaintext[4..8].copy_from_slice(&timestamp_90khz.to_be_bytes());
                plaintext[8..12].copy_from_slice(&self.config.ssrc.to_be_bytes());

                // NV_VIDEO_PACKET is packed and its 32-bit members are little-endian.
                let nv = RTP_HEADER_BYTES + 4;
                let stream_packet_index = (sequence as u32) << 8;
                plaintext[nv..nv + 4].copy_from_slice(&stream_packet_index.to_le_bytes());
                plaintext[nv + 4..nv + 8].copy_from_slice(&frame_index.to_le_bytes());
                let mut flags = 0x01; // FLAG_CONTAINS_PIC_DATA
                if shard_index == 0 { flags |= 0x04; } // FLAG_SOF (per FEC block)
                if shard_index + 1 == block_shards { flags |= 0x02; } // FLAG_EOF (per FEC block)
                plaintext[nv + 8] = flags;
                plaintext[nv + 9] = 0;
                plaintext[nv + 10] = 0x10; // multiFecFlags marker
                plaintext[nv + 11] = ((block_index as u8) << 4) | (last_block_index << 6);
                let length = (frame_data.len() - frame_offset).min(shard_payload_size);
                let payload_start = MOONLIGHT_PACKET_HEADER_BYTES;
                plaintext[payload_start..payload_start + length]
                    .copy_from_slice(&frame_data[frame_offset..frame_offset + length]);
                frame_offset += length;
                block_payload.extend_from_slice(&plaintext);
            }

            let fec = FecBlock::encode(&block_payload, self.config.shard_size, self.config.fec_percentage, self.config.minimum_fec_packets as usize)
                .map_err(map_fec_error)?;
            if fec.data_shards > max_data_shards { return Err(MoonlightPacketError::FecBlockTooLarge); }
            for (shard_index, mut shard) in fec.shards.into_iter().enumerate() {
                let sequence = low_sequence.wrapping_add(shard_index as u16);
                shard[0] = 0x90;
                shard[1] = 96;
                shard[2..4].copy_from_slice(&sequence.to_be_bytes());
                shard[4..8].copy_from_slice(&timestamp_90khz.to_be_bytes());
                shard[8..12].copy_from_slice(&self.config.ssrc.to_be_bytes());

                let nv = RTP_HEADER_BYTES + 4;
                shard[nv + 4..nv + 8].copy_from_slice(&frame_index.to_le_bytes());
                shard[nv + 10] = 0x10;
                shard[nv + 11] = ((block_index as u8) << 4) | (last_block_index << 6);
                let fec_info = ((shard_index as u32) << 12)
                    | ((fec.data_shards as u32) << 22)
                    | ((self.config.fec_percentage as u32) << 4);
                shard[nv + 12..nv + 16].copy_from_slice(&fec_info.to_le_bytes());

                let datagram = if let Some(video_cipher) = cipher.as_deref_mut() {
                    let encrypted = video_cipher.seal_shard(frame_index, &shard)?;
                    if encrypted.len() > MAX_DATAGRAM_BYTES { return Err(MoonlightPacketError::EncryptedDatagramTooLarge); }
                    encrypted
                } else { shard };
                packets.push(datagram);
            }
            low_sequence = low_sequence.wrapping_add((fec.data_shards + fec.parity_shards) as u16);
        }
        self.next_sequence = low_sequence;
        debug_assert_eq!(frame_offset, frame_data.len());
        Ok(packets)
    }
}

fn max_data_shards_per_block(fec_percentage: u8, minimum_parity_shards: usize) -> Option<usize> {
    (1..=MAX_DATA_SHARDS_PER_BLOCK).rev().find(|data| {
        let data = *data;
        let percentage_parity = (data * fec_percentage as usize).div_ceil(100);
        data + percentage_parity.max(minimum_parity_shards) <= MAX_DATA_SHARDS_PER_BLOCK
    })
}

fn map_fec_error(_error: FecError) -> MoonlightPacketError { MoonlightPacketError::FecBlockTooLarge }

#[cfg(test)]
mod tests {
    use super::*;

    fn nv_header(packet: &[u8]) -> &[u8] { &packet[RTP_HEADER_BYTES + 4..MOONLIGHT_PACKET_HEADER_BYTES] }

    #[test]
    fn emits_moonlight_rtp_extension_frame_metadata_and_short_header() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, initial_sequence: 9, ssrc: 0x11223344, multi_fec_supported: false, fec_percentage: 0, minimum_fec_packets: 0, encryption_required: false }).unwrap();
        let packets = packetizer.packetize(7, 0x01020304, true, 42, &[&[0x65, 0xaa, 0xbb]], None).unwrap();
        assert_eq!(packets.len(), 1);
        let packet = &packets[0];
        assert_eq!(&packet[..2], &[0x90, 96]);
        assert_eq!(&packet[2..4], &9_u16.to_be_bytes());
        assert_eq!(&packet[4..8], &0x01020304_u32.to_be_bytes());
        assert_eq!(&packet[8..12], &0x11223344_u32.to_be_bytes());
        assert_eq!(&nv_header(packet)[..8], &[0, 9, 0, 0, 7, 0, 0, 0]); // stream index and frame index, LE.
        assert_eq!(nv_header(packet)[8], 0x07); // picture data, SOF, EOF.
        assert_eq!(&nv_header(packet)[12..16], &(1_u32 << 22).to_le_bytes()); // one data shard, no FEC.
        assert_eq!(&packet[MOONLIGHT_PACKET_HEADER_BYTES..MOONLIGHT_PACKET_HEADER_BYTES + 8], &[1, 42, 0, 2, 15, 0, 0, 0]);
        let encoded_payload = &packet[MOONLIGHT_PACKET_HEADER_BYTES + 8..];
        assert_eq!(&encoded_payload[..7], &[0, 0, 0, 1, 0x65, 0xaa, 0xbb]);
        assert!(encoded_payload[7..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn splits_large_frames_into_bounded_multi_fec_blocks() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, initial_sequence: u16::MAX - 1, ssrc: 0, multi_fec_supported: true, fec_percentage: 0, minimum_fec_packets: 0, encryption_required: false }).unwrap();
        let nal = vec![0x41; 32 * 260];
        let packets = packetizer.packetize(4, 0, false, 0, &[&nal], None).unwrap();
        assert!(packets.len() > MAX_DATA_SHARDS_PER_BLOCK);
        let first = nv_header(&packets[0]);
        let second_block = nv_header(packets.iter().find(|packet| (nv_header(packet)[11] >> 4 & 0x3) == 1).unwrap());
        assert_eq!(first[11] >> 4 & 0x3, 0);
        assert_eq!(second_block[11] >> 4 & 0x3, 1);
        assert_eq!(second_block[11] >> 6 & 0x3, 1);
        assert_eq!(&packets[1][2..4], &u16::MAX.to_be_bytes());
    }

    #[test]
    fn applies_the_moonlight_video_cipher_after_framing() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, fec_percentage: 50, ..MoonlightVideoConfig::default() }).unwrap();
        let mut cipher = MoonlightVideoCipher::new(&[1; 16], 2).unwrap();
        let packets = packetizer.packetize(3, 0, true, 0, &[&[0x65, 1]], Some(&mut cipher)).unwrap();
        assert_eq!(packets.len(), 2);
        assert!(packets.iter().all(|packet| packet.len() == MOONLIGHT_VIDEO_PREFIX_BYTES + 64));
        assert!(packets.iter().all(|packet| &packet[12..16] == &3_u32.to_le_bytes()));
    }

    #[test]
    fn adds_parity_shards_and_updates_wire_metadata_before_encryption() {
        let config = MoonlightVideoConfig { shard_size: 64, fec_percentage: 25, ..MoonlightVideoConfig::default() };
        let mut packetizer = MoonlightVideoPacketizer::new(config).unwrap();
        let nal = vec![0x41; 64];
        let packets = packetizer.packetize(9, 900, false, 4, &[&nal], None).unwrap();
        assert_eq!(packets.len(), 4); // three data shards plus one parity shard
        assert!(packets.iter().all(|packet| packet[0] == 0x90 && packet[1] == 96));
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), index as u16);
            let info = u32::from_le_bytes(nv_header(packet)[12..16].try_into().unwrap());
            assert_eq!((info >> 22) & 0x3ff, 3);
            assert_eq!((info >> 4) & 0xff, 25);
            assert_eq!((info >> 12) & 0x3ff, index as u32);
        }
    }

    #[test]
    fn minimum_parity_and_percentage_both_fit_the_protocol_block_limit() {
        assert_eq!(max_data_shards_per_block(0, 1), Some(254));
        assert_eq!(max_data_shards_per_block(20, 1), Some(212));
        assert_eq!(max_data_shards_per_block(100, 255), None);
        assert!(matches!(MoonlightVideoPacketizer::new(MoonlightVideoConfig { fec_percentage: 101, ..MoonlightVideoConfig::default() }), Err(MoonlightPacketError::InvalidFecPercentage(101))));
    }

    #[test]
    fn emits_negotiated_minimum_parity_when_percentage_is_zero() {
        let config = MoonlightVideoConfig { shard_size: 64, minimum_fec_packets: 1, ..MoonlightVideoConfig::default() };
        let mut packetizer = MoonlightVideoPacketizer::new(config).unwrap();
        let packets = packetizer.packetize(2, 90, false, 0, &[&[0x41, 1]], None).unwrap();
        assert_eq!(packets.len(), 2);
        let data_info = u32::from_le_bytes(nv_header(&packets[0])[12..16].try_into().unwrap());
        let parity_info = u32::from_le_bytes(nv_header(&packets[1])[12..16].try_into().unwrap());
        assert_eq!((data_info >> 22) & 0x3ff, 1);
        assert_eq!((parity_info >> 22) & 0x3ff, 1);
        assert_eq!((parity_info >> 12) & 0x3ff, 1);
    }

    #[test]
    fn applies_negotiated_encryption_and_fec_requirements_to_the_sender() {
        let profile = MoonlightStreamProfile {
            width: 1920, height: 1080, frames_per_second: 60, bitrate_kbps: 30_000,
            packet_size: 1200, codec: VideoCodec::H264, minimum_fec_packets: 1,
            moonlight_feature_flags: 0, nvidia_feature_flags: 135, encryption_flags: 1,
        };
        let config = MoonlightVideoConfig { shard_size: 64, ..MoonlightVideoConfig::default() }
            .apply_session_profile(&profile, false).unwrap();
        assert!(config.encryption_required);
        assert_eq!(config.minimum_fec_packets, 1);

        let mut packetizer = MoonlightVideoPacketizer::new(config).unwrap();
        assert_eq!(packetizer.packetize(1, 0, false, 0, &[&[0x41, 1]], None).unwrap_err(), MoonlightPacketError::EncryptionRequired);
        let mut cipher = MoonlightVideoCipher::new(&[5; 16], 1).unwrap();
        let packets = packetizer.packetize(1, 0, false, 0, &[&[0x41, 1]], Some(&mut cipher)).unwrap();
        assert_eq!(packets.len(), 2);
        assert!(packets.iter().all(|packet| packet.len() == MOONLIGHT_VIDEO_PREFIX_BYTES + 64));
    }

    #[test]
    fn rejects_invalid_packet_size_and_frame_limits() {
        assert!(MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 32, ..MoonlightVideoConfig::default() }).is_err());
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 33, ..MoonlightVideoConfig::default() }).unwrap();
        let too_large = vec![0x41; 33 * 255 * 4];
        assert!(matches!(packetizer.packetize(0, 0, false, 0, &[&too_large], None), Err(MoonlightPacketError::TooManyBlocks(_))));
    }

    #[test]
    fn refuses_multiblock_frames_when_client_capability_is_unknown_or_absent() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, ..MoonlightVideoConfig::default() }).unwrap();
        let nal = vec![0x41; 32 * 260];
        assert_eq!(packetizer.packetize(0, 0, false, 0, &[&nal], None).unwrap_err(), MoonlightPacketError::MultiBlockUnsupported);
    }

    #[test]
    fn sends_framed_video_datagrams_to_negotiated_udp_peer() {
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        let mut sender = MoonlightUdpSender::bind(
            "127.0.0.1:0".parse().unwrap(), receiver.local_addr().unwrap(),
            MoonlightVideoConfig { shard_size: 64, ..MoonlightVideoConfig::default() }, None,
        ).unwrap();
        let stats = sender.send_access_unit(1, 900, true, 0, &[&[0x65, 1, 2]]).unwrap();
        assert_eq!(stats.packets, 1);
        let mut packet = [0; 128];
        let (bytes, _) = receiver.recv_from(&mut packet).unwrap();
        assert_eq!(bytes, 64);
        assert_eq!(&packet[..2], &[0x90, 96]);
        assert_eq!(&packet[20..24], &[1, 0, 0, 0]);
    }

    #[test]
    fn reserves_and_uses_the_exact_even_rtp_and_consecutive_rtcp_ports() {
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        let pair = ReservedUdpPortPair::bind(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)).unwrap();
        let expected_ports = pair.ports();
        assert_eq!(expected_ports.rtp() % 2, 0);
        assert_eq!(expected_ports.rtcp(), expected_ports.rtp() + 1);
        let (mut sender, advertised_ports) = MoonlightUdpSender::bind_reserved(
            pair, receiver.local_addr().unwrap(), MoonlightVideoConfig::default(), None,
        ).unwrap();
        assert_eq!(advertised_ports, expected_ports);
        assert_eq!(sender.local_addr().unwrap().port(), advertised_ports.rtp());
        assert!(UdpSocket::bind(SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), advertised_ports.rtcp())).is_err());
        sender.send_access_unit(3, 90, true, 0, &[&[0x65, 1]]).unwrap();
        let mut packet = [0; 1400];
        assert_eq!(receiver.recv_from(&mut packet).unwrap().0, 1200);
    }
}
