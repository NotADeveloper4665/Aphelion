//! PipeWire capture adapter for a node selected by an authorized desktop portal session.
//!
//! The portal/session chooser is deliberately owned by the host UI/control layer. This
//! module accepts only the selected PipeWire node ID and captures fixed-size BGRA frames
//! from GStreamer. It does not enumerate screens, bypass portal permissions, or transmit
//! frames over supervisor IPC.

use std::{
    io::{self, Read},
    process::{Child, ChildStdout, Command, Stdio},
    time::Instant,
};

const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// PipeWire node returned for the user's selected portal stream.
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
}

impl CaptureConfig {
    pub fn frame_bytes(self) -> Result<usize, CaptureError> {
        if self.node_id == 0 || self.width == 0 || self.height == 0 {
            return Err(CaptureError::InvalidConfig);
        }
        if self.frames_per_second == 0 || self.frames_per_second > 240 {
            return Err(CaptureError::InvalidConfig);
        }
        let bytes = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|bytes| *bytes <= MAX_FRAME_BYTES)
            .ok_or(CaptureError::InvalidConfig)?;
        Ok(bytes)
    }
}

#[derive(Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Monotonic time at which this process received the complete frame.
    pub captured_at: Instant,
    /// BGRA8, tightly packed, row-major pixels.
    pub bgra: Vec<u8>,
}

#[derive(Debug)]
pub enum CaptureError {
    InvalidConfig,
    Spawn(io::Error),
    MissingPipe,
    ProcessExited(Option<i32>),
    TruncatedFrame { expected: usize, received: usize },
    Read(io::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig => f.write_str("invalid or oversized PipeWire capture configuration"),
            Self::Spawn(error) => write!(f, "cannot start GStreamer PipeWire capture: {error}"),
            Self::MissingPipe => f.write_str("GStreamer capture process did not expose its frame pipe"),
            Self::ProcessExited(code) => write!(f, "GStreamer capture process exited with status {code:?}"),
            Self::TruncatedFrame { expected, received } => write!(f, "capture pipe closed mid-frame ({received}/{expected} bytes)"),
            Self::Read(error) => write!(f, "cannot read captured frame: {error}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// A synchronous, fixed-format frame source. Run it on a dedicated media thread.
pub trait FrameSource {
    fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError>;
}

/// Captures BGRA frames from a portal-authorized PipeWire node using GStreamer.
pub struct PipeWireCapture {
    config: CaptureConfig,
    frame_bytes: usize,
    child: Child,
    stdout: ChildStdout,
}

impl PipeWireCapture {
    pub fn spawn(config: CaptureConfig) -> Result<Self, CaptureError> {
        Self::spawn_with_program(config, "gst-launch-1.0")
    }

    /// Explicit executable path is exposed for packaging and controlled test harnesses.
    pub fn spawn_with_program(config: CaptureConfig, program: &str) -> Result<Self, CaptureError> {
        let frame_bytes = config.frame_bytes()?;
        let mut child = Command::new(program)
            .args(gstreamer_args(config))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(CaptureError::Spawn)?;
        let stdout = child.stdout.take().ok_or(CaptureError::MissingPipe)?;
        Ok(Self { config, frame_bytes, child, stdout })
    }

    pub fn stop(&mut self) -> Result<(), CaptureError> {
        if self.child.try_wait().map_err(CaptureError::Read)?.is_none() {
            self.child.kill().map_err(CaptureError::Read)?;
        }
        self.child.wait().map_err(CaptureError::Read)?;
        Ok(())
    }
}

impl FrameSource for PipeWireCapture {
    fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
        let mut bgra = vec![0; self.frame_bytes];
        let mut received = 0;
        while received < bgra.len() {
            match self.stdout.read(&mut bgra[received..]) {
                Ok(0) if received == 0 => {
                    let status = self.child.try_wait().map_err(CaptureError::Read)?;
                    return Err(CaptureError::ProcessExited(status.and_then(|s| s.code())));
                }
                Ok(0) => return Err(CaptureError::TruncatedFrame { expected: bgra.len(), received }),
                Ok(bytes) => received += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(CaptureError::Read(error)),
            }
        }
        Ok(CapturedFrame { width: self.config.width, height: self.config.height, captured_at: Instant::now(), bgra })
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn gstreamer_args(config: CaptureConfig) -> Vec<String> {
    vec![
        "-q".into(),
        "pipewiresrc".into(),
        format!("path={}", config.node_id),
        "do-timestamp=true".into(),
        "!".into(),
        format!("video/x-raw,format=BGRA,width={},height={},framerate={}/1", config.width, config.height, config.frames_per_second),
        "!".into(),
        "fdsink".into(),
        "fd=1".into(),
        "sync=false".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_frame_size_and_rate_before_spawning() {
        assert_eq!(CaptureConfig { node_id: 2, width: 2, height: 3, frames_per_second: 60 }.frame_bytes().unwrap(), 24);
        assert!(matches!(CaptureConfig { node_id: 0, width: 2, height: 3, frames_per_second: 60 }.frame_bytes(), Err(CaptureError::InvalidConfig)));
        assert!(matches!(CaptureConfig { node_id: 2, width: 16_384, height: 16_384, frames_per_second: 60 }.frame_bytes(), Err(CaptureError::InvalidConfig)));
        assert!(matches!(CaptureConfig { node_id: 2, width: 2, height: 3, frames_per_second: 241 }.frame_bytes(), Err(CaptureError::InvalidConfig)));
    }

    #[test]
    fn command_uses_only_explicit_node_and_fixed_bgra_caps() {
        let args = gstreamer_args(CaptureConfig { node_id: 42, width: 1920, height: 1080, frames_per_second: 60 });
        assert!(args.iter().any(|arg| arg == "path=42"));
        assert!(args.iter().any(|arg| arg == "video/x-raw,format=BGRA,width=1920,height=1080,framerate=60/1"));
        assert!(!args.iter().any(|arg| arg.contains("device=")));
    }
}
