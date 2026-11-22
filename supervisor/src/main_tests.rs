use super::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

fn valid_response() -> Message {
    Message::new(
        "response",
        "host.start",
        "worker-a",
        7,
        Some("request-a".to_owned()),
        None,
        json!({"state": "process_running"}),
    )
}

#[test]
fn response_validation_rejects_incompatible_protocol_version() {
    let mut response = valid_response();
    response.protocol.major += 1;
    assert!(validate_response(&response, "worker-a", 7, "request-a", "host.start").is_err());
}

#[test]
fn response_validation_rejects_missing_request_id() {
    let mut response = valid_response();
    response.request_id = None;
    assert!(validate_response(&response, "worker-a", 7, "request-a", "host.start").is_err());
}

#[test]
fn response_validation_rejects_stale_worker_generation() {
    let response = valid_response();
    assert!(validate_response(&response, "worker-a", 6, "request-a", "host.start").is_err());
}

#[test]
fn response_validation_rejects_incorrect_response_type() {
    let response = valid_response();
    assert!(validate_response(&response, "worker-a", 7, "request-a", "worker.health").is_err());
}

#[test]
fn response_validation_rejects_uncorrelated_request_id() {
    let mut response = valid_response();
    response.request_id = Some("request-b".to_owned());
    assert!(validate_response(&response, "worker-a", 7, "request-a", "host.start").is_err());
}

async fn read_wire_message(bytes: Vec<u8>) -> Result<Message> {
    let (mut writer, reader) = tokio::io::duplex(MAX_MESSAGE_BYTES + 8);
    let writer_task = tokio::spawn(async move {
        let result = writer.write_all(&bytes).await;
        drop(writer);
        result
    });
    let mut reader = BufReader::new(reader);
    let result = read_message(&mut reader).await;
    writer_task.await??;
    result
}

#[tokio::test]
async fn ipc_rejects_malformed_json() {
    let error = read_wire_message(b"{not-json}\n".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("key must be a string"));
}

#[tokio::test]
async fn ipc_rejects_oversized_message() {
    let error = read_wire_message(vec![b'x'; MAX_MESSAGE_BYTES + 1]).await.unwrap_err();
    assert!(error.to_string().contains("exceeds size limit"));
}

#[tokio::test]
async fn ipc_rejects_truncated_message() {
    let error = read_wire_message(b"{\"protocol\":".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("truncated IPC message"));
}

#[tokio::test]
async fn ipc_reports_disconnected_peer() {
    let error = read_wire_message(Vec::new()).await.unwrap_err();
    assert!(error.to_string().contains("disconnected"));
}

#[tokio::test]
async fn reaps_worker_that_crashes_before_shutdown() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 0.05; exit 23")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crashing child");

    let status = reap_worker(&mut child, Duration::from_secs(1))
        .await
        .expect("reap crashed child");
    assert_eq!(status.code(), Some(23));
    assert!(child.id().is_none(), "crashed child must be reaped");
}
