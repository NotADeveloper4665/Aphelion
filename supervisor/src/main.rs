use aphelion_supervisor::{message_identity_matches, protocol_version_supported, Message, RestartPolicy, PROTOCOL_MAJOR, PROTOCOL_MINOR, MAX_MESSAGE_BYTES};
use serde_json::json;
use std::{env, os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
use nix::{sys::signal::{kill, Signal}, unistd::Pid};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, Command},
    time::timeout,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> Result<()> {
    let session_id = Uuid::new_v4().to_string();
    let runtime_dir = std::env::temp_dir().join(format!("aphelion-{}", Uuid::new_v4()));
    tokio::fs::create_dir(&runtime_dir).await?;
    tokio::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700)).await?;
    let socket_path = runtime_dir.join("worker.sock");
    let listener = UnixListener::bind(&socket_path)?;

    let executable = env::current_exe()?;
    let worker_executable = executable.parent().ok_or("supervisor has no parent directory")?.join("mock-worker");
    if !worker_executable.is_file() {
        return Err(format!("mock worker binary not found at {} (build the whole workspace)", worker_executable.display()).into());
    }

    let policy = RestartPolicy::default();
    let mut generation = 1_u64;
    let mut restarts = 0_u32;
    loop {
        let worker_id = Uuid::new_v4().to_string();
        println!("supervisor: starting worker generation {generation} ({worker_id})");
        let mut child = Command::new(&worker_executable)
            .arg("--socket").arg(&socket_path)
            .arg("--worker-id").arg(&worker_id)
            .arg("--generation").arg(generation.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;

        let result = run_demo(&listener, &mut child, &worker_id, generation, &session_id).await;
        let graceful_wait = if result.is_ok() {
            Duration::from_secs(2)
        } else {
            Duration::ZERO
        };
        let status = reap_worker(&mut child, graceful_wait).await?;
        println!("supervisor: worker generation {generation} exited with {status}");

        if result.is_ok() && status.success() {
            break;
        }

        let failure = result.err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| format!("worker exited unsuccessfully with {status}"));
        let Some(delay) = policy.delay_before_restart(restarts) else {
            let _ = tokio::fs::remove_file(&socket_path).await;
            let _ = tokio::fs::remove_dir(&runtime_dir).await;
            return Err(format!("worker restart budget exhausted after {restarts} retries: {failure}").into());
        };

        restarts += 1;
        generation = generation.checked_add(1).ok_or("worker generation exhausted")?;
        println!(
            "supervisor: worker failed ({failure}); retry {restarts}/{} in {} ms",
            policy.max_restarts,
            delay.as_millis()
        );
        tokio::time::sleep(delay).await;
    }

    let _ = tokio::fs::remove_file(&socket_path).await;
    let _ = tokio::fs::remove_dir(&runtime_dir).await;
    Ok(())
}

