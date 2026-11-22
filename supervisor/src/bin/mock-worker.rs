use aphelion_supervisor::{Message, PROTOCOL_MAJOR, PROTOCOL_MINOR, MAX_MESSAGE_BYTES};
use serde_json::json;
use std::{collections::HashSet, env, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::sleep,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let socket = arg_value(&args, "--socket")?;
    let worker_id = arg_value(&args, "--worker-id")?;
    let generation: u64 = arg_value(&args, "--generation")?.parse()?;
    let stream = connect_with_retry(PathBuf::from(socket)).await?;
    println!("mock-worker: connected to supervisor");
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let hello_id = Uuid::new_v4().to_string();
    send(&mut write, &Message::new("event", "worker.hello", worker_id, generation,
        Some(hello_id.clone()), None,
        json!({"build": "mock-worker/0.1.0", "capabilities": ["session.start", "session.stop", "worker.health"]}))).await?;
    let ack = read_message(&mut reader).await?;
    if ack.message_type != "worker.hello_ack" || ack.request_id.as_deref() != Some(hello_id.as_str()) {
        return Err("supervisor handshake acknowledgement was invalid".into());
    }
    println!("mock-worker: handshake accepted");

    let mut active_sessions = HashSet::<String>::new();
    loop {
        let request = read_message(&mut reader).await?;
        if request.kind != "request" { return Err("worker expected request message".into()); }
        if request.worker_instance_id != worker_id || request.worker_generation != generation {
            return Err("request addressed to a different worker generation".into());
        }
        let session_id = request.session_id.clone();
        let (body, should_exit) = match request.message_type.as_str() {
            "session.start" => {
                let sid = session_id.as_deref().ok_or("session.start requires session_id")?;
                let inserted = active_sessions.insert(sid.to_owned());
                (json!({"state": "running", "created": inserted, "active_sessions": active_sessions.len()}), false)
            }
            "worker.health" => (
                json!({
                    "worker_state": "ready",
                    "active_sessions": active_sessions.len(),
                    "sessions": active_sessions.iter().collect::<Vec<_>>(),
                    "subsystems": {
                        "capture": {"state": "not_implemented"},
                        "encoder": {"state": "not_implemented"},
                        "audio": {"state": "disabled_in_prototype"},
                        "input": {"state": "not_implemented"},
                        "network": {"state": "not_implemented"}
                    },
                    "metrics": null
                }), false
            ),
            "session.stop" => {
                let sid = session_id.as_deref().ok_or("session.stop requires session_id")?;
                let removed = active_sessions.remove(sid);
                (json!({"state": "stopped", "was_active": removed, "active_sessions": active_sessions.len()}), false)
            }
            "worker.shutdown" => {
                active_sessions.clear();
                (json!({"state": "shutting_down", "active_sessions": 0}), true)
            }
            other => (json!({"error": "UNSUPPORTED_MESSAGE", "message": other}), false),
        };
        let response = Message::new("response", request.message_type, worker_id, generation,
            request.request_id, session_id, body);
        send(&mut write, &response).await?;
        if should_exit {
            println!("mock-worker: shutdown acknowledged; exiting");
            break;
        }
    }
    Ok(())
}

fn arg_value<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    let index = args.iter().position(|arg| arg == name).ok_or_else(|| format!("missing {name}"))?;
    args.get(index + 1).map(String::as_str).ok_or_else(|| format!("missing value for {name}").into())
}

async fn connect_with_retry(path: PathBuf) -> Result<UnixStream> {
    for _ in 0..50 {
        match UnixStream::connect(&path).await {
            Ok(stream) => return Ok(stream),
            Err(_) => sleep(Duration::from_millis(100)).await,
        }
    }
    Err(format!("could not connect to supervisor socket {}", path.display()).into())
}

async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Message> {
    let mut line = Vec::new();
    let mut limited = reader.take((MAX_MESSAGE_BYTES + 1) as u64);
    let count = limited.read_until(b'\n', &mut line).await?;
    if count == 0 { return Err("supervisor disconnected".into()); }
    if count > MAX_MESSAGE_BYTES { return Err("IPC message exceeds size limit".into()); }
    if !line.ends_with(b"\n") { return Err("truncated IPC message (missing newline delimiter)".into()); }
    Ok(serde_json::from_slice(&line)?)
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    if message.protocol.major != PROTOCOL_MAJOR || message.protocol.minor > PROTOCOL_MINOR {
        return Err("unsupported protocol version".into());
    }
    let mut bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_MESSAGE_BYTES { return Err("IPC message exceeds size limit".into()); }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
