//! Bounded parser for the Moonlight ANNOUNCE SDP attributes used by video setup.
//!
//! This extracts requested stream parameters and negotiated feature/encryption flags. It
//! does not authenticate the RTSP peer, verify pairing certificates, or derive session
//! keys. Unknown SDP attributes are ignored for forward compatibility; recognized
//! duplicate attributes and invalid numeric ranges are rejected.

use std::collections::HashMap;

pub const MAX_SDP_BYTES: usize = 64 * 1024;
const MAX_SDP_LINES: usize = 1024;
const ML_FF_FEC_STATUS: u32 = 0x01;
const ML_FF_SESSION_ID_V1: u32 = 0x02;

const WIDTH: &str = "x-nv-video[0].clientViewportWd";
const HEIGHT: &str = "x-nv-video[0].clientViewportHt";
const FPS: &str = "x-nv-video[0].maxFPS";
const PACKET_SIZE: &str = "x-nv-video[0].packetSize";
const BITRATE: &str = "x-nv-vqos[0].bw.maximumBitrateKbps";
const CODEC: &str = "x-nv-vqos[0].bitStreamFormat";
const MIN_FEC: &str = "x-nv-vqos[0].fec.minRequiredFecPackets";
const ML_FEATURES: &str = "x-ml-general.featureFlags";
const NV_FEATURES: &str = "x-nv-general.featureFlags";
const ENCRYPTION: &str = "x-ss-general.encryptionEnabled";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec { H264, Hevc, Av1 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoonlightStreamProfile {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_kbps: u32,
    pub packet_size: u32,
    pub codec: VideoCodec,
    pub minimum_fec_packets: u32,
    pub moonlight_feature_flags: u32,
    pub nvidia_feature_flags: u32,
    pub encryption_flags: u32,
}

impl MoonlightStreamProfile {
    pub fn supports_session_id_v1(&self) -> bool { self.moonlight_feature_flags & ML_FF_SESSION_ID_V1 != 0 }
    pub fn supports_fec_status(&self) -> bool { self.moonlight_feature_flags & ML_FF_FEC_STATUS != 0 }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdpError {
    TooLarge,
    TooManyLines,
    InvalidEncoding,
    DuplicateAttribute(&'static str),
    MissingAttribute(&'static str),
    InvalidNumber(&'static str),
    ValueOutOfRange(&'static str),
    UnsupportedCodec(u32),
}

impl std::fmt::Display for SdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("Moonlight SDP exceeds the 64 KiB parser limit"),
            Self::TooManyLines => f.write_str("Moonlight SDP exceeds the parser line limit"),
            Self::InvalidEncoding => f.write_str("Moonlight SDP must be ASCII without NUL bytes"),
            Self::DuplicateAttribute(name) => write!(f, "Moonlight SDP repeats recognized attribute {name}"),
            Self::MissingAttribute(name) => write!(f, "Moonlight SDP is missing required attribute {name}"),
            Self::InvalidNumber(name) => write!(f, "Moonlight SDP attribute {name} is not an unsigned integer"),
            Self::ValueOutOfRange(name) => write!(f, "Moonlight SDP attribute {name} is outside supported bounds"),
            Self::UnsupportedCodec(value) => write!(f, "Moonlight SDP requests unsupported video format {value}"),
        }
    }
}

impl std::error::Error for SdpError {}

pub fn parse_moonlight_sdp(body: &[u8]) -> Result<MoonlightStreamProfile, SdpError> {
    if body.len() > MAX_SDP_BYTES { return Err(SdpError::TooLarge); }
    let text = std::str::from_utf8(body).map_err(|_| SdpError::InvalidEncoding)?;
    if !text.is_ascii() || text.as_bytes().contains(&0) { return Err(SdpError::InvalidEncoding); }

    let mut attributes = HashMap::new();
    let mut line_count = 0;
    for raw_line in text.split('\n') {
        line_count += 1;
        if line_count > MAX_SDP_LINES { return Err(SdpError::TooManyLines); }
        let line = raw_line.trim_end_matches('\r').trim();
        let Some(attribute) = line.strip_prefix("a=") else { continue; };
        let Some((name, value)) = attribute.split_once(':') else { continue; };
        if is_recognized(name) && attributes.insert(name, value.trim()).is_some() {
            return Err(SdpError::DuplicateAttribute(recognized_name(name)));
        }
    }

    let width = required_number(&attributes, WIDTH)?;
    let height = required_number(&attributes, HEIGHT)?;
    let frames_per_second = required_number(&attributes, FPS)?;
    let packet_size = required_number(&attributes, PACKET_SIZE)?;
    let bitrate_kbps = required_number(&attributes, BITRATE)?;
    let codec_number = required_number(&attributes, CODEC)?;
    let minimum_fec_packets = number(&attributes, MIN_FEC)?.unwrap_or(0);
    let moonlight_feature_flags = number(&attributes, ML_FEATURES)?.unwrap_or(0);
    let nvidia_feature_flags = number(&attributes, NV_FEATURES)?.unwrap_or(135);
    let encryption_flags = number(&attributes, ENCRYPTION)?.unwrap_or(0);

    if width == 0 || width > 16_384 || width % 2 != 0 { return Err(SdpError::ValueOutOfRange(WIDTH)); }
    if height == 0 || height > 16_384 || height % 2 != 0 { return Err(SdpError::ValueOutOfRange(HEIGHT)); }
    if frames_per_second == 0 || frames_per_second > 240 { return Err(SdpError::ValueOutOfRange(FPS)); }
    if packet_size < 256 || packet_size > 1500 { return Err(SdpError::ValueOutOfRange(PACKET_SIZE)); }
    if bitrate_kbps == 0 || bitrate_kbps > 200_000 { return Err(SdpError::ValueOutOfRange(BITRATE)); }
    if minimum_fec_packets > 254 { return Err(SdpError::ValueOutOfRange(MIN_FEC)); }
    let codec = match codec_number {
        0 => VideoCodec::H264,
        1 => VideoCodec::Hevc,
        2 => VideoCodec::Av1,
        value => return Err(SdpError::UnsupportedCodec(value)),
    };

    Ok(MoonlightStreamProfile {
        width, height, frames_per_second, bitrate_kbps, packet_size, codec,
        minimum_fec_packets, moonlight_feature_flags, nvidia_feature_flags, encryption_flags,
    })
}

fn is_recognized(name: &str) -> bool {
    matches!(name, WIDTH | HEIGHT | FPS | PACKET_SIZE | BITRATE | CODEC | MIN_FEC | ML_FEATURES | NV_FEATURES | ENCRYPTION)
}

fn recognized_name(name: &str) -> &'static str {
    match name {
        WIDTH => WIDTH, HEIGHT => HEIGHT, FPS => FPS, PACKET_SIZE => PACKET_SIZE,
        BITRATE => BITRATE, CODEC => CODEC, MIN_FEC => MIN_FEC, ML_FEATURES => ML_FEATURES,
        NV_FEATURES => NV_FEATURES, ENCRYPTION => ENCRYPTION, _ => "unknown",
    }
}

fn required_number(attributes: &HashMap<&str, &str>, name: &'static str) -> Result<u32, SdpError> {
    number(attributes, name)?.ok_or(SdpError::MissingAttribute(name))
}

fn number(attributes: &HashMap<&str, &str>, name: &'static str) -> Result<Option<u32>, SdpError> {
    let Some(value) = attributes.get(name) else {
        return Ok(None);
    };
    value.parse::<u32>().map(Some).map_err(|_| SdpError::InvalidNumber(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\ns=Aphelion\r\na=x-nv-video[0].clientViewportWd:1920\r\na=x-nv-video[0].clientViewportHt:1080\r\na=x-nv-video[0].maxFPS:60\r\na=x-nv-video[0].packetSize:1200\r\na=x-nv-vqos[0].bw.maximumBitrateKbps:30000\r\na=x-nv-vqos[0].bitStreamFormat:0\r\na=x-ml-general.featureFlags:3\r\na=x-ss-general.encryptionEnabled:2\r\n";

    #[test]
    fn extracts_supported_stream_profile_and_feature_bits() {
        let profile = parse_moonlight_sdp(SDP.as_bytes()).unwrap();
        assert_eq!((profile.width, profile.height, profile.frames_per_second), (1920, 1080, 60));
        assert_eq!(profile.codec, VideoCodec::H264);
        assert_eq!(profile.packet_size, 1200);
        assert!(profile.supports_fec_status());
        assert!(profile.supports_session_id_v1());
        assert_eq!(profile.encryption_flags, 2);
    }

    #[test]
    fn rejects_missing_duplicate_invalid_and_oversized_parameters() {
        assert_eq!(parse_moonlight_sdp(b"v=0\r\n").unwrap_err(), SdpError::MissingAttribute(WIDTH));
        let duplicate = format!("{SDP}a={WIDTH}:1920\r\n");
        assert_eq!(parse_moonlight_sdp(duplicate.as_bytes()).unwrap_err(), SdpError::DuplicateAttribute(WIDTH));
        let invalid = SDP.replace("maxFPS:60", "maxFPS:0");
        assert_eq!(parse_moonlight_sdp(invalid.as_bytes()).unwrap_err(), SdpError::ValueOutOfRange(FPS));
        assert_eq!(parse_moonlight_sdp(&vec![b'x'; MAX_SDP_BYTES + 1]).unwrap_err(), SdpError::TooLarge);
    }
}
