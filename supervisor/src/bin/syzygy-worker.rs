use aphelion_supervisor::{
    protocol_version_supported, Message, SessionIdMapper, MAX_MESSAGE_BYTES,
};
use serde_json::{json, Value};
use std::{
    env,
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use nix::{sys::signal::{killpg, Signal}, unistd::Pid};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SYZYGY_STOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(12);

#[tokio::main]
async fn main() -> Result<()> {
    if let Err(error) = run().await {
        eprintln!("syzygy-worker: {error}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let socket_path = PathBuf::from(argument(&args, "--socket")?);
    let worker_id = argument(&args, "--worker-id")?.to_owned();
    let generation = argument(&args, "--generation")?.parse::<u64>()?;
    let stream = connect_with_retry(&socket_path).await?;
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);

    handshake(&mut reader, &mut writer, &worker_id, generation).await?;
    eprintln!("syzygy-worker: control handshake complete for generation {generation}");

    let mut child: Option<ManagedChild> = None;
    let mut sessions = SessionIdMapper::new(worker_id.clone(), generation);
    let (session_event_tx, mut session_event_rx) = mpsc::channel(64);
    let mut last_exit: Option<Value> = None;

    loop {
        if let Some(current) = child.as_mut() {
            if let Some(status) = current.child.try_wait()? {
                last_exit = Some(json!({
                    "success": status.success(),
                    "code": status.code(),
                    "signal": status.signal(),
                }));
                child = None;
                return Err("managed Syzygy host process exited unexpectedly".into());
            }
        }

        let request = tokio::select! {
            Some(event) = session_event_rx.recv() => {
                emit_session_event(
                    event,
                    &mut sessions,
                    &mut writer,
                    &worker_id,
                    generation,
                ).await?;
                continue;
            }
            result = timeout(Duration::from_secs(60), read_message(&mut reader)) => result??,
        };
        validate_request(&request, &worker_id, generation)?;
        while let Ok(event) = session_event_rx.try_recv() {
            emit_session_event(event, &mut sessions, &mut writer, &worker_id, generation).await?;
        }

        match request.message_type.as_str() {
            "host.start" => {
                reject_session_id(&request)?;
                let response_body = if child.is_some() {
                    json!({
                        "ok": true,
                        "state": "process_running",
                        "already_running": true,
                        "stream_ready": false,
                        "process_id": child.as_ref().and_then(ManagedChild::id),
                    })
                } else {
                    let mut spawned = spawn_syzygy(session_event_tx.clone())?;
                    // Allow immediate exec failures to surface before acknowledging process start.
                    sleep(Duration::from_millis(100)).await;
                    if let Some(status) = spawned.child.try_wait()? {
                        return Err(format!("Syzygy exited during launch with {status}").into());
                    }
                    let process_id = spawned.child.id();
                    child = Some(spawned);
                    last_exit = None;
                    json!({
                        "ok": true,
                        "state": "process_started",
                        "already_running": false,
                        "stream_ready": false,
                        "process_id": process_id,
                    })
                };
                send_response(&mut writer, &request, &worker_id, generation, response_body).await?;
            }
            "host.stop" => {
                reject_session_id(&request)?;
                let was_running = child.is_some();
                if let Some(mut process) = child.take() {
                    stop_process(
                        &mut process,
                        &mut session_event_rx,
                        &mut sessions,
                        &mut writer,
                        &worker_id,
                        generation,
                    ).await?;
                }
                send_response(
                    &mut writer,
                    &request,
                    &worker_id,
                    generation,
                    json!({
                        "ok": true,
                        "state": "stopped",
                        "was_running": was_running,
                        "stream_ready": false,
                    }),
                )
                .await?;
            }
            "worker.health" => {
                let running = if let Some(process) = child.as_mut() {
                    match process.child.try_wait()? {
                        None => true,
                        Some(status) => {
                            last_exit = Some(json!({
                                "success": status.success(),
                                "code": status.code(),
                                "signal": status.signal(),
                            }));
                            child = None;
                            return Err("managed Syzygy host process exited unexpectedly".into());
                        }
                    }
                } else {
                    false
                };
                send_response(
                    &mut writer,
                    &request,
                    &worker_id,
                    generation,
                    json!({
                        "state": if running { "process_running" } else { "stopped" },
                        "process_running": running,
                        "process_id": child.as_ref().and_then(ManagedChild::id),
                        "last_exit": last_exit,
                        "stream_ready": false,
                        "client_session_count": sessions.active_count(),
                        "client_session_count_source": "SYZYGY_SESSION_START_STOP logs",
                        "media_health": null,
                    }),
                )
                .await?;
            }
            "worker.shutdown" => {
                if let Some(mut process) = child.take() {
                    stop_process(
                        &mut process,
                        &mut session_event_rx,
                        &mut sessions,
                        &mut writer,
                        &worker_id,
                        generation,
                    ).await?;
                }
                flush_session_events(
                    &mut session_event_rx,
                    &mut sessions,
                    &mut writer,
                    &worker_id,
                    generation,
                ).await?;
                send_response(
                    &mut writer,
                    &request,
                    &worker_id,
                    generation,
                    json!({"ok": true, "state": "shutdown"}),
                )
                .await?;
                break;
            }
            _ => {
                send_response(
                    &mut writer,
                    &request,
                    &worker_id,
                    generation,
                    json!({"ok": false, "error": "unsupported request type"}),
                )
                .await?;
            }
        }
    }

    Ok(())
}

struct ManagedChild {
    child: Child,
    process_group: Pid,
    output_tasks: Vec<JoinHandle<()>>,
    cleaned: bool,
}

impl ManagedChild {
    fn id(&self) -> Option<u32> {
        self.child.id()
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = killpg(self.process_group, Signal::SIGKILL);
        }
    }
}

fn spawn_syzygy(session_event_tx: mpsc::Sender<ObservedSessionLog>) -> Result<ManagedChild> {
    let executable = env::var_os("APHELION_SYZYGY_EXECUTABLE")
        .ok_or("APHELION_SYZYGY_EXECUTABLE must point to the existing Syzygy binary")?;
    let configured_args = env::var("APHELION_SYZYGY_ARGS").unwrap_or_else(|_| "[]".to_owned());
    let configured_args: Vec<String> = serde_json::from_str(&configured_args)?;
    for arg in &configured_args {
        if matches!(arg.as_str(), "-s" | "-stop" | "-status" | "-foreground") {
            return Err(format!("APHELION_SYZYGY_ARGS must not include supervisor-owned flag {arg}").into());
        }
    }

    let mut command = Command::new(executable);
    command.args(configured_args)
        .arg("-s")
        .arg("-foreground")
        .env("SYZYGY_MANAGED_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let process_group = Pid::from_raw(child.id().ok_or("Syzygy child has no process ID")? as i32);

    let mut output_tasks = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        output_tasks.push(tokio::spawn(drain_output(stdout, "stdout", Some(session_event_tx))));
    }
    if let Some(stderr) = child.stderr.take() {
        output_tasks.push(tokio::spawn(drain_output(stderr, "stderr", None)));
    }
    Ok(ManagedChild { child, process_group, output_tasks, cleaned: false })
}

#[derive(Debug)]
struct ObservedSessionLog {
    started: bool,
    source_token: String,
}

async fn drain_output<R: AsyncRead + Unpin>(
    stream: R,
    label: &'static str,
    session_event_tx: Option<mpsc::Sender<ObservedSessionLog>>,
) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let observed = session_event_tx.as_ref()
                    .and_then(|_| parse_session_lifecycle_log(&line));
                if let (Some(sender), Some((started, token))) = (session_event_tx.as_ref(), observed) {
                    if sender.send(ObservedSessionLog {
                        started,
                        source_token: token.to_owned(),
                    }).await.is_err() {
                        eprintln!("syzygy-worker: supervisor session-event channel closed");
                        break;
                    }
                    let action = if started { "start" } else { "stop" };
                    eprintln!("syzygy[{label}]: observed client session {action}");
                } else {
                    eprintln!("syzygy[{label}]: {line}");
                }
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("syzygy-worker: failed reading child {label}: {error}");
                break;
            }
        }
    }
}