async fn run_demo(
    listener: &UnixListener,
    child: &mut Child,
    worker_id: &str,
    generation: u64,
    session_id: &str,
) -> Result<()> {
    let (stream, _) = timeout(Duration::from_secs(5), listener.accept()).await??;
    println!("supervisor: worker connected; validating handshake");
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let hello = read_message(&mut reader).await?;
    validate_identity(&hello, worker_id, generation)?;
    if hello.kind != "event" || hello.message_type != "worker.hello" {
        return Err("expected worker.hello event".into());
    }
    if !protocol_version_supported(&hello.protocol) {
        return Err("worker protocol version is incompatible".into());
    }
    if !matches!(hello.request_id.as_deref(), Some(id) if !id.is_empty()) {
        return Err("worker.hello requires a non-empty request_id".into());
    }
    send_message(&mut write, &Message::new(
        "response", "worker.hello_ack", worker_id, generation,
        hello.request_id.clone(), None,
        json!({"accepted": true, "protocol": {"major": PROTOCOL_MAJOR, "minor": PROTOCOL_MINOR}}),
    )).await?;
    println!("supervisor: handshake complete");

    let start_id = Uuid::new_v4().to_string();
    let start = Message::new("request", "session.start", worker_id, generation,
        Some(start_id.clone()), Some(session_id.to_owned()), json!({"audio_enabled": false}));
    send_message(&mut write, &start).await?;
    let started = read_message(&mut reader).await?;
    validate_response(&started, worker_id, generation, &start_id, "session.start")?;
    println!("supervisor: start_session => {}", started.body);

    let health_id = Uuid::new_v4().to_string();
    let health = Message::new("request", "worker.health", worker_id, generation,
        Some(health_id.clone()), None, json!({}));
    send_message(&mut write, &health).await?;
    let health_result = read_message(&mut reader).await?;
    validate_response(&health_result, worker_id, generation, &health_id, "worker.health")?;
    println!("supervisor: get_health => {}", health_result.body);

    let stop_id = Uuid::new_v4().to_string();
    let stop = Message::new("request", "session.stop", worker_id, generation,
        Some(stop_id.clone()), Some(session_id.to_owned()), json!({}));
    send_message(&mut write, &stop).await?;
    let stopped = read_message(&mut reader).await?;
    validate_response(&stopped, worker_id, generation, &stop_id, "session.stop")?;
    println!("supervisor: stop_session => {}", stopped.body);

    let shutdown_id = Uuid::new_v4().to_string();
    let shutdown = Message::new("request", "worker.shutdown", worker_id, generation,
        Some(shutdown_id.clone()), None, json!({"reason": "prototype_complete"}));
    send_message(&mut write, &shutdown).await?;
    let acknowledgement = read_message(&mut reader).await?;
    validate_response(&acknowledgement, worker_id, generation, &shutdown_id, "worker.shutdown")?;
    println!("supervisor: shutdown acknowledged; waiting for child exit");
    drop(write);
    drop(reader);
    let _ = child;
    Ok(())
}

async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Message> {
    let mut line = Vec::new();
    let mut limited = reader.take((MAX_MESSAGE_BYTES + 1) as u64);
    let count = timeout(Duration::from_secs(5), limited.read_until(b'\n', &mut line)).await??;
    if count == 0 { return Err("worker disconnected before replying".into()); }
    if count > MAX_MESSAGE_BYTES { return Err("IPC message exceeds size limit".into()); }
    if !line.ends_with(b"\n") { return Err("truncated IPC message (missing newline delimiter)".into()); }
    Ok(serde_json::from_slice(&line)?)
}

async fn send_message<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_MESSAGE_BYTES { return Err("outgoing IPC message exceeds size limit".into()); }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

fn validate_identity(message: &Message, worker_id: &str, generation: u64) -> Result<()> {
    if !message_identity_matches(message, worker_id, generation) {
        return Err("worker identity/generation mismatch".into());
    }
    Ok(())
}

fn validate_response(message: &Message, worker_id: &str, generation: u64, request_id: &str, message_type: &str) -> Result<()> {
    validate_identity(message, worker_id, generation)?;
    if message.kind != "response" || message.message_type != message_type || message.request_id.as_deref() != Some(request_id) {
        return Err(format!("unexpected or uncorrelated response for {message_type}").into());
    }
    if !protocol_version_supported(&message.protocol) {
        return Err("response protocol major version mismatch".into());
    }
    Ok(())
}


/// Reap the child on every path. After the requested graceful window, escalate
/// through SIGTERM and finally SIGKILL with bounded waits.
async fn reap_worker(child: &mut Child, graceful_wait: Duration) -> Result<std::process::ExitStatus> {
    if !graceful_wait.is_zero() {
        if let Ok(status) = timeout(graceful_wait, child.wait()).await {
            return Ok(status?);
        }
    }

    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    if let Ok(status) = timeout(Duration::from_secs(1), child.wait()).await {
        return Ok(status?);
    }

    child.kill().await?;
    Ok(child.wait().await?)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[tokio::test]
    async fn reaps_worker_that_exits_gracefully() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test child");

        let status = reap_worker(&mut child, Duration::from_secs(2))
            .await
            .expect("reap graceful child");
        assert!(status.success());
        assert!(child.id().is_none(), "child must be reaped");
    }

    #[tokio::test]
    async fn escalates_when_worker_ignores_sigterm() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; exec sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn TERM-ignoring test child");

        let status = reap_worker(&mut child, Duration::ZERO)
            .await
            .expect("reap forced child");
        assert!(!status.success(), "SIGKILL termination must be non-zero");
        assert!(child.id().is_none(), "child must be reaped after SIGKILL");
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod main_tests;
