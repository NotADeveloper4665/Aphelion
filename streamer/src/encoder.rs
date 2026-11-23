//! Low-latency software H.264 encoder adapter backed by FFmpeg's CLI.
//!
//! The adapter is a real process boundary: raw BGRA frames enter over stdin and Annex-B
//! access units leave over stdout. A bounded channel propagates backpressure. The first
//! access unit is emitted when the next AUD is observed; this one-frame framing delay
//! avoids confusing arbitrary pipe read boundaries with encoded frame boundaries.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{mpsc::{self, Receiver, RecvTimeoutError, SyncSender}, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::capture::CapturedFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_kbps: u32,
}

impl EncoderConfig {
    fn validate(self) -> Result<usize, EncoderError> {
        if self.width == 0 || self.height == 0 || self.width % 2 != 0 || self.height % 2 != 0
            || self.frames_per_second == 0 || self.frames_per_second > 240
            || self.bitrate_kbps == 0 || self.bitrate_kbps > 200_000
        {
            return Err(EncoderError::InvalidConfig);
        }
        (self.width as usize).checked_mul(self.height as usize).and_then(|pixels| pixels.checked_mul(4))
            .filter(|bytes| *bytes <= 256 * 1024 * 1024)
            .ok_or(EncoderError::InvalidConfig)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    /// Annex-B start codes are omitted from each NAL unit.
    pub nals: Vec<Vec<u8>>,
    pub received_at: Option<Instant>,
}

#[derive(Debug)]
pub enum EncoderError {
    InvalidConfig,
    FrameMismatch { expected: usize, received: usize },
    Spawn(io::Error),
    MissingPipe,
    Write(io::Error),
    Read(io::Error),
    ProcessExited(Option<i32>),
    ReaderPanicked,
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig => f.write_str("invalid FFmpeg H.264 encoder configuration"),
            Self::FrameMismatch { expected, received } => write!(f, "encoder frame is {received} bytes; expected {expected}"),
            Self::Spawn(error) => write!(f, "cannot start FFmpeg encoder: {error}"),
            Self::MissingPipe => f.write_str("FFmpeg encoder did not expose its raw frame pipes"),
            Self::Write(error) => write!(f, "cannot write frame to FFmpeg: {error}"),
            Self::Read(error) => write!(f, "cannot read H.264 output from FFmpeg: {error}"),
            Self::ProcessExited(code) => write!(f, "FFmpeg encoder exited with status {code:?}"),
            Self::ReaderPanicked => f.write_str("FFmpeg output reader thread panicked"),
        }
    }
}

impl std::error::Error for EncoderError {}

/// Blocking software H.264 encoder. Keep it on the media thread and consume access units
/// while submitting frames so its two-unit output queue can apply bounded backpressure.
pub struct FfmpegH264Encoder {
    config: EncoderConfig,
    frame_bytes: usize,
    child: Child,
    stdin: Option<ChildStdin>,
    output: Receiver<EncodedAccessUnit>,
    reader: Option<JoinHandle<Result<(), EncoderError>>>,
    timestamps: Arc<Mutex<VecDeque<Instant>>>,
}

impl FfmpegH264Encoder {
    pub fn spawn(config: EncoderConfig) -> Result<Self, EncoderError> {
        Self::spawn_with_program(config, "ffmpeg")
    }

