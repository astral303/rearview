use crate::agent::refs::MessageRange;
use crate::agent::time_span::TimeSpan;
use crate::history::message_texts::{MessageStarts, MessageTextBuilder};
use crate::history::{Conversation, MessageTextStarts};
use crate::search::normalize_for_search;
use crate::time_filter::TimeFilter;
use chrono::{DateTime, Local, TimeZone};
use std::path::PathBuf;

/// `day` October 2026 at `hour`, local time.
pub fn october(day: u32, hour: u32) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
        .single()
        .expect("an unambiguous local time")
}

/// 10 October 2026 from 00:00 to 23:00.
pub fn window_on_october_10() -> TimeFilter {
    TimeFilter {
        after: Some(october(10, 0)),
        before: Some(october(10, 23)),
    }
}

/// A session titled `title`, with one message per `(text, time)` pair. The
/// session's time is its last message's.
pub fn timed_messages_conversation(
    messages: &[(&str, DateTime<Local>)],
    title: Option<&str>,
) -> Conversation {
    let texts = messages.iter().map(|(text, _)| *text).collect::<Vec<_>>();
    let (full_text, full_text_starts) = text_with_starts(title, &texts, messages.len());
    let spans = messages
        .iter()
        .map(|(_, time)| Some(TimeSpan::from_seconds(time.timestamp(), time.timestamp())))
        .collect::<Vec<_>>();
    let timestamp = messages.last().map_or_else(Local::now, |(_, time)| *time);
    Conversation {
        search_text_lower: normalize_for_search(&full_text),
        full_text,
        message_count: messages.len(),
        message_times: crate::history::MessageTimes::new(&spans),
        message_text_starts: MessageTextStarts {
            full_text: full_text_starts,
            ..MessageTextStarts::default()
        },
        custom_title: title.map(str::to_string),
        ..one_message_conversation("", timestamp, None, None, None)
    }
}

/// `conversation` with `subagent_texts` as its `agent_search_text` and
/// `thinking_and_tool_call_texts` as its `thinking_and_tool_call_text`, one
/// per message in order.
pub fn with_agent_texts(
    mut conversation: Conversation,
    subagent_texts: &[&str],
    thinking_and_tool_call_texts: &[&str],
) -> Conversation {
    let message_count = conversation.message_count;
    let starts = &mut conversation.message_text_starts;
    (conversation.agent_search_text, starts.agent_search_text) =
        text_with_starts(None, subagent_texts, message_count);
    (
        conversation.thinking_and_tool_call_text,
        starts.thinking_and_tool_call_text,
    ) = text_with_starts(None, thinking_and_tool_call_texts, message_count);
    conversation
}

/// `leading`, then `texts` as the text of messages m1, m2, …, and where each
/// message's text starts.
fn text_with_starts(
    leading: Option<&str>,
    texts: &[&str],
    message_count: usize,
) -> (String, MessageStarts) {
    let mut builder = MessageTextBuilder::default();
    for (index, text) in texts.iter().enumerate() {
        builder.push(Some(index + 1), (*text).to_owned());
    }
    builder.finish(leading, message_count, str::to_owned)
}

/// A session of one message, written at `timestamp`.
pub fn one_message_conversation(
    text: &str,
    timestamp: DateTime<Local>,
    summary: Option<&str>,
    title: Option<&str>,
    project: Option<&str>,
) -> Conversation {
    let mut builder = MessageTextBuilder::default();
    builder.push(Some(1), text.to_string());
    let (full_text, full_text_starts) =
        builder.finish(title.into_iter().chain(summary), 1, str::to_owned);

    Conversation {
        source: crate::history::Source::Claude,
        subagents: Vec::new(),
        session_id: String::new(),
        path: PathBuf::new(),
        index: 0,
        timestamp,
        preview: text.to_string(),
        preview_first: text.to_string(),
        preview_last: text.to_string(),
        full_text: full_text.clone(),
        agent_search_text: String::new(),
        thinking_and_tool_call_text: String::new(),
        semantic_route_text: String::new(),
        semantic_turns: vec![text.to_string()],
        semantic_turn_ranges: vec![MessageRange::single(1)],
        search_text_lower: normalize_for_search(&full_text),
        project_name: project.map(str::to_string),
        project_path: None,
        cwd: None,
        message_count: 1,
        message_times: crate::history::MessageTimes::new(&[Some(TimeSpan::from_seconds(
            timestamp.timestamp(),
            timestamp.timestamp(),
        ))]),
        message_text_starts: MessageTextStarts {
            full_text: full_text_starts,
            ..MessageTextStarts::default()
        },
        assistant_messages: 1,
        parse_errors: vec![],
        summary: summary.map(str::to_string),
        custom_title: title.map(str::to_string),
        model: None,
        total_tokens: 0,
        duration_minutes: None,
        fingerprint: None,
        has_transient_read_error: false,
        is_missing_earlier_turns: false,
        cached_session: None,
    }
}
