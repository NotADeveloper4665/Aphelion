use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

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