    pub fn spawn_with_program(config: EncoderConfig, program: &str) -> Result<Self, EncoderError> {
        let frame_bytes = config.validate()?;
        let args = vec![
            "-hide_banner".to_owned(), "-loglevel".into(), "error".into(), "-f".into(), "rawvideo".into(),
            "-pixel_format".into(), "bgra".into(), "-video_size".into(), format!("{}x{}", config.width, config.height),
            "-framerate".into(), config.frames_per_second.to_string(), "-i".into(), "pipe:0".into(), "-an".into(),
            "-c:v".into(), "libx264".into(), "-preset".into(), "ultrafast".into(), "-tune".into(), "zerolatency".into(),
            "-b:v".into(), format!("{}k", config.bitrate_kbps), "-x264-params".into(), "aud=1:repeat-headers=1".into(),
            "-f".into(), "h264".into(), "pipe:1".into(),
        ];
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(EncoderError::Spawn)?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(EncoderError::MissingPipe);
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(EncoderError::MissingPipe);
        };
        let timestamps = Arc::new(Mutex::new(VecDeque::new()));
        let reader_timestamps = Arc::clone(&timestamps);
        let (sender, output) = mpsc::sync_channel(2);
        let reader = match thread::Builder::new().name("aphelion-h264-output".into())
            .spawn(move || read_access_units(stdout, sender, reader_timestamps)) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(EncoderError::Spawn(error));
            }
        };
        Ok(Self { config, frame_bytes, child, stdin: Some(stdin), output, reader: Some(reader), timestamps })
    }

    pub fn submit(&mut self, frame: &CapturedFrame) -> Result<(), EncoderError> {
        if frame.width != self.config.width || frame.height != self.config.height || frame.bgra.len() != self.frame_bytes {
            return Err(EncoderError::FrameMismatch { expected: self.frame_bytes, received: frame.bgra.len() });
        }
        self.timestamps.lock().expect("encoder timestamp queue poisoned").push_back(frame.received_at);
        let stdin = self.stdin.as_mut().ok_or(EncoderError::ProcessExited(None))?;
        let result = stdin.write_all(&frame.bgra).and_then(|_| stdin.flush());
        if let Err(error) = result {
            self.timestamps.lock().expect("encoder timestamp queue poisoned").pop_back();
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(EncoderError::ProcessExited(status.code()));
            }
            return Err(EncoderError::Write(error));
        }
        Ok(())
    }

    pub fn receive(&mut self) -> Result<EncodedAccessUnit, EncoderError> {
        self.output.recv().map_err(|_| {
            let code = self.child.try_wait().ok().flatten().and_then(|status| status.code());
            EncoderError::ProcessExited(code)
        })
    }

    pub fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedAccessUnit>, EncoderError> {
        match self.output.recv_timeout(timeout) {
            Ok(unit) => Ok(Some(unit)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                let code = self.child.try_wait().ok().flatten().and_then(|status| status.code());
                Err(EncoderError::ProcessExited(code))
            }
        }
    }

    /// Flushes the final encoded unit while draining bounded output, then reaps FFmpeg.
    pub fn finish(mut self) -> Result<Vec<EncodedAccessUnit>, EncoderError> {
        self.stdin.take();
        let mut tail = Vec::new();
        let mut process_status = None;
        loop {
            match self.output.recv_timeout(Duration::from_millis(20)) {
                Ok(unit) => tail.push(unit),
                Err(RecvTimeoutError::Timeout) => {
                    if process_status.is_none() {
                        process_status = self.child.try_wait().map_err(EncoderError::Read)?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = match process_status {
            Some(status) => status,
            None => self.child.wait().map_err(EncoderError::Read)?,
        };
        if !status.success() { return Err(EncoderError::ProcessExited(status.code())); }
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| EncoderError::ReaderPanicked)??;
        }
        while let Ok(unit) = self.output.try_recv() { tail.push(unit); }
        Ok(tail)
    }
}

impl Drop for FfmpegH264Encoder {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() { let _ = self.child.kill(); }
        let _ = self.child.wait();
    }
}

fn read_access_units(mut stdout: ChildStdout, output: SyncSender<EncodedAccessUnit>, timestamps: Arc<Mutex<VecDeque<Instant>>>) -> Result<(), EncoderError> {
    let mut parser = AnnexBAccessUnitParser::default();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let count = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(EncoderError::Read(error)),
        };
        for nals in parser.push(&chunk[..count]) {
            send_unit(&output, &timestamps, nals)?;
        }
    }
    if let Some(nals) = parser.finish() { send_unit(&output, &timestamps, nals)?; }
    Ok(())
}