fn map_session_event(
    event: ObservedSessionLog,
    sessions: &mut SessionIdMapper,
    worker_id: &str,
    generation: u64,
) -> Result<Option<Message>> {
    let (message_type, session_id) = if event.started {
        let (session_id, newly_active) = sessions.start(&event.source_token)
            .map_err(|error| error.to_owned())?;
        if !newly_active {
            return Ok(None);
        }
        ("session.started", session_id)
    } else if let Some(session_id) = sessions.stop(&event.source_token) {
        ("session.stopped", session_id)
    } else {
        return Ok(None);
    };
    Ok(Some(Message::new(
        "event",
        message_type,
        worker_id,
        generation,
        None,
        Some(session_id),
        json!({"source": "syzygy_session_lifecycle_log"}),
    )))
}

async fn emit_session_event<W: AsyncWrite + Unpin>(
    event: ObservedSessionLog,
    sessions: &mut SessionIdMapper,
    writer: &mut W,
    worker_id: &str,
    generation: u64,
) -> Result<()> {
    if let Some(message) = map_session_event(event, sessions, worker_id, generation)? {
        send_message(writer, &message).await?;
    }
    Ok(())
}

async fn flush_session_events<W: AsyncWrite + Unpin>(
    receiver: &mut mpsc::Receiver<ObservedSessionLog>,
    sessions: &mut SessionIdMapper,
    writer: &mut W,
    worker_id: &str,
    generation: u64,
) -> Result<()> {
    while let Ok(event) = receiver.try_recv() {
        emit_session_event(event, sessions, writer, worker_id, generation).await?;
    }
    for session_id in sessions.stop_all() {
        send_message(
            writer,
            &Message::new(
                "event",
                "session.stopped",
                worker_id,
                generation,
                None,
                Some(session_id),
                json!({"source": "host_stopped"}),
            ),
        ).await?;
    }
    Ok(())
}

