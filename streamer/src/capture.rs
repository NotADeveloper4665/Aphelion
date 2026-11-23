//! PipeWire capture adapter for a node selected by an authorized desktop portal session.
//!
//! The portal/session chooser is deliberately owned by the host UI/control layer. This
//! module accepts only the selected PipeWire node ID and captures fixed-size BGRA frames
//! from GStreamer. It does not enumerate screens, bypass portal permissions, or transmit
//! frames over supervisor IPC.

use std::{
    io::{self, Read},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

const CAPTURE_QUEUE_DEPTH: usize = 2;
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

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Monotonic time at which this process received the complete frame.
    pub received_at: Instant,
    /// BGRA8, tightly packed, row-major pixels.
    pub bgra: Arc<[u8]>,
}

#[derive(Debug)]
pub enum CaptureError {
    InvalidConfig,
    Spawn(io::Error),
    MissingPipe,
    ProcessExited(Option<i32>),
    SharedSourceClosed,
    FrameSequenceExhausted,
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
            Self::SharedSourceClosed => f.write_str("shared capture source has closed"),
            Self::FrameSequenceExhausted => f.write_str("shared capture frame sequence exhausted"),
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

/// Latest-frame fan-out for one capture owner and multiple per-session consumers.
///
/// The hub retains only one frame. A slow subscriber skips stale frames instead of
/// building a queue; subscribers share the same pixel allocation through Arc.
#[derive(Clone, Default)]
pub struct LatestFrameHub {
    shared: Arc<(Mutex<LatestFrameState>, Condvar)>,
}

#[derive(Default)]
struct LatestFrameState {
    sequence: u64,
    latest: Option<Arc<CapturedFrame>>,
    closed: bool,
}

pub struct LatestFrameSubscription {
    shared: Arc<(Mutex<LatestFrameState>, Condvar)>,
    seen_sequence: u64,
}

impl LatestFrameHub {
    pub fn new() -> Self { Self::default() }

    /// Publish a capture result to all sessions, replacing the retained frame.
    pub fn publish(&self, frame: CapturedFrame) -> Result<(), CaptureError> {
        let (state_lock, ready) = &*self.shared;
        let mut state = state_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed { return Err(CaptureError::SharedSourceClosed); }
        state.sequence = state.sequence.checked_add(1).ok_or(CaptureError::FrameSequenceExhausted)?;
        state.latest = Some(Arc::new(frame));
        ready.notify_all();
        Ok(())
    }

    /// Subscribe for future frames. Each subscriber has its own sequence cursor.
    pub fn subscribe(&self) -> LatestFrameSubscription {
        let (state_lock, _) = &*self.shared;
        let state = state_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        LatestFrameSubscription { shared: Arc::clone(&self.shared), seen_sequence: state.sequence }
    }

    /// Wake all consumers and make further reads return a closed-source error.
    pub fn close(&self) {
        let (state_lock, ready) = &*self.shared;
        let mut state = state_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        ready.notify_all();
    }
}

impl FrameSource for LatestFrameSubscription {
    fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
        let (state_lock, ready) = &*self.shared;
        let mut state = state_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.sequence > self.seen_sequence {
                let frame = state.latest.as_ref().expect("published sequence always has a frame");
                self.seen_sequence = state.sequence;
                return Ok(frame.as_ref().clone());
            }
            if state.closed { return Err(CaptureError::SharedSourceClosed); }
            state = ready.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
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
        Ok(CapturedFrame { width: self.config.width, height: self.config.height, received_at: Instant::now(), bgra: bgra.into() })
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
        "queue".into(),
        format!("max-size-buffers={CAPTURE_QUEUE_DEPTH}"),
        "max-size-bytes=0".into(),
        "max-size-time=0".into(),
        "leaky=downstream".into(),
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
        assert!(args.iter().any(|arg| arg == "max-size-buffers=2"));
        assert!(args.iter().any(|arg| arg == "max-size-bytes=0"));
        assert!(args.iter().any(|arg| arg == "max-size-time=0"));
        assert!(args.iter().any(|arg| arg == "leaky=downstream"));
        assert!(!args.iter().any(|arg| arg.contains("device=")));
    }

    fn frame(fill: u8) -> CapturedFrame {
        CapturedFrame {
            width: 2,
            height: 2,
            received_at: Instant::now(),
            bgra: vec![fill; 16].into(),
        }
    }

    #[test]
    fn capture_hub_fans_out_shared_frames_and_drops_stale_frames() {
        let hub = LatestFrameHub::new();
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();

        hub.publish(frame(1)).unwrap();
        hub.publish(frame(2)).unwrap();

        let first_frame = first.next_frame().unwrap();
        let second_frame = second.next_frame().unwrap();
        assert_eq!(first_frame.bgra.as_ref(), &[2; 16]);
        assert!(Arc::ptr_eq(&first_frame.bgra, &second_frame.bgra));
    }

    #[test]
    fn capture_hub_close_unblocks_future_reads_and_rejects_publish() {
        let hub = LatestFrameHub::new();
        let mut subscriber = hub.subscribe();
        hub.close();
        assert!(matches!(subscriber.next_frame(), Err(CaptureError::SharedSourceClosed)));
        assert!(matches!(hub.publish(frame(3)), Err(CaptureError::SharedSourceClosed)));
    }

    #[cfg(unix)]
    fn fixture_program(script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        use std::io::Write;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let script_contents = format!("#!/bin/sh\n{script}\n");
        let path = loop {
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let candidate = std::env::temp_dir().join(format!("aphelion-capture-fixture-{}-{now}-{id}.sh", std::process::id()));
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
                Ok(mut file) => {
                    file.write_all(script_contents.as_bytes()).unwrap();
                    drop(file);
                    break candidate;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create capture fixture: {error}"),
            }
        };
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn reads_complete_bgra_frames_and_reports_truncated_output() {
        let config = CaptureConfig { node_id: 8, width: 1, height: 1, frames_per_second: 30 };
        let complete_program = fixture_program("printf ABCD");
        let mut capture = PipeWireCapture::spawn_with_program(config, &complete_program).unwrap();
        let frame = capture.next_frame().unwrap();
        assert_eq!(frame.bgra.as_ref(), b"ABCD");
        assert_eq!((frame.width, frame.height), (1, 1));
        let _ = capture.stop();
        std::fs::remove_file(complete_program).unwrap();

        let truncated_program = fixture_program("printf AB");
        let mut capture = PipeWireCapture::spawn_with_program(config, &truncated_program).unwrap();
        assert!(matches!(capture.next_frame(), Err(CaptureError::TruncatedFrame { expected: 4, received: 2 })));
        drop(capture);
        std::fs::remove_file(truncated_program).unwrap();
    }
}
