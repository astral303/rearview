use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentPartSource, AgentTranscript,
};
use std::path::PathBuf;

pub fn source(role: AgentMessageRole) -> AgentPartSource {
    AgentPartSource {
        role,
        timestamp: None,
        jsonl_line: 1,
        part_index: 0,
        assistant_message_id: None,
        parent_tool_use_id: None,
        tool_name: None,
    }
}

pub fn text_message(ordinal: usize, role: AgentMessageRole, text: &str) -> AgentMessage {
    AgentMessage {
        ordinal,
        role,
        timestamp: None,
        jsonl_line: ordinal,
        assistant_message_id: None,
        parent_tool_use_id: None,
        parts: vec![AgentMessagePart::Text {
            text: text.to_string(),
            source: source(role),
        }],
    }
}

pub fn user_jsonl_line(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": "2024-01-01T00:00:00Z",
        "message": {"role": "user", "content": text}
    })
    .to_string()
}

pub fn assistant_jsonl_line(text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": "2024-01-01T00:00:01Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
    })
    .to_string()
}

pub fn transcript(messages: Vec<AgentMessage>, path: &str) -> AgentTranscript {
    AgentTranscript {
        path: PathBuf::from(path),
        messages,
        malformed_lines: Vec::new(),
        summary: None,
        custom_title: None,
    }
}