fn parse_session_lifecycle_log(line: &str) -> Option<(bool, &str)> {
    for (marker, started) in [
        ("SYZYGY_SESSION_START ", true),
        ("SYZYGY_SESSION_STOP ", false),
    ] {
        if let Some(rest) = line.split_once(marker).map(|(_, rest)| rest) {
            let token = rest.split_whitespace().next()?;
            if !token.is_empty() {
                return Some((started, token));
            }
        }
    }
    None
}

async fn stop_process<W: AsyncWrite + Unpin>(
    process: &mut ManagedChild,
    session_events: &mut mpsc::Receiver<ObservedSessionLog>,
    sessions: &mut SessionIdMapper,
    writer: &mut W,
    worker_id: &str,
    generation: u64,
) -> Result<()> {
    request_syzygy_service_stop().await;
    let _ = killpg(process.process_group, Signal::SIGTERM);
    let graceful = timeout(SHUTDOWN_GRACE, async {
        loop {
            tokio::select! {
                status = process.child.wait() => {
                    status?;
                    break;
                }
                Some(event) = session_events.recv() => {
                    emit_session_event(event, sessions, writer, worker_id, generation).await?;
                }
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }).await;
    match graceful {
        Ok(result) => result?,
        Err(_) => {
            let _ = killpg(process.process_group, Signal::SIGKILL);
            process.child.kill().await?;
            process.child.wait().await?;
        }
    }

    // Remove helper descendants even when the leader exits during the grace window.
    let _ = killpg(process.process_group, Signal::SIGKILL);
    for mut task in process.output_tasks.drain(..) {
        loop {
            tokio::select! {
                result = &mut task => {
                    let _ = result;
                    break;
                }
                Some(event) = session_events.recv() => {
                    emit_session_event(event, sessions, writer, worker_id, generation).await?;
                }
            }
        }
    }
    flush_session_events(session_events, sessions, writer, worker_id, generation).await?;
    process.cleaned = true;
    Ok(())
}

/// Ask Syzygy's own background manager to stop, using the exact binary selected
/// for this worker. Aphelion also owns a foreground process group, so the
/// direct group shutdown below remains necessary and is the fallback if the
/// service manager has no matching instance or the command fails.
async fn request_syzygy_service_stop() {
    let Some(executable) = env::var_os("APHELION_SYZYGY_EXECUTABLE") else {
        eprintln!("syzygy-worker: cannot request Syzygy -stop: executable is unset");
        return;
    };

    let mut command = Command::new(executable);
    command
        .arg("-stop")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = timeout(SYZYGY_STOP_COMMAND_TIMEOUT, command.output()).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            eprintln!(
                "syzygy-worker: Syzygy -stop exited with {}; stdout={stdout:?}; stderr={stderr:?}",
                output.status
            );
        }
        Ok(Err(error)) => {
            eprintln!("syzygy-worker: Syzygy -stop failed to launch: {error}");
        }
        Err(_) => {
            eprintln!("syzygy-worker: Syzygy -stop timed out; escalating to owned process group");
        }
    }
}

