use aphelion_supervisor::{message_identity_matches, protocol_version_supported, Message, MAX_MESSAGE_BYTES};
use serde_json::json;
use std::{collections::{HashMap, VecDeque}, env, path::PathBuf, time::Duration};
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
    let crash_on_start = args.iter().any(|arg| arg == "--crash-on-start");
    let stream = connect_with_retry(PathBuf::from(socket)).await?;
    println!("mock-worker: connected to supervisor");
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let hello_id = Uuid::new_v4().to_string();
    send(&mut write, &Message::new("event", "worker.hello", worker_id, generation,
        Some(hello_id.clone()), None,
        json!({"build": "mock-worker/0.1.0", "capabilities": ["host.start", "host.stop", "worker.health", "worker.shutdown"]}))).await?;
    let ack = read_message(&mut reader).await?;
    if ack.kind != "response" || ack.message_type != "worker.hello_ack" || ack.request_id.as_deref() != Some(hello_id.as_str())
        || !protocol_version_supported(&ack.protocol)
        || !message_identity_matches(&ack, worker_id, generation) {
        return Err("supervisor handshake acknowledgement was invalid".into());
    }
    println!("mock-worker: handshake accepted");

    let mut host_running = false;
    let mut request_cache = RequestCache::new(MAX_CACHED_REQUESTS);
    loop {
        let request = read_message(&mut reader).await?;
        if request.kind != "request" { return Err("worker expected request message".into()); }
        if !protocol_version_supported(&request.protocol) { return Err("unsupported request protocol version".into()); }
        if !matches!(request.request_id.as_deref(), Some(id) if !id.is_empty()) { return Err("request requires a non-empty request_id".into()); }
        if !message_identity_matches(&request, worker_id, generation) {
            return Err("request addressed to a different worker generation".into());
        }
        if let Some(cached_response) = request_cache.lookup(&request)? {
            send(&mut write, cached_response).await?;
            continue;
        }
        if crash_on_start && request.message_type == "host.start" {
            eprintln!("mock-worker: injected failure during host.start");
            std::process::exit(42);
        }
        if request.session_id.is_some() {
            return Err("host lifecycle requests must not include session_id".into());
        }
        let (body, should_exit) = match request.message_type.as_str() {
            "host.start" => (start_host(&mut host_running), false),
            "worker.health" => (
                json!({
                    "worker_state": "ready",
                    "host_running": host_running,
                    "active_sessions": 0,
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
            "host.stop" => (stop_host(&mut host_running), false),
            "worker.shutdown" => {
                host_running = false;
                (json!({"state": "shutting_down", "host_running": false}), true)
            }
            other => (json!({"error": "UNSUPPORTED_MESSAGE", "message": other}), false),
        };
        let response = Message::new("response", request.message_type.clone(), worker_id, generation,
            request.request_id.clone(), request.session_id.clone(), body);
        request_cache.insert(&request, &response)?;
        send(&mut write, &response).await?;
        if should_exit {
            println!("mock-worker: shutdown acknowledged; exiting");
            break;
        }
    }
    Ok(())
}

const MAX_CACHED_REQUESTS: usize = 128;

struct RequestCache {
    entries: HashMap<String, (Vec<u8>, Message)>,
    oldest_first: VecDeque<String>,
    capacity: usize,
}

impl RequestCache {
    fn new(capacity: usize) -> Self {
        Self { entries: HashMap::new(), oldest_first: VecDeque::new(), capacity }
    }

    fn lookup(&self, request: &Message) -> Result<Option<&Message>> {
        let request_id = request.request_id.as_deref().ok_or("request requires a non-empty request_id")?;
        let Some((fingerprint, response)) = self.entries.get(request_id) else {
            return Ok(None);
        };
        let encoded = serde_json::to_vec(request)?;
        if *fingerprint != encoded {
            return Err(format!("request_id {request_id} was reused with different content").into());
        }
        Ok(Some(response))
    }

    fn insert(&mut self, request: &Message, response: &Message) -> Result<()> {
        let request_id = request.request_id.as_deref().ok_or("request requires a non-empty request_id")?.to_owned();
        if self.capacity == 0 {
            return Ok(());
        }
        if !self.entries.contains_key(&request_id) {
            while self.entries.len() >= self.capacity {
                if let Some(oldest) = self.oldest_first.pop_front() {
                    self.entries.remove(&oldest);
                } else {
                    break;
                }
            }
            self.oldest_first.push_back(request_id.clone());
        }
        self.entries.insert(request_id, (serde_json::to_vec(request)?, response.clone()));
        Ok(())
    }
}

fn start_host(host_running: &mut bool) -> serde_json::Value {
    let already_running = *host_running;
    *host_running = true;
    json!({"state": "process_running", "already_running": already_running})
}

fn stop_host(host_running: &mut bool) -> serde_json::Value {
    let was_running = *host_running;
    *host_running = false;
    json!({"state": "stopped", "was_running": was_running})
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
    if !protocol_version_supported(&message.protocol) {
        return Err("unsupported protocol version".into());
    }
    let mut bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_MESSAGE_BYTES { return Err("IPC message exceeds size limit".into()); }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
#[path = "support/mock_worker_tests.rs"]
mod mock_worker_tests;
