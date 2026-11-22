use aphelion_supervisor::{
    protocol_version_supported, Message, SessionIntent,
    MAX_MESSAGE_BYTES,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    env,
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::Stdio,
    sync::{atomic::{AtomicUsize, Ordering}, Arc},
    time::Duration,
};
use nix::{sys::signal::{killpg, Signal}, unistd::Pid};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    time::{sleep, timeout},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
    let client_session_count = Arc::new(AtomicUsize::new(0));
    let mut managed_session: Option<String> = None;
    let mut last_exit: Option<Value> = None;

    loop {
        if let Some(current) = child.as_mut() {
            if let Some(status) = current.child.try_wait()? {
                let was_managed = managed_session.is_some();
                last_exit = Some(json!({
                    "success": status.success(),
                    "code": status.code(),
                    "signal": status.signal(),
                }));
                child = None;
                managed_session = None;
                client_session_count.store(0, Ordering::Relaxed);
                if was_managed {
                    return Err("managed Syzygy host process exited unexpectedly".into());
                }
            }
        }

        let request = timeout(Duration::from_secs(60), read_message(&mut reader)).await??;
        validate_request(&request, &worker_id, generation)?;

        match request.message_type.as_str() {
            "session.start" => {
                let session_id = required_session_id(&request)?;
                let intent: SessionIntent = serde_json::from_value(request.body.clone())?;
                intent.validate().map_err(|error| error.to_owned())?;

                let response_body = if let Some(current_session) = managed_session.as_deref() {
                    if current_session != session_id {
                        return Err("Syzygy worker supports one host process; another session is already managed".into());
                    }
                    json!({
                        "ok": true,
                        "state": "process_running",
                        "already_running": true,
                        "stream_ready": false,
                        "intent_applied": false,
                        "process_id": child.as_ref().and_then(ManagedChild::id),
                    })
                } else {
                    client_session_count.store(0, Ordering::Relaxed);
                    let mut spawned = spawn_syzygy(Arc::clone(&client_session_count))?;
                    // Allow immediate exec failures to surface before acknowledging process start.
                    sleep(Duration::from_millis(100)).await;
                    if let Some(status) = spawned.child.try_wait()? {
                        return Err(format!("Syzygy exited during launch with {status}").into());
                    }
                    let process_id = spawned.child.id();
                    child = Some(spawned);
                    managed_session = Some(session_id.to_owned());
                    last_exit = None;
                    json!({
                        "ok": true,
                        "state": "process_started",
                        "already_running": false,
                        "stream_ready": false,
                        "intent_applied": false,
                        "requested_intent": intent,
                        "process_id": process_id,
                    })
                };
                send_response(&mut writer, &request, &worker_id, generation, response_body).await?;
            }
            "session.stop" => {
                let session_id = required_session_id(&request)?;
                if let Some(current_session) = managed_session.as_deref() {
                    if current_session != session_id {
                        return Err("session.stop does not match the managed host session".into());
                    }
                }
                let was_running = child.is_some();
                if let Some(mut process) = child.take() {
                    stop_process(&mut process).await?;
                }
                managed_session = None;
                client_session_count.store(0, Ordering::Relaxed);
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
                            let was_managed = managed_session.is_some();
                            last_exit = Some(json!({
                                "success": status.success(),
                                "code": status.code(),
                                "signal": status.signal(),
                            }));
                            child = None;
                            managed_session = None;
                            client_session_count.store(0, Ordering::Relaxed);
                            if was_managed {
                                return Err("managed Syzygy host process exited unexpectedly".into());
                            }
                            false
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
                        "managed_session": managed_session,
                        "last_exit": last_exit,
                        "stream_ready": false,
                        "client_session_count": client_session_count.load(Ordering::Relaxed),
                        "client_session_count_source": "SYZYGY_SESSION_START_STOP logs",
                        "media_health": null,
                    }),
                )
                .await?;
            }
            "worker.shutdown" => {
                if let Some(mut process) = child.take() {
                    stop_process(&mut process).await?;
                }
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

fn spawn_syzygy(client_session_count: Arc<AtomicUsize>) -> Result<ManagedChild> {
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

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain_output(stdout, "stdout", Some(client_session_count)));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(drain_output(stderr, "stderr", None));
    }
    Ok(ManagedChild { child, process_group, cleaned: false })
}

async fn drain_output<R: AsyncRead + Unpin>(
    stream: R,
    label: &'static str,
    client_session_count: Option<Arc<AtomicUsize>>,
) {
    let mut active_tokens = HashSet::new();
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let observed = match (client_session_count.as_ref(), parse_session_lifecycle_log(&line)) {
                    (Some(count), Some((started, token))) => {
                        if started {
                            active_tokens.insert(token.to_owned());
                        } else {
                            active_tokens.remove(token);
                        }
                        count.store(active_tokens.len(), Ordering::Relaxed);
                        Some((started, active_tokens.len()))
                    }
                    _ => None,
                };
                if let Some((started, active_count)) = observed {
                    let action = if started { "started" } else { "stopped" };
                    eprintln!("syzygy[{label}]: observed client session {action}; active count {active_count}");
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

async fn stop_process(process: &mut ManagedChild) -> Result<()> {
    let _ = killpg(process.process_group, Signal::SIGTERM);
    match timeout(SHUTDOWN_GRACE, process.child.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) => {
            let _ = killpg(process.process_group, Signal::SIGKILL);
            process.child.kill().await?;
        }
    }
    // Remove helper descendants even when the leader exits during the grace window.
    let _ = killpg(process.process_group, Signal::SIGKILL);
    process.cleaned = true;
    Ok(())
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
                "capabilities": ["session.start", "session.stop", "worker.health", "worker.shutdown"]
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

fn required_session_id(request: &Message) -> Result<&str> {
    request.session_id.as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "session request requires a non-empty session_id".into())
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
        drop(ManagedChild { child, process_group, cleaned: false });

        sleep(Duration::from_millis(400)).await;
        assert!(!marker.exists(), "descendant in the worker process group must be killed");
        tokio::fs::remove_dir_all(directory).await.expect("remove test directory");
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