async fn handshake<R, W>(
    reader: &mut BufReader<R>,
    writer: &mut W,
    worker_id: &str,
    generation: u64,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let request_id = Uuid::new_v4().to_string();
    send_message(
        writer,
        &Message::new(
            "event",
            "worker.hello",
            worker_id,
            generation,
            Some(request_id.clone()),
            None,
            json!({
                "build": env!("CARGO_PKG_NAME").to_owned() + "/" + env!("CARGO_PKG_VERSION"),
                "adapter": "syzygy-foreground-process",
                "capabilities": ["host.start", "host.stop", "worker.health", "worker.shutdown"]
            }),
        ),
    )
    .await?;

    let ack = timeout(Duration::from_secs(5), read_message(reader)).await??;
    if ack.kind != "response"
        || ack.message_type != "worker.hello_ack"
        || ack.request_id.as_deref() != Some(&request_id)
        || !protocol_version_supported(&ack.protocol)
        || ack.worker_instance_id != worker_id
        || ack.worker_generation != generation
        || ack.body["accepted"] != true
    {
        return Err("supervisor rejected or miscorrelated worker handshake".into());
    }
    Ok(())
}

fn validate_request(request: &Message, worker_id: &str, generation: u64) -> Result<()> {
    if !protocol_version_supported(&request.protocol) {
        return Err("unsupported supervisor protocol version".into());
    }
    if request.kind != "request"
        || request.worker_instance_id != worker_id
        || request.worker_generation != generation
        || !matches!(request.request_id.as_deref(), Some(id) if !id.trim().is_empty())
    {
        return Err("invalid request envelope or worker identity".into());
    }
    Ok(())
}

fn reject_session_id(request: &Message) -> Result<()> {
    if request.session_id.is_some() {
        return Err("host lifecycle request must not include session_id".into());
    }
    Ok(())
}

async fn send_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: &Message,
    worker_id: &str,
    generation: u64,
    body: Value,
) -> Result<()> {
    send_message(
        writer,
        &Message::new(
            "response",
            request.message_type.clone(),
            worker_id,
            generation,
            request.request_id.clone(),
            request.session_id.clone(),
            body,
        ),
    )
    .await
}

