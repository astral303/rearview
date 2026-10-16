use crate::agent::protocol::ProtocolOptions;
use crate::agent::refs::{AgentConversationKey, ResolvedConversation};
use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentPartSource, AgentTranscript,
    agent_part_text,
};
use crate::history::{Conversation, Source};
use crate::time_filter::TimeFilter;
use std::path::{Path, PathBuf};

/// Asserts that `row` counts `count` messages, as `transcript` numbers them,
/// and that each of `markers` sits in a semantic turn carrying the ordinal of
/// its transcript message.
pub fn assert_row_numbers_messages_as_transcript(
    row: &Conversation,
    transcript: &AgentTranscript,
    count: usize,
    markers: &[&str],
) {
    assert_eq!(
        (row.message_count, transcript.messages.len()),
        (count, count),
        "row and transcript message counts"
    );
    for marker in markers {
        let ordinal = transcript
            .messages
            .iter()
            .find(|message| {
                message
                    .parts
                    .iter()
                    .filter_map(agent_part_text)
                    .any(|text| text.contains(marker))
            })
            .unwrap_or_else(|| panic!("the transcript holds {marker:?}"))
            .ordinal;
        let turn = row
            .semantic_turns
            .iter()
            .position(|turn| turn.contains(marker))
            .unwrap_or_else(|| panic!("the row embeds {marker:?}"));
        assert_eq!(row.semantic_turn_ranges[turn].start, ordinal, "{marker}");
    }
}

/// [`assert_row_numbers_messages_as_transcript`] for the session at `path`,
/// read by `source`'s format and by `agent read`.
pub fn assert_row_numbers_messages_as_agent_read(
    source: Source,
    path: &Path,
    count: usize,
    markers: &[&str],
) {
    let row = source
        .provider()
        .format()
        .parse_conversation(path, None, None)
        .unwrap()
        .expect("the session is listed");
    let transcript = AgentTranscript::load_owned(source, path, &[]).unwrap();
    assert_row_numbers_messages_as_transcript(&row, &transcript, count, markers);
}

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
        jsonl_line: ordinal,
        assistant_message_id: None,
        parent_tool_use_id: None,
        parts: vec![AgentMessagePart::Text {
            text: text.to_string(),
            source: source(role),
        }],
    }
}

/// [`text_message`] with its part written at `timestamp`.
pub fn text_message_at(
    ordinal: usize,
    role: AgentMessageRole,
    text: &str,
    timestamp: &str,
) -> AgentMessage {
    AgentMessage {
        parts: vec![AgentMessagePart::Text {
            text: text.to_string(),
            source: AgentPartSource {
                timestamp: Some(timestamp.to_owned()),
                ..source(role)
            },
        }],
        ..text_message(ordinal, role, text)
    }
}

/// `timestamp` as `agent` output prints it: in local time, to the second,
/// with its offset.
pub fn local_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// The value of a record line's `name=` atom.
pub fn atom<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
}

pub type RecordTimes = (
    chrono::DateTime<chrono::FixedOffset>,
    chrono::DateTime<chrono::FixedOffset>,
);

/// A record line's `at=` time, and its `to=` time or else its `at=` time.
pub fn times(line: &str) -> Option<RecordTimes> {
    let time =
        |name| atom(line, name).map(|time| chrono::DateTime::parse_from_rfc3339(time).unwrap());
    let at = time("at")?;
    Some((at, time("to").unwrap_or(at)))
}

/// The first and last time of the messages an `agent read` output shows.
pub fn read_times(read: &str) -> Option<RecordTimes> {
    read.lines()
        .filter(|line| line.starts_with("message m"))
        .filter_map(times)
        .reduce(|(first, last), (at, to)| (first.min(at), last.max(to)))
}

/// Every part shown, without a budget or a time window.
pub const EVERY_PART: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: true,
    thinking: true,
    subagents: true,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

/// `row` resolved under the project `project`.
pub fn resolved_conversation(row: &Conversation) -> ResolvedConversation {
    let file_name = row.path.file_name().unwrap().to_string_lossy();
    let key = AgentConversationKey::new("project", file_name, row.path.clone());
    ResolvedConversation {
        reference: key.conversation_ref(),
        key,
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
