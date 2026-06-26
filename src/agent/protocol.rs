use crate::agent::refs::{MessageRange, ResolvedConversation};
use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentTranscript, MAX_AGENT_SEGMENT_CHARS,
    bounded_tool_summary,
};
use crate::error::{AppError, Result};
use serde_json::Value;
use std::collections::BTreeSet;

const OUTLINE_SHORT_MESSAGE_LIMIT: usize = 20;
const OUTLINE_SEGMENT_SIZE: usize = 10;
const SNIPPET_LIMIT: usize = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolOptions {
    pub budget: Option<usize>,
    pub tools: bool,
    pub tool_results: bool,
    pub thinking: bool,
    pub subagents: bool,
}

#[derive(Clone, Debug)]
pub struct ReadRequest<'a> {
    pub resolved: &'a ResolvedConversation,
    pub transcript: &'a AgentTranscript,
    pub range: Option<MessageRange>,
}

#[derive(Clone, Debug)]
pub struct ProtocolFocus {
    pub conversation_full_ref: Option<String>,
    pub range: MessageRange,
}

#[derive(Clone, Debug)]
struct RenderedMessage<'a> {
    conversation: &'a ResolvedConversation,
    message: &'a AgentMessage,
    body: String,
}

pub fn format_read(
    requests: &[ReadRequest<'_>],
    focus: Option<ProtocolFocus>,
    options: ProtocolOptions,
) -> Result<String> {
    let mut messages = Vec::new();
    for request in requests {
        let selected = selected_messages(request.transcript, request.range)?;
        for message in selected {
            if let Some(rendered) = render_message(request.resolved, message, options) {
                messages.push(rendered);
            }
        }
    }

    let selected = select_for_budget(&messages, focus, options.budget);
    let cut = cut_marker(messages.len(), &selected);
    let mut output = String::new();
    output.push_str(&format!(
        "protocol agent-read v=1 cut={} budget={}\n",
        escape_atom(&cut),
        budget_atom(options.budget)
    ));

    let mut last_ref: Option<String> = None;
    render_selected_messages(&mut output, &messages, &selected, &mut last_ref);
    Ok(output)
}

pub fn format_outline(
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    options: ProtocolOptions,
) -> String {
    let visible: Vec<_> = transcript
        .messages
        .iter()
        .filter_map(|message| render_message(resolved, message, options))
        .collect();
    let mut output = String::new();
    output.push_str(&format!(
        "protocol agent-outline v=1 cut=none budget={}\n",
        budget_atom(options.budget)
    ));
    output.push_str(&format!(
        "conversation uuid={} ref={} path={}\n",
        escape_atom(&resolved.reference.uuid()),
        escape_atom(&resolved.reference.canonical()),
        escape_atom(&resolved.key.session_filename)
    ));

    if visible.len() <= OUTLINE_SHORT_MESSAGE_LIMIT {
        for rendered in visible {
            output.push_str(&format!(
                "m{} role={} c~{} {}\n",
                rendered.message.ordinal,
                role_atom(rendered.message.role),
                rendered.body.chars().count(),
                snippet(&rendered.body)
            ));
        }
    } else {
        for chunk in visible.chunks(OUTLINE_SEGMENT_SIZE) {
            let first = chunk.first().expect("chunk is non-empty");
            let last = chunk.last().expect("chunk is non-empty");
            let count: usize = chunk
                .iter()
                .map(|message| message.body.chars().count())
                .sum();
            output.push_str(&format!(
                "seg m{}..m{} c~{} {} / {}\n",
                first.message.ordinal,
                last.message.ordinal,
                count,
                snippet(&first.body),
                snippet(&last.body)
            ));
        }
    }

    if let Some(budget) = options.budget
        && output.chars().count() > budget
    {
        let mut truncated = String::new();
        truncated.push_str(&format!(
            "protocol agent-outline v=1 cut=tail budget={}\n",
            budget
        ));
        for line in output.lines().skip(1) {
            if truncated.chars().count() + line.chars().count() + 1 > budget {
                break;
            }
            truncated.push_str(line);
            truncated.push('\n');
        }
        return truncated;
    }

    output
}

pub fn escape_atom(value: &str) -> String {
    let mut escaped = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'/'
            | b'+'
            | b'-' => escaped.push(byte as char),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

fn selected_messages(
    transcript: &AgentTranscript,
    range: Option<MessageRange>,
) -> Result<Vec<&AgentMessage>> {
    let Some(range) = range else {
        return Ok(transcript.messages.iter().collect());
    };
    let max = transcript.messages.len();
    if range.end > max {
        return Err(AppError::ConfigError(format!(
            "message range m{}..m{} exceeds transcript length m{}",
            range.start, range.end, max
        )));
    }
    Ok(transcript
        .messages
        .iter()
        .filter(|message| range.start <= message.ordinal && message.ordinal <= range.end)
        .collect())
}

fn render_message<'a>(
    conversation: &'a ResolvedConversation,
    message: &'a AgentMessage,
    options: ProtocolOptions,
) -> Option<RenderedMessage<'a>> {
    if message.parent_tool_use_id.is_some() && !options.subagents {
        return None;
    }
    let mut parts = Vec::new();
    for part in &message.parts {
        match part {
            AgentMessagePart::Text { text, .. } => parts.push(text.clone()),
            AgentMessagePart::ToolUse { name, input, .. } if options.tools => {
                parts.push(bounded_tool_summary(name, input, MAX_AGENT_SEGMENT_CHARS));
            }
            AgentMessagePart::ToolResult {
                content: Some(content),
                ..
            } if options.tool_results => {
                parts.push(tool_result_text(content));
            }
            AgentMessagePart::Thinking { thinking, .. } if options.thinking => {
                parts.push(format!("thinking: {thinking}"));
            }
            _ => {}
        }
    }
    let body = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!body.is_empty()).then_some(RenderedMessage {
        conversation,
        message,
        body,
    })
}

