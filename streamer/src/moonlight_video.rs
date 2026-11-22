//! Moonlight H.264 video datagram layout, in an explicit no-parity prototype mode.
//!
//! The packet fields mirror the packed RTP + NV_VIDEO_PACKET layout used by the
//! Syzygy/Moonlight compatibility path. Frames larger than one 255-data-shard FEC block
//! are split across the protocol's four-block limit. FEC percentage is currently zero;
//! generic Reed-Solomon encoding exists in [`crate::fec`] but parity headers and recovery
//! behavior must be differentially validated before this module emits parity packets.

use crate::crypto::{CryptoError, MoonlightVideoCipher, MOONLIGHT_VIDEO_PREFIX_BYTES};

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
}

impl Default for MoonlightVideoConfig {
    fn default() -> Self { Self { shard_size: 1200, initial_sequence: 0, ssrc: 0 } }
}

/// Packetizes one encoded H.264 access unit into Moonlight video datagrams.
///
/// This first cut emits data-only frames with `fecInfo` percentage 0. It accepts an
/// optional already-negotiated per-session cipher; passing no cipher emits plaintext.
/// Do not enable the plaintext mode on a real authenticated Moonlight network session.
pub struct MoonlightVideoPacketizer {
    config: MoonlightVideoConfig,
    next_sequence: u16,
}

impl MoonlightVideoPacketizer {
    pub fn new(config: MoonlightVideoConfig) -> Result<Self, MoonlightPacketError> {
        if !(MOONLIGHT_PACKET_HEADER_BYTES + 1..=MAX_DATAGRAM_BYTES).contains(&config.shard_size) {
            return Err(MoonlightPacketError::PacketSizeOutOfRange(config.shard_size));
        }
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
        let total_shards = frame_data_size.div_ceil(shard_payload_size);
        let block_count = total_shards.div_ceil(MAX_DATA_SHARDS_PER_BLOCK);
        if block_count > MAX_FEC_BLOCKS { return Err(MoonlightPacketError::TooManyBlocks(block_count)); }
        let last_payload_len = frame_data_size % shard_payload_size;
        let last_payload_len = if last_payload_len == 0 { shard_payload_size } else { last_payload_len };

        let mut frame_data = Vec::with_capacity(frame_data_size);
        frame_data.push(0x01); // short frame header type
        frame_data.extend_from_slice(&processing_latency_tenths_ms.to_le_bytes());
        frame_data.push(if idr { 2 } else { 1 });
        frame_data.extend_from_slice(&(last_payload_len as u16).to_le_bytes());
        frame_data.extend_from_slice(&[0, 0]);
        frame_data.extend_from_slice(&encoded);

        let last_block_index = (block_count - 1) as u8;
        let mut packets = Vec::with_capacity(total_shards);
        let mut frame_offset = 0;
        let mut frame_packet_index = 0usize;
        for block_index in 0..block_count {
            let remaining = frame_data.len() - frame_offset;
            let block_shards = remaining.div_ceil(shard_payload_size).min(MAX_DATA_SHARDS_PER_BLOCK);
            for shard_index in 0..block_shards {
                let mut plaintext = vec![0; self.config.shard_size];
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.wrapping_add(1);

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
                let fec_info = ((shard_index as u32) << 12) | ((block_shards as u32) << 22);
                plaintext[nv + 12..nv + 16].copy_from_slice(&fec_info.to_le_bytes());

                let length = (frame_data.len() - frame_offset).min(shard_payload_size);
                let payload_start = MOONLIGHT_PACKET_HEADER_BYTES;
                plaintext[payload_start..payload_start + length]
                    .copy_from_slice(&frame_data[frame_offset..frame_offset + length]);
                frame_offset += length;

                let datagram = if let Some(video_cipher) = cipher.as_deref_mut() {
                    let encrypted = video_cipher.seal_shard(frame_index, &plaintext)?;
                    if encrypted.len() > MAX_DATAGRAM_BYTES { return Err(MoonlightPacketError::EncryptedDatagramTooLarge); }
                    encrypted
                } else {
                    plaintext
                };
                packets.push(datagram);
                frame_packet_index += 1;
            }
        }
        debug_assert_eq!(frame_packet_index, total_shards);
        Ok(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv_header(packet: &[u8]) -> &[u8] { &packet[RTP_HEADER_BYTES + 4..MOONLIGHT_PACKET_HEADER_BYTES] }

    #[test]
    fn emits_moonlight_rtp_extension_frame_metadata_and_short_header() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, initial_sequence: 9, ssrc: 0x11223344 }).unwrap();
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
        assert_eq!(&packet[MOONLIGHT_PACKET_HEADER_BYTES + 8..], &[0, 0, 0, 1, 0x65, 0xaa, 0xbb]);
    }

    #[test]
    fn splits_large_frames_into_bounded_multi_fec_blocks() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, initial_sequence: u16::MAX - 1, ssrc: 0 }).unwrap();
        let nal = vec![0x41; 32 * 260];
        let packets = packetizer.packetize(4, 0, false, 0, &[&nal], None).unwrap();
        assert!(packets.len() > MAX_DATA_SHARDS_PER_BLOCK);
        let first = nv_header(&packets[0]);
        let second_block = nv_header(&packets[MAX_DATA_SHARDS_PER_BLOCK]);
        assert_eq!(first[11] >> 4 & 0x3, 0);
        assert_eq!(second_block[11] >> 4 & 0x3, 1);
        assert_eq!(second_block[11] >> 6 & 0x3, 1);
        assert_eq!(&packets[1][2..4], &u16::MAX.to_be_bytes());
    }

    #[test]
    fn applies_the_moonlight_video_cipher_after_framing() {
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 64, ..MoonlightVideoConfig::default() }).unwrap();
        let mut cipher = MoonlightVideoCipher::new(&[1; 16], 2).unwrap();
        let packets = packetizer.packetize(3, 0, true, 0, &[&[0x65, 1]], Some(&mut cipher)).unwrap();
        assert_eq!(packets[0].len(), MOONLIGHT_VIDEO_PREFIX_BYTES + 64);
        assert_eq!(&packets[0][12..16], &3_u32.to_le_bytes());
    }

    #[test]
    fn rejects_invalid_packet_size_and_frame_limits() {
        assert!(MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 32, ..MoonlightVideoConfig::default() }).is_err());
        let mut packetizer = MoonlightVideoPacketizer::new(MoonlightVideoConfig { shard_size: 33, ..MoonlightVideoConfig::default() }).unwrap();
        let too_large = vec![0x41; 33 * 255 * 4];
        assert!(matches!(packetizer.packetize(0, 0, false, 0, &[&too_large], None), Err(MoonlightPacketError::TooManyBlocks(_))));
    }
}