fn send_unit(output: &SyncSender<EncodedAccessUnit>, timestamps: &Mutex<VecDeque<Instant>>, nals: Vec<Vec<u8>>) -> Result<(), EncoderError> {
    let received_at = timestamps.lock().expect("encoder timestamp queue poisoned").pop_front();
    output.send(EncodedAccessUnit { nals, received_at }).map_err(|_| EncoderError::ProcessExited(None))
}

#[derive(Default)]
struct AnnexBAccessUnitParser {
    bytes: Vec<u8>,
    current: Vec<Vec<u8>>,
}

impl AnnexBAccessUnitParser {
    fn push(&mut self, input: &[u8]) -> Vec<Vec<Vec<u8>>> {
        self.bytes.extend_from_slice(input);
        let starts = start_codes(&self.bytes);
        if starts.len() < 2 { return Vec::new(); }
        let mut complete = Vec::new();
        let mut consumed = 0;
        for pair in starts.windows(2) {
            let (start, prefix) = pair[0];
            let (next, _) = pair[1];
            if let Some(nal) = self.bytes.get(start + prefix..next).filter(|nal| !nal.is_empty()) {
                self.push_nal(nal.to_vec(), &mut complete);
            }
            consumed = next;
        }
        self.bytes.drain(..consumed);
        complete
    }

    fn push_nal(&mut self, nal: Vec<u8>, complete: &mut Vec<Vec<Vec<u8>>>) {
        if nal.first().is_some_and(|header| header & 0x1f == 9) && !self.current.is_empty() {
            complete.push(std::mem::take(&mut self.current));
        }
        self.current.push(nal);
    }

    fn finish(mut self) -> Option<Vec<Vec<u8>>> {
        let starts = start_codes(&self.bytes);
        if let Some((start, prefix)) = starts.first().copied() {
            if let Some(nal) = self.bytes.get(start + prefix..).filter(|nal| !nal.is_empty()) {
                let mut ignored = Vec::new();
                self.push_nal(nal.to_vec(), &mut ignored);
            }
        }
        (!self.current.is_empty()).then_some(self.current)
    }
}

fn start_codes(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            found.push((index, 4));
            index += 4;
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            found.push((index, 3));
            index += 3;
        } else { index += 1; }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_unit_parser_handles_split_start_codes_and_aud_boundaries() {
        let mut parser = AnnexBAccessUnitParser::default();
        assert!(parser.push(&[0, 0]).is_empty());
        assert!(parser.push(&[0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 1]).is_empty());
        let complete = parser.push(&[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x41, 2]);
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0], vec![vec![0x09, 0xf0], vec![0x65, 1]]);
        assert_eq!(parser.finish().unwrap(), vec![vec![0x09, 0xf0], vec![0x41, 2]]);
    }

    #[test]
    fn validates_even_dimensions_and_bounded_rate_and_bitrate() {
        assert!(EncoderConfig { width: 1920, height: 1080, frames_per_second: 60, bitrate_kbps: 10_000 }.validate().is_ok());
        assert!(matches!(EncoderConfig { width: 1919, height: 1080, frames_per_second: 60, bitrate_kbps: 10_000 }.validate(), Err(EncoderError::InvalidConfig)));
    }

    #[test]
    fn ffmpeg_process_encodes_frames_and_flushes_final_access_unit() {
        let config = EncoderConfig { width: 16, height: 16, frames_per_second: 30, bitrate_kbps: 500 };
        let mut encoder = FfmpegH264Encoder::spawn(config).expect("CI installs FFmpeg with libx264");
        for fill in [0x10, 0x80] {
            encoder.submit(&CapturedFrame {
                width: config.width,
                height: config.height,
                received_at: Instant::now(),
                bgra: vec![fill; 16 * 16 * 4].into(),
            }).unwrap();
        }
        let units = encoder.finish().unwrap();
        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|unit| unit.nals.iter().any(|nal| nal.first().is_some_and(|header| header & 0x1f == 9))));
        assert!(units.iter().all(|unit| unit.received_at.is_some()));
    }
}
