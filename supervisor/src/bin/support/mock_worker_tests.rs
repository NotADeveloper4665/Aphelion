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
    let request = request("r1", "session.start", Some("s1"));
    let response = Message::new(
        "response",
        "session.start",
        "worker-a",
        2,
        Some("r1".to_owned()),
        Some("s1".to_owned()),
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
    let original = request("r1", "session.start", Some("s1"));
    let response = Message::new("response", "session.start", "worker-a", 2,
        Some("r1".to_owned()), Some("s1".to_owned()), json!({"created": true}));
    let conflicting = request("r1", "session.stop", Some("s1"));
    let mut cache = RequestCache::new(4);
    cache.insert(&original, &response).expect("cache request");

    assert!(cache.lookup(&conflicting).unwrap_err().to_string().contains("reused with different content"));
}

#[test]
fn request_cache_evicts_old_entries_at_its_bound() {
    let mut cache = RequestCache::new(1);
    let first = request("r1", "session.start", Some("s1"));
    let second = request("r2", "session.stop", Some("s1"));
    let response = |request: &Message| Message::new(
        "response", request.message_type.clone(), "worker-a", 2,
        request.request_id.clone(), request.session_id.clone(), json!({}));
    cache.insert(&first, &response(&first)).expect("insert first");
    cache.insert(&second, &response(&second)).expect("insert second");

    assert!(cache.lookup(&first).expect("lookup evicted request").is_none());
    assert!(cache.lookup(&second).expect("lookup retained request").is_some());
}

#[test]
fn starting_and_stopping_sessions_are_idempotent() {
    let mut sessions = HashSet::new();
    let first_start = start_session(&mut sessions, "s1");
    let repeated_start = start_session(&mut sessions, "s1");
    assert_eq!(first_start["created"], true);
    assert_eq!(repeated_start["created"], false);
    assert_eq!(sessions.len(), 1);

    let first_stop = stop_session(&mut sessions, "s1");
    let repeated_stop = stop_session(&mut sessions, "s1");
    assert_eq!(first_stop["was_active"], true);
    assert_eq!(repeated_stop["was_active"], false);
    assert!(sessions.is_empty());
}
