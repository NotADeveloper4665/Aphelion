use aphelion_supervisor::{
    protocol_version_supported, Message, SessionIntent, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    MAX_MESSAGE_BYTES,
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

    let mut child: Option<Child> = None;
    let mut managed_session: Option<String> = None;
    let mut last_exit: Option<Value> = None;

    loop {
        if let Some(current) = child.as_mut() {
            if let Some(status) = current.try_wait()? {
                last_exit = Some(json!({
                    "success": status.success(),
                    "code": status.code(),
                    "signal": status.signal(),
                }));
                child = None;
                managed_session = None;
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
                        "process_id": child.as_ref().and_then(Child::id),
                    })
                } else {
                    let spawned = spawn_syzygy()?;
                    let process_id = spawned.id();
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
                send_response(
                    &writer,
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
                    match process.try_wait()? {
                        None => true,
                        Some(status) => {
                            last_exit = Some(json!({
                                "success": status.success(),
                                "code": status.code(),
                                "signal": status.signal(),
                            }));
                            child = None;
                            managed_session = None;
                            false
                        }
                    }
                } else {
                    false
                };
                send_response(
                    &writer,
                    &request,
                    &worker_id,
                    generation,
                    json!({
                        "state": if running { "process_running" } else { "stopped" },
                        "process_running": running,
                        "process_id": child.as_ref().and_then(Child::id),
                        "managed_session": managed_session,
                        "last_exit": last_exit,
                        "stream_ready": false,
                        "client_session_count": null,
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
                    &writer,
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
                    &writer,
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

fn spawn_syzygy() -> Result<Child> {
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

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain_output(stdout, "stdout"));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(drain_output(stderr, "stderr"));
    }
    Ok(child)
}

async fn drain_output<R: AsyncRead + Unpin>(stream: R, label: &'static str) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => eprintln!("syzygy[{label}]: {line}"),
            Ok(None) => break,
            Err(error) => {
                eprintln!("syzygy-worker: failed reading child {label}: {error}");
                break;
            }
        }
    }
}

async fn stop_process(child: &mut Child) -> Result<()> {
    if let Some(pid) = child.id() {
        let group = Pid::from_raw(pid as i32);
        let _ = killpg(group, Signal::SIGTERM);
        if timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
            let _ = killpg(group, Signal::SIGKILL);
            child.kill().await?;
        } else {
            // The leader exited; remove any helper descendants left in its group.
            let _ = killpg(group, Signal::SIGKILL);
        }
    } else {
        let _ = child.wait().await?;
    }
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

fn argument<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    let index = args.iter().position(|arg| arg == name)
        .ok_or_else(|| format!("missing {name}"))?;
    args.get(index + 1).map(String::as_str)
        .ok_or_else(|| format!("missing value for {name}").into())
}
