use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub initial_delay: Duration,
    pub maximum_delay: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            initial_delay: Duration::from_millis(250),
            maximum_delay: Duration::from_secs(4),
        }
    }
}

impl RestartPolicy {
    /// Return the delay before the next restart, where zero is the first retry.
    pub fn delay_before_restart(&self, restart_number: u32) -> Option<Duration> {
        if restart_number >= self.max_restarts {
            return None;
        }
        let multiplier = 1_u32.checked_shl(restart_number.min(31)).unwrap_or(u32::MAX);
        Some(self.initial_delay.saturating_mul(multiplier).min(self.maximum_delay))
    }
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub protocol: ProtocolVersion,
    pub kind: String,
    #[serde(rename = "type")]
    pub message_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub worker_instance_id: String,
    pub worker_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTransition {
    Started { session_id: String, newly_active: bool },
    Stopped { session_id: String, was_active: bool },
}

#[derive(Debug, Default)]
pub struct SessionRegistry {
    active: HashSet<String>,
}

impl SessionRegistry {
    /// Apply a worker-originated lifecycle event. Unknown event types are ignored
    /// so a compatible minor-version worker can add events without breaking replies.
    pub fn apply_event(&mut self, message: &Message) -> Result<Option<SessionTransition>, &'static str> {
        if message.kind != "event" {
            return Err("session lifecycle message must be an event");
        }
        let session_id = message.session_id.as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or("session lifecycle event requires a non-empty session_id")?
            .to_owned();
        match message.message_type.as_str() {
            "session.started" => {
                let newly_active = self.active.insert(session_id.clone());
                Ok(Some(SessionTransition::Started { session_id, newly_active }))
            }
            "session.stopped" => {
                let was_active = self.active.remove(&session_id);
                Ok(Some(SessionTransition::Stopped { session_id, was_active }))
            }
            _ => Ok(None),
        }
    }