async fn send_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    if bytes.len().saturating_add(1) > MAX_MESSAGE_BYTES {
        return Err("outgoing IPC message exceeds size limit".into());
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Message> {
    let mut line = Vec::new();
    let mut limited = reader.take((MAX_MESSAGE_BYTES + 1) as u64);
    let count = timeout(Duration::from_secs(5), limited.read_until(b'\n', &mut line)).await??;
    if count == 0 {
        return Err("supervisor disconnected".into());
    }
    if count > MAX_MESSAGE_BYTES {
        return Err("IPC message exceeds size limit".into());
    }
    if !line.ends_with(b"\n") {
        return Err("truncated IPC message".into());
    }
    Ok(serde_json::from_slice(&line)?)
}

async fn connect_with_retry(path: &PathBuf) -> Result<UnixStream> {
    let mut last_error = None;
    for _ in 0..50 {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last_error = Some(error);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(format!("could not connect to supervisor socket: {}", last_error.unwrap()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_managed_child_kills_its_process_group() {
        let directory = env::temp_dir().join(format!("aphelion-worker-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await.expect("create test directory");
        let marker = directory.join("descendant-survived");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 0.2; touch \"$1\"; sleep 60")
            .arg("test-shell")
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().expect("spawn process group");
        let process_group = Pid::from_raw(child.id().expect("test pid") as i32);
        drop(ManagedChild { child, process_group, output_tasks: Vec::new(), cleaned: false });

        sleep(Duration::from_millis(400)).await;
        assert!(!marker.exists(), "descendant in the worker process group must be killed");
        tokio::fs::remove_dir_all(directory).await.expect("remove test directory");
    }

    #[test]
    fn host_lifecycle_rejects_client_session_identity() {
        let request = Message::new(
            "request",
            "host.start",
            "worker-a",
            1,
            Some("request-a".to_owned()),
            Some("session-a".to_owned()),
            json!({}),
        );
        assert!(reject_session_id(&request)
            .unwrap_err()
            .to_string()
            .contains("must not include session_id"));
    }

    #[test]
    fn maps_log_tokens_to_instance_scoped_events_without_leaking_tokens() {
        let mut sessions = SessionIdMapper::new("worker-a", 3);
        let started = map_session_event(
            ObservedSessionLog { started: true, source_token: "private-pointer".to_owned() },
            &mut sessions,
            "worker-a",
            3,
        ).expect("map start event").expect("new session event");
        assert_eq!(started.message_type, "session.started");
        assert_eq!(started.session_id.as_deref(), Some("session-worker-a-3-1"));
        assert!(!serde_json::to_string(&started).unwrap().contains("private-pointer"));

        let stopped = map_session_event(
            ObservedSessionLog { started: false, source_token: "private-pointer".to_owned() },
            &mut sessions,
            "worker-a",
            3,
        ).expect("map stop event").expect("known session stop event");
        assert_eq!(stopped.message_type, "session.stopped");
        assert_eq!(stopped.session_id.as_deref(), Some("session-worker-a-3-1"));
        assert_eq!(sessions.active_count(), 0);
    }

    #[test]
    fn parses_syzygy_session_lifecycle_logs_without_exposing_tokens() {
        assert_eq!(
            parse_session_lifecycle_log("info: SYZYGY_SESSION_START 0x1234 hevc"),
            Some((true, "0x1234")),
        );
        assert_eq!(
            parse_session_lifecycle_log("info: SYZYGY_SESSION_STOP 0x1234"),
            Some((false, "0x1234")),
        );
        assert_eq!(parse_session_lifecycle_log("ordinary Syzygy output"), None);
        assert_eq!(parse_session_lifecycle_log("SYZYGY_SESSION_START "), None);
    }
}

fn argument<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    let index = args.iter().position(|arg| arg == name)
        .ok_or_else(|| format!("missing {name}"))?;
    args.get(index + 1).map(String::as_str)
        .ok_or_else(|| format!("missing value for {name}").into())
}
