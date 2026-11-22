use super::*;
use serde_json::json;

fn request(request_id: &str, message_type: &str, session_id: Option<&str>) -> Message {
    Message::new(
        "request",
        message_type,
        "worker-a",
        2,
        Some(request_id.to_owned()),
        session_id.map(str::to_owned),
        json!({}),
    )
}

#[test]
fn duplicate_request_id_replays_original_response() {
    let request = request("r1", "host.start", None);
    let response = Message::new(
        "response",
        "host.start",
        "worker-a",
        2,
        Some("r1".to_owned()),
        None,
        json!({"created": true}),
    );
    let mut cache = RequestCache::new(4);
    cache.insert(&request, &response).expect("cache request");

    let replay = cache.lookup(&request).expect("lookup duplicate").expect("cached response");
    assert_eq!(replay.body, response.body);
    assert_eq!(replay.request_id, response.request_id);
}

#[test]
fn reusing_request_id_with_different_content_is_rejected() {
    let original = request("r1", "host.start", None);
    let response = Message::new("response", "host.start", "worker-a", 2,
        Some("r1".to_owned()), None, json!({"created": true}));
    let conflicting = request("r1", "host.stop", None);
    let mut cache = RequestCache::new(4);
    cache.insert(&original, &response).expect("cache request");

    assert!(cache.lookup(&conflicting).unwrap_err().to_string().contains("reused with different content"));
}

#[test]
fn request_cache_evicts_old_entries_at_its_bound() {
    let mut cache = RequestCache::new(1);
    let first = request("r1", "host.start", None);
    let second = request("r2", "host.stop", None);
    let response = |request: &Message| Message::new(
        "response", request.message_type.clone(), "worker-a", 2,
        request.request_id.clone(), request.session_id.clone(), json!({}));
    cache.insert(&first, &response(&first)).expect("insert first");
    cache.insert(&second, &response(&second)).expect("insert second");

    assert!(cache.lookup(&first).expect("lookup evicted request").is_none());
    assert!(cache.lookup(&second).expect("lookup retained request").is_some());
}

#[test]
fn starting_and_stopping_host_are_idempotent() {
    let mut host_running = false;
    let mut host_started_at: Option<Instant> = None;
    let first_start = start_host(&mut host_running);
    let repeated_start = start_host(&mut host_running);
    assert_eq!(first_start["already_running"], false);
    assert_eq!(repeated_start["already_running"], true);
    assert!(host_running);

    let first_stop = stop_host(&mut host_running);
    let repeated_stop = stop_host(&mut host_running);
    assert_eq!(first_stop["was_running"], true);
    assert_eq!(repeated_stop["was_running"], false);
    assert!(!host_running);
}