    pub fn is_active(&self, session_id: &str) -> bool {
        self.active.contains(session_id)
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

pub const REQUIRED_WORKER_CAPABILITIES: [&str; 4] = [
    "session.start",
    "session.stop",
    "worker.health",
    "worker.shutdown",
];

pub fn missing_required_capabilities(capabilities: &Value) -> Vec<&'static str> {
    let Some(capabilities) = capabilities.as_array() else {
        return REQUIRED_WORKER_CAPABILITIES.to_vec();
    };
    REQUIRED_WORKER_CAPABILITIES
        .iter()
        .copied()
        .filter(|required| !capabilities.iter().any(|value| value.as_str() == Some(required)))
        .collect()
}

pub fn protocol_version_supported(version: &ProtocolVersion) -> bool {
    version.major == PROTOCOL_MAJOR && version.minor <= PROTOCOL_MINOR
}

pub fn message_identity_matches(message: &Message, worker_id: &str, generation: u64) -> bool {
    message.worker_instance_id == worker_id && message.worker_generation == generation
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionIntent {
    /// Stable display selector such as "default"; not a capture-backend name.
    pub display: String,
    pub video: VideoIntent,
    pub audio_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoIntent {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl SessionIntent {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.display.trim().is_empty() {
            return Err("display selector must not be empty");
        }
        if self.video.width == 0 || self.video.height == 0 || self.video.fps == 0 {
            return Err("video width, height, and fps must be non-zero");
        }
        Ok(())
    }
}

impl Message {
    pub fn new(
        kind: impl Into<String>,
        message_type: impl Into<String>,
        worker_instance_id: impl Into<String>,
        worker_generation: u64,
        request_id: Option<String>,
        session_id: Option<String>,
        body: Value,
    ) -> Self {
        Self {
            protocol: ProtocolVersion {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
            kind: kind.into(),
            message_type: message_type.into(),
            request_id,
            worker_instance_id: worker_instance_id.into(),
            worker_generation,
            session_id,
            body,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_uses_versioned_wire_envelope() {
        let message = Message::new(
            "request",
            "session.start",
            "worker-a",
            3,
            Some("request-a".to_owned()),
            Some("session-a".to_owned()),
            json!({"audio_enabled": false}),
        );

        let encoded = serde_json::to_value(&message).expect("message should serialize");
        assert_eq!(encoded["protocol"]["major"], PROTOCOL_MAJOR);
        assert_eq!(encoded["protocol"]["minor"], PROTOCOL_MINOR);
        assert_eq!(encoded["kind"], "request");
        assert_eq!(encoded["type"], "session.start");
        assert!(encoded.get("message_type").is_none());
        assert_eq!(encoded["worker_instance_id"], "worker-a");
        assert_eq!(encoded["worker_generation"], 3);
        assert_eq!(encoded["session_id"], "session-a");

        let decoded: Message =
            serde_json::from_value(encoded).expect("message should deserialize");
        assert_eq!(decoded.message_type, "session.start");
        assert_eq!(decoded.request_id.as_deref(), Some("request-a"));
    }

    #[test]
    fn worker_scoped_message_omits_session_identity() {
        let message = Message::new(
            "request",
            "worker.health",
            "worker-a",
            1,
            Some("request-b".to_owned()),
            None,
            json!({}),
        );

        let encoded = serde_json::to_value(message).expect("message should serialize");
        assert!(encoded.get("session_id").is_none());
    }

    #[test]
    fn protocol_compatibility_requires_same_major_and_supported_minor() {
        assert!(protocol_version_supported(&ProtocolVersion { major: PROTOCOL_MAJOR, minor: PROTOCOL_MINOR }));
        assert!(!protocol_version_supported(&ProtocolVersion { major: PROTOCOL_MAJOR + 1, minor: 0 }));
        assert!(!protocol_version_supported(&ProtocolVersion { major: PROTOCOL_MAJOR.saturating_sub(1), minor: 0 }));
        assert!(!protocol_version_supported(&ProtocolVersion { major: PROTOCOL_MAJOR, minor: PROTOCOL_MINOR + 1 }));
    }


    #[test]
    fn restart_policy_uses_capped_exponential_backoff_and_exhausts_budget() {
        let policy = RestartPolicy {
            max_restarts: 4,
            initial_delay: Duration::from_secs(1),
            maximum_delay: Duration::from_secs(2),
        };
        assert_eq!(policy.delay_before_restart(0), Some(Duration::from_secs(1)));
        assert_eq!(policy.delay_before_restart(1), Some(Duration::from_secs(2)));
        assert_eq!(policy.delay_before_restart(2), Some(Duration::from_secs(2)));
        assert_eq!(policy.delay_before_restart(3), Some(Duration::from_secs(2)));
        assert_eq!(policy.delay_before_restart(4), None);
    }



    #[test]
    fn worker_capability_handshake_requires_all_lifecycle_commands() {
        let available = serde_json::json!(["session.start", "session.stop", "worker.health", "worker.shutdown"]);
        assert!(missing_required_capabilities(&available).is_empty());

        let incomplete = serde_json::json!(["session.start", "worker.health"]);
        assert_eq!(
            missing_required_capabilities(&incomplete),
            vec!["session.stop", "worker.shutdown"]
        );
        assert_eq!(
            missing_required_capabilities(&serde_json::Value::Null),
            REQUIRED_WORKER_CAPABILITIES.to_vec()
        );
    }

    #[test]
    fn session_intent_is_process_agnostic_and_serializable() {
        let intent = SessionIntent {
            display: "default".to_owned(),
            video: VideoIntent { codec: VideoCodec::H264, width: 1920, height: 1080, fps: 60 },
            audio_enabled: true,
        };
        intent.validate().expect("valid stream intent");
        let value = serde_json::to_value(intent).expect("serialize stream intent");
        assert_eq!(value["display"], "default");
        assert_eq!(value["video"]["codec"], "h264");
        assert_eq!(value["video"]["width"], 1920);
        assert_eq!(value["video"]["height"], 1080);
        assert_eq!(value["video"]["fps"], 60);
        assert_eq!(value["audio_enabled"], true);
        for implementation_detail in ["encoder_backend", "capture_backend", "syzygy_class"] {
            assert!(value.get(implementation_detail).is_none());
        }
    }

    #[test]
    fn session_intent_rejects_missing_display_or_video_dimensions() {
        let mut intent = SessionIntent {
            display: "default".to_owned(),
            video: VideoIntent { codec: VideoCodec::Hevc, width: 1280, height: 720, fps: 60 },
            audio_enabled: false,
        };
        intent.validate().expect("valid intent");
        intent.display.clear();
        assert!(intent.validate().is_err());
        intent.display = "default".to_owned();
        intent.video.width = 0;
        assert!(intent.validate().is_err());
    }


    #[test]
    fn session_registry_tracks_worker_events_idempotently() {
        let mut sessions = SessionRegistry::default();
        let event = |kind, session_id| Message::new(
            "event", kind, "worker-a", 3, None, Some(session_id.to_owned()), json!({}));

        let started = sessions.apply_event(&event("session.started", "session-a"))
            .expect("valid event").expect("known event");
        assert_eq!(started, SessionTransition::Started {
            session_id: "session-a".to_owned(),
            newly_active: true,
        });
        assert!(sessions.is_active("session-a"));
        assert_eq!(sessions.active_count(), 1);

        let repeated = sessions.apply_event(&event("session.started", "session-a"))
            .expect("duplicate event").expect("known event");
        assert_eq!(repeated, SessionTransition::Started {
            session_id: "session-a".to_owned(),
            newly_active: false,
        });

        let stopped = sessions.apply_event(&event("session.stopped", "session-a"))
            .expect("stop event").expect("known event");
        assert_eq!(stopped, SessionTransition::Stopped {
            session_id: "session-a".to_owned(),
            was_active: true,
        });
        assert_eq!(sessions.active_count(), 0);
    }

    #[test]
    fn session_registry_rejects_missing_identity_and_ignores_unknown_events() {
        let mut sessions = SessionRegistry::default();
        let missing_id = Message::new("event", "session.started", "worker-a", 1, None, None, json!({}));
        assert!(sessions.apply_event(&missing_id).is_err());

        let unknown = Message::new("event", "worker.metrics", "worker-a", 1, None, None, json!({}));
        assert_eq!(sessions.apply_event(&unknown).expect("unknown compatible event"), None);
    }

    #[test]
    fn worker_identity_rejects_wrong_instance_and_stale_generation() {
        let message = Message::new(
            "event",
            "worker.hello",
            "worker-a",
            7,
            Some("hello-1".to_owned()),
            None,
            json!({}),
        );

        assert!(message_identity_matches(&message, "worker-a", 7));
        assert!(!message_identity_matches(&message, "worker-b", 7));
        assert!(!message_identity_matches(&message, "worker-a", 6));
        assert!(!message_identity_matches(&message, "worker-a", 8));
    }

}