/// Try to add `candidate` to `selected`. If the rendered output would exceed
/// the budget, revert the insertion and return `false`. Otherwise return `true`.
fn try_expand(
    selected: &mut BTreeSet<usize>,
    candidate: usize,
    messages: &[RenderedMessage<'_>],
    budget: usize,
) -> bool {
    selected.insert(candidate);
    if rendered_len(
        messages,
        &selected.iter().copied().collect::<Vec<_>>(),
        "head+focus+tail",
        budget,
    )
    .chars()
    .count()
        > budget
    {
        selected.remove(&candidate);
        false
    } else {
        true
    }
}

fn select_for_budget(
    messages: &[RenderedMessage<'_>],
    focus: Option<ProtocolFocus>,
    budget: Option<usize>,
) -> Vec<usize> {
    let all: Vec<usize> = (0..messages.len()).collect();
    let Some(budget) = budget else {
        return all;
    };
    if rendered_len(messages, &all, "none", budget).chars().count() <= budget {
        return all;
    }

    let mut selected = BTreeSet::new();
    if let Some(focus) = focus {
        for (index, rendered) in messages.iter().enumerate() {
            let conversation_matches = focus
                .conversation_full_ref
                .as_deref()
                .is_none_or(|target| rendered.conversation.reference.full_ref() == target);
            if conversation_matches
                && focus.range.start <= rendered.message.ordinal
                && rendered.message.ordinal <= focus.range.end
            {
                selected.insert(index);
            }
        }
    }
    if selected.is_empty() && !messages.is_empty() {
        selected.insert(0);
        if rendered_len(
            messages,
            &selected.iter().copied().collect::<Vec<_>>(),
            "tail",
            budget,
        )
        .chars()
        .count()
            > budget
        {
            selected.clear();
        }
    }

    loop {
        let mut changed = false;
        let current: Vec<usize> = selected.iter().copied().collect();
        if let Some(first) = current.first().copied()
            && first > 0
        {
            let candidate = first - 1;
            if try_expand(&mut selected, candidate, messages, budget) {
                changed = true;
            }
        }
        let current: Vec<usize> = selected.iter().copied().collect();
        if let Some(last) = current.last().copied()
            && last + 1 < messages.len()
        {
            let candidate = last + 1;
            if try_expand(&mut selected, candidate, messages, budget) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    selected.into_iter().collect()
}

fn rendered_len(
    messages: &[RenderedMessage<'_>],
    selected: &[usize],
    cut: &str,
    budget: usize,
) -> String {
    let mut output = format!("protocol agent-read v=1 cut={cut} budget={budget}\n");
    let mut last_ref: Option<String> = None;
    render_selected_messages(&mut output, messages, selected, &mut last_ref);
    output
}

fn render_selected_messages(
    output: &mut String,
    messages: &[RenderedMessage<'_>],
    selected: &[usize],
    last_ref: &mut Option<String>,
) {
    for index in selected {
        let rendered = &messages[*index];
        let canonical = rendered.conversation.reference.canonical();
        if last_ref.as_deref() != Some(canonical.as_str()) {
            output.push_str(&format!(
                "conversation uuid={} ref={} path={}\n",
                escape_atom(&rendered.conversation.reference.uuid()),
                escape_atom(&canonical),
                escape_atom(&rendered.conversation.key.session_filename)
            ));
            *last_ref = Some(canonical);
        }
        output.push_str(&format!(
            "message m{} role={} line={}\n",
            rendered.message.ordinal,
            role_atom(rendered.message.role),
            rendered.message.jsonl_line
        ));
        push_body(output, &rendered.body);
    }
}

fn cut_marker(total: usize, selected: &[usize]) -> String {
    if selected.len() == total {
        return "none".to_string();
    }
    let Some(first) = selected.first().copied() else {
        return "tail".to_string();
    };
    let last = selected.last().copied().unwrap_or(first);
    let mut parts = Vec::new();
    if first > 0 {
        parts.push("head");
    }
    if selected.windows(2).any(|pair| pair[1] != pair[0] + 1) || (first > 0 && last + 1 < total) {
        parts.push("focus");
    }
    if last + 1 < total {
        parts.push("tail");
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join("+")
    }
}

fn push_body(output: &mut String, body: &str) {
    for line in body.split('\n') {
        if line.is_empty() {
            output.push_str("|\n");
        } else {
            output.push_str("| ");
            output.push_str(line);
            output.push('\n');
        }
    }
}

fn role_atom(role: AgentMessageRole) -> &'static str {
    match role {
        AgentMessageRole::User => "user",
        AgentMessageRole::Assistant => "assistant",
    }
}

fn budget_atom(budget: Option<usize>) -> String {
    budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => content.to_string(),
    }
}

fn snippet(body: &str) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= SNIPPET_LIMIT {
        normalized
    } else {
        normalized.chars().take(SNIPPET_LIMIT).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::refs::{AgentConversationKey, AgentConversationRef};
    use crate::agent::test_support::{source, text_message};
    use serde_json::json;
    use std::path::PathBuf;

    fn resolved(filename: &str) -> ResolvedConversation {
        let key =
            AgentConversationKey::new("project with space", filename, PathBuf::from(filename));
        ResolvedConversation {
            reference: AgentConversationRef::from_parts("project with space", filename),
            key,
        }
    }

    fn transcript(messages: Vec<AgentMessage>) -> AgentTranscript {
        crate::agent::test_support::transcript(messages, "test.jsonl")
    }

    fn options() -> ProtocolOptions {
        ProtocolOptions {
            budget: Some(6000),
            tools: false,
            tool_results: false,
            thinking: false,
            subagents: false,
        }
    }

    #[test]
    fn escapes_header_atom_delimiters() {
        assert_eq!(escape_atom("a b%c=d|e\tf\ng"), "a%20b%25c%3Dd%7Ce%09f%0Ag");
    }

    #[test]
    fn read_defaults_hide_non_text_parts_and_frame_body_lines() {
        let resolved = resolved("session file.jsonl");
        let mut message = text_message(1, AgentMessageRole::Assistant, "hello\nprotocol fake");
        message.parts.push(AgentMessagePart::ToolUse {
            id: "toolu_1".to_string(),
            name: "Bash".to_string(),
            input: json!({"command": "pwd"}),
            source: source(AgentMessageRole::Assistant),
        });
        message.parts.push(AgentMessagePart::Thinking {
            thinking: "secret".to_string(),
            source: source(AgentMessageRole::Assistant),
        });
        let transcript = transcript(vec![message]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            options(),
        )
        .unwrap();

        assert!(output.starts_with("protocol agent-read v=1 cut=none budget=6000\n"));
        assert!(output.contains("path=session%20file.jsonl"));
        assert!(output.contains("| hello\n| protocol fake\n"));
        assert!(!output.contains("pwd"));
        assert!(!output.contains("secret"));
        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn read_preserves_focus_when_budget_truncates() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(
            (1..=7)
                .map(|index| {
                    text_message(
                        index,
                        AgentMessageRole::User,
                        &format!("message {index} with padding padding padding"),
                    )
                })
                .collect(),
        );
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange { start: 1, end: 7 }),
            }],
            Some(ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange::single(4),
            }),
            ProtocolOptions {
                budget: Some(260),
                ..options()
            },
        )
        .unwrap();

        assert!(output.starts_with("protocol agent-read v=1 cut=head+focus+tail budget=260\n"));
        assert!(output.contains("message m4 role=user"));
        assert!(output.contains("message 4 with padding"));
    }

    #[test]
    fn no_budget_read_emits_full_output_with_no_cut() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "one"),
            text_message(2, AgentMessageRole::Assistant, "two"),
        ]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            ProtocolOptions {
                budget: None,
                ..options()
            },
        )
        .unwrap();

        assert!(output.starts_with("protocol agent-read v=1 cut=none budget=none\n"));
        assert!(output.contains("message m1 role=user"));
        assert!(output.contains("message m2 role=assistant"));
    }

    #[test]
    fn non_focused_oversized_read_respects_budget_with_header_only_cut() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![text_message(
            1,
            AgentMessageRole::User,
            &"x".repeat(500),
        )]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            ProtocolOptions {
                budget: Some(80),
                ..options()
            },
        )
        .unwrap();

        assert_eq!(output, "protocol agent-read v=1 cut=tail budget=80\n");
    }

    #[test]
    fn qualified_focus_only_matches_target_conversation() {
        let first = resolved("first.jsonl");
        let second = resolved("second.jsonl");
        let first_transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "first one padding padding"),
            text_message(2, AgentMessageRole::User, "first two padding padding"),
        ]);
        let second_transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "second one padding padding"),
            text_message(2, AgentMessageRole::User, "second two padding padding"),
        ]);
        let output = format_read(
            &[
                ReadRequest {
                    resolved: &first,
                    transcript: &first_transcript,
                    range: None,
                },
                ReadRequest {
                    resolved: &second,
                    transcript: &second_transcript,
                    range: None,
                },
            ],
            Some(ProtocolFocus {
                conversation_full_ref: Some(second.reference.full_ref()),
                range: MessageRange::single(2),
            }),
            ProtocolOptions {
                budget: Some(190),
                ..options()
            },
        )
        .unwrap();

        assert!(output.contains("second two padding padding"));
        assert!(!output.contains("first two padding padding"));
    }

    #[test]
    fn snippet_truncates_to_eighty_characters() {
        assert_eq!(snippet(&"a".repeat(100)).chars().count(), SNIPPET_LIMIT);
    }

    #[test]
    fn subagent_messages_are_hidden_by_default_and_visible_with_option() {
        let resolved = resolved("session.jsonl");
        let mut subagent = text_message(2, AgentMessageRole::Assistant, "subagent hidden text");
        subagent.parent_tool_use_id = Some("agent-abcdef".to_string());
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "question"),
            subagent,
            text_message(3, AgentMessageRole::Assistant, "answer"),
        ]);

        let hidden = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            options(),
        )
        .unwrap();
        let visible = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            ProtocolOptions {
                subagents: true,
                ..options()
            },
        )
        .unwrap();

        assert!(!hidden.contains("subagent hidden text"));
        assert!(visible.contains("message m2 role=assistant"));
        assert!(visible.contains("subagent hidden text"));
    }

    #[test]
    fn short_outline_emits_one_line_per_message() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "one"),
            text_message(2, AgentMessageRole::Assistant, "two"),
        ]);
        let output = format_outline(&resolved, &transcript, options());

        assert!(output.contains("m1 role=user c~3 one\n"));
        assert!(output.contains("m2 role=assistant c~3 two\n"));
        assert!(!output.contains("seg "));
    }

    #[test]
    fn long_outline_emits_deterministic_segments() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(
            (1..=21)
                .map(|index| {
                    text_message(index, AgentMessageRole::User, &format!("message {index}"))
                })
                .collect(),
        );
        let output = format_outline(&resolved, &transcript, options());

        assert!(output.contains("seg m1..m10 c~91 message 1 / message 10\n"));
        assert!(output.contains("seg m11..m20 c~100 message 11 / message 20\n"));
        assert!(output.contains("seg m21..m21 c~10 message 21 / message 21\n"));
        assert!(!output.contains("summary"));
    }
}
