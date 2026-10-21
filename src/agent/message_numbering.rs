//! `agent read`'s numbering of a transcript's messages. The session list's
//! row takes its message count and its semantic turns' message numbers from
//! it, so a semantic or hybrid hit's message number is `agent read`'s number
//! for the matched message.

use crate::agent::transcript::{AgentMessageRole, agent_user_text};
use crate::history::is_clear_metadata_message;
use crate::log_entry::{AssistantMessage, ContentBlock, LogEntry, UserContent};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Reply ids are unique within a logical thread, including when a provider
/// copies a parent's context into several child transcripts.
#[derive(Clone, Default, Hash, PartialEq, Eq)]
pub(crate) enum ThreadIdentity {
    #[default]
    Session,
    Progress {
        transcript: Option<Arc<Path>>,
        label: String,
    },
}

impl ThreadIdentity {
    pub(crate) fn of(entry: &LogEntry, transcript: Option<&Path>) -> Self {
        match entry {
            LogEntry::User {
                parent_tool_use_id: Some(call),
                ..
            }
            | LogEntry::Assistant {
                parent_tool_use_id: Some(call),
                ..
            } => Self::Progress {
                transcript: None,
                label: call.clone(),
            },
            LogEntry::Progress { data, .. }
                if data.get("type").and_then(serde_json::Value::as_str)
                    == Some("agent_progress") =>
            {
                data.get("agentId")
                    .and_then(serde_json::Value::as_str)
                    .map_or(Self::Session, |label| Self::Progress {
                        transcript: transcript.map(Arc::from),
                        label: label.to_owned(),
                    })
            }
            _ => Self::Session,
        }
    }
}

#[derive(Default)]
pub(crate) struct ThreadNumbers(HashMap<ThreadIdentity, i64>);

impl ThreadNumbers {
    pub(crate) fn number(&mut self, identity: ThreadIdentity) -> i64 {
        if identity == ThreadIdentity::Session {
            return 0;
        }
        let next = self.0.len() as i64 + 1;
        *self.0.entry(identity).or_insert(next)
    }
}

/// A message's place in the numbering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Numbered {
    pub(crate) ordinal: usize,
    /// False when the record continues an earlier reply with the same id.
    pub(crate) starts_message: bool,
}

/// Numbers one transcript's messages, fed its entries in order.
#[derive(Default)]
pub(crate) struct MessageNumbering {
    count: usize,
    has_user_message: bool,
    /// Direct replies and progress replies use separate id namespaces.
    own_replies: ReplyOrdinals,
    subagent_replies: ReplyOrdinals,
    threads: ThreadNumbers,
}

impl MessageNumbering {
    pub(crate) fn thread(&mut self, identity: ThreadIdentity) -> i64 {
        self.threads.number(identity)
    }

    /// Number entries using reply ids scoped to a logical thread.
    pub(crate) fn number_in_thread(&mut self, entry: &LogEntry, thread: i64) -> Option<Numbered> {
        match entry {
            LogEntry::User { message, .. } => self.user_message(&message.content),
            LogEntry::Assistant { message, .. } => self.reply(message, thread),
            LogEntry::PiMetadata { searchable, .. } => searchable.then(|| self.next()),
            _ => None,
        }
    }

    pub(crate) fn number_progress_in_thread(
        &mut self,
        role: AgentMessageRole,
        blocks: &[ContentBlock],
        reply_id: Option<&str>,
        thread: i64,
    ) -> Option<Numbered> {
        has_visible_block(role, blocks).then(|| {
            self.subagent_replies
                .number(thread, reply_id, &mut self.count)
        })
    }

    /// Number one spliced sub-agent message: records sharing `reply_id` are
    /// one message. [`has_visible_block`] decides which turns are messages.
    pub(crate) fn number_subagent_message(
        &mut self,
        reply_id: Option<&str>,
        thread: i64,
    ) -> Numbered {
        self.subagent_replies
            .number(thread, reply_id, &mut self.count)
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }

    fn user_message(&mut self, content: &UserContent) -> Option<Numbered> {
        let first_text = first_visible_text(content);
        let has_visible_part = match content {
            UserContent::String(_) => first_text.is_some(),
            UserContent::Blocks(blocks) => has_visible_block(AgentMessageRole::User, blocks),
        };
        if !has_visible_part {
            return None;
        }
        if let Some(text) = first_text.as_deref()
            && (is_clear_metadata_message(text)
                || (!self.has_user_message && text.trim() == "Warmup"))
        {
            return None;
        }
        self.has_user_message = true;
        Some(self.next())
    }

    fn reply(&mut self, message: &AssistantMessage, thread: i64) -> Option<Numbered> {
        if !self.has_user_message
            || !has_visible_block(AgentMessageRole::Assistant, &message.content)
        {
            return None;
        }
        Some(
            self.own_replies
                .number(thread, message.id.as_deref(), &mut self.count),
        )
    }

    fn next(&mut self) -> Numbered {
        next_message(&mut self.count)
    }
}

/// The ordinal of each reply id numbered so far.
#[derive(Default)]
struct ReplyOrdinals(HashMap<i64, HashMap<String, usize>>);

impl ReplyOrdinals {
    /// The ordinal of the reply `id` when an earlier record started it, else
    /// the next message after `count`.
    fn number(&mut self, thread: i64, id: Option<&str>, count: &mut usize) -> Numbered {
        let replies = self.0.entry(thread).or_default();
        if let Some(&ordinal) = id.and_then(|id| replies.get(id)) {
            return Numbered {
                ordinal,
                starts_message: false,
            };
        }
        let numbered = next_message(count);
        if let Some(id) = id {
            replies.insert(id.to_owned(), numbered.ordinal);
        }
        numbered
    }
}

fn next_message(count: &mut usize) -> Numbered {
    *count += 1;
    Numbered {
        ordinal: *count,
        starts_message: true,
    }
}

/// True when `block` shows as a part of its message.
pub(crate) fn is_visible_block(role: AgentMessageRole, block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text } => shown_text(role, text).is_some(),
        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => true,
        ContentBlock::Thinking { thinking, .. } => !thinking.trim().is_empty(),
        ContentBlock::Image { .. } | ContentBlock::Other => false,
    }
}

/// `text` as its message shows it: a user's text after [`agent_user_text`],
/// an assistant's unchanged. `None` when only whitespace is left.
pub(crate) fn visible_text(role: AgentMessageRole, text: String) -> Option<String> {
    let converted = match shown_text(role, &text)? {
        Cow::Owned(converted) => Some(converted),
        Cow::Borrowed(_) => None,
    };
    Some(converted.unwrap_or(text))
}

/// [`visible_text`] borrowed from `text` when nothing converts it.
fn shown_text(role: AgentMessageRole, text: &str) -> Option<Cow<'_, str>> {
    let shown = match role {
        AgentMessageRole::User => agent_user_text(text),
        AgentMessageRole::Assistant => Cow::Borrowed(text),
    };
    (!shown.trim().is_empty()).then_some(shown)
}

/// True when one of `blocks` shows as a part of a message of `role`.
pub(crate) fn has_visible_block(role: AgentMessageRole, blocks: &[ContentBlock]) -> bool {
    blocks.iter().any(|block| is_visible_block(role, block))
}

fn first_visible_text(content: &UserContent) -> Option<Cow<'_, str>> {
    match content {
        UserContent::String(text) => shown_text(AgentMessageRole::User, text),
        UserContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
            ContentBlock::Text { text } => shown_text(AgentMessageRole::User, text),
            _ => None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::test_support::assert_row_numbers_messages_as_transcript;
    use crate::agent::transcript::{AgentTranscript, agent_part_text};
    use crate::history::format::RecordLine;
    use crate::history::parser::{RowDating, conversation_from_entries};
    use crate::history::{Conversation, Source};
    use crate::log_entry::LogEntry;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    const PROMPT: &str = "Which module numbers the messages of a session?";
    const REPLY: &str = "The numbering module counts them for the list and for the agent CLI.";

    fn user(content: Value) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": content}})
    }

    fn assistant(content: Value) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant", "content": content}})
    }

    fn prompt() -> Value {
        user(json!(PROMPT))
    }

    fn reply() -> Value {
        assistant(json!([{"type": "text", "text": REPLY}]))
    }

    fn tool_call() -> Value {
        assistant(json!([{"type": "tool_use", "id": "call_1", "name": "Read", "input": {}}]))
    }

    fn tool_result(content: Value) -> Value {
        user(json!([{"type": "tool_result", "tool_use_id": "call_1", "content": content}]))
    }

    fn entries(records: &[Value]) -> Vec<LogEntry> {
        records
            .iter()
            .map(|record| serde_json::from_value(record.clone()).unwrap())
            .collect()
    }

    fn row(path: &Path, entries: Vec<LogEntry>) -> Conversation {
        conversation_from_entries(
            Source::Claude,
            path.to_path_buf(),
            &entries,
            &[],
            Vec::new(),
            RowDating::new(None, &crate::clock::TestClock::default()),
            None,
        )
        .expect("a session holding a prompt is listed")
    }

    fn transcript(path: &Path, entries: Vec<LogEntry>) -> AgentTranscript {
        let lines = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| (RecordLine::from(index + 1), entry))
            .collect();
        AgentTranscript::from_entries(path, lines, Vec::new()).unwrap()
    }

    /// The text of each part `agent read` shows, per message of `records`.
    fn message_texts(records: &[Value]) -> Vec<Vec<String>> {
        transcript(Path::new("session.jsonl"), entries(records))
            .messages
            .iter()
            .map(|message| message.parts.iter().filter_map(agent_part_text).collect())
            .collect()
    }

    /// Asserts that the row and `agent read` number `records` alike: `count`
    /// messages, and the reply at one ordinal.
    fn assert_numbered_alike(records: &[Value], count: usize) {
        let entries = entries(records);
        let path = PathBuf::from("session.jsonl");
        let row = row(&path, entries.clone());
        let transcript = transcript(&path, entries);
        assert_row_numbers_messages_as_transcript(&row, &transcript, count, &[REPLY]);
    }

    #[test]
    fn tool_result_with_no_text_is_message() {
        let image = json!({"type": "image", "source": {"type": "base64", "data": "AA=="}});
        for content in [
            json!([{"type": "tool_reference", "tool_name": "Read"}]),
            json!(""),
            json!([image]),
        ] {
            assert_numbered_alike(&[prompt(), tool_call(), tool_result(content), reply()], 4);
        }
    }

    #[test]
    fn users_shell_command_with_empty_output_is_message() {
        let shell = user(json!([
            {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "true"}},
            {"type": "tool_result", "tool_use_id": "call_1", "content": ""},
        ]));
        assert_numbered_alike(&[prompt(), shell, reply()], 3);
    }

    #[test]
    fn replies_after_session_opens_with_tool_result_are_messages() {
        let opening_reply = assistant(json!([{"type": "text", "text": "The file is empty."}]));
        assert_numbered_alike(
            &[tool_result(json!("")), opening_reply, prompt(), reply()],
            4,
        );
    }

    #[test]
    fn user_text_of_whitespace_alone_isnt_message() {
        assert_numbered_alike(&[prompt(), user(json!("   ")), reply()], 2);
    }

    #[test]
    fn searchable_metadata_row_with_empty_text_is_message() {
        let row = json!({"type": "pi-metadata", "label": "Custom", "text": ""});
        assert_numbered_alike(&[prompt(), row, reply()], 3);
    }

    #[test]
    fn warmup_with_no_reply_leaves_next_reply_message() {
        assert_numbered_alike(&[user(json!("Warmup")), prompt(), reply()], 2);
    }

    #[test]
    fn reply_after_opening_tool_result_counts_as_answer() {
        let records = [tool_result(json!("")), reply(), prompt()];

        let row = row(Path::new("session.jsonl"), entries(&records));

        assert_eq!(row.assistant_messages, 1);
    }

    #[test]
    fn warmup_after_opening_tool_result_is_message() {
        assert_numbered_alike(&[tool_result(json!("")), user(json!("Warmup")), reply()], 3);
    }

    #[test]
    fn subagent_reply_of_several_records_is_one_message() {
        let records = [
            prompt(),
            subagent_progress("assistant", "msg_child", "The child reads the file."),
            subagent_progress("assistant", "msg_child", "The child found the bug."),
            reply(),
        ];

        assert_numbered_alike(&records, 3);
        assert_eq!(
            message_texts(&records)[1],
            ["The child reads the file.", "The child found the bug."]
        );
    }

    #[test]
    fn subagent_record_sharing_session_reply_id_is_message_of_its_own() {
        let mut opening = assistant(json!([{"type": "text", "text": "Launching the fork."}]));
        opening["message"]["id"] = json!("msg_parent");
        let copy = subagent_progress("assistant", "msg_parent", "Launching the fork.");

        assert_numbered_alike(&[prompt(), opening, copy, reply()], 4);
    }

    #[test]
    fn equal_reply_ids_in_distinct_children_are_separate_messages() {
        let first = subagent_progress("assistant", "copied_reply", "First child.");
        let mut second = subagent_progress("assistant", "copied_reply", "Second child.");
        second["data"]["agentId"] = json!("a2");
        let records = [prompt(), first, second, reply()];

        assert_numbered_alike(&records, 4);
        assert_eq!(
            message_texts(&records)[1..3],
            [["First child."], ["Second child."]]
        );
    }

    #[test]
    fn repeated_child_labels_in_distinct_transcripts_doesnt_merge_replies() {
        let records = [
            prompt(),
            subagent_progress("assistant", "copied_reply", "First child."),
            subagent_progress("assistant", "copied_reply", "Second child."),
            subagent_progress("assistant", "copied_reply", "First child continues."),
        ];
        let lines = entries(&records)
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let transcript = match index {
                    1 | 3 => Some(std::sync::Arc::from(Path::new("child-1.jsonl"))),
                    2 => Some(std::sync::Arc::from(Path::new("child-2.jsonl"))),
                    _ => None,
                };
                (
                    RecordLine {
                        line: index + 1,
                        transcript,
                    },
                    entry,
                )
            })
            .collect();

        let transcript =
            AgentTranscript::from_entries(Path::new("parent.jsonl"), lines, Vec::new()).unwrap();

        assert_eq!(transcript.messages.len(), 3);
        let texts: Vec<_> = transcript.messages[1]
            .parts
            .iter()
            .filter_map(agent_part_text)
            .collect();
        assert_eq!(texts, ["First child.", "First child continues."]);
        assert_eq!(
            transcript.messages[2]
                .parts
                .iter()
                .filter_map(agent_part_text)
                .collect::<Vec<_>>(),
            ["Second child."]
        );
    }

    #[test]
    fn sidechains_with_equal_reply_ids_have_distinct_semantic_turns() {
        let mut first = reply();
        first["message"]["id"] = json!("shared_reply");
        first["parent_tool_use_id"] = json!("call_1");
        let mut second = first.clone();
        second["parent_tool_use_id"] = json!("call_2");
        second["message"]["content"][0]["text"] = json!("The second child checks the source.");

        assert_numbered_alike(&[prompt(), first, second], 3);
    }

    #[test]
    fn slash_commands_output_and_error_are_messages_shown_without_their_tags() {
        let records = [
            prompt(),
            user(json!(
                "<local-command-stdout>\u{1b}[2mCompacted\u{1b}[22m</local-command-stdout>"
            )),
            user(json!(
                "<local-command-stderr>Error during compaction: connection lost.</local-command-stderr>"
            )),
            reply(),
        ];

        assert_numbered_alike(&records, 4);
        assert_eq!(
            message_texts(&records)[1..3],
            [["Compacted"], ["Error during compaction: connection lost."]]
        );
    }

    /// A sub-agent's `role` record in a Claude `agent_progress` payload,
    /// holding `text` under the reply id `id`.
    fn subagent_progress(role: &str, id: &str, text: &str) -> Value {
        json!({
            "type": "progress",
            "data": {
                "type": "agent_progress",
                "agentId": "a1",
                "message": {
                    "type": role,
                    "message": {"role": role, "id": id, "content": [{"type": "text", "text": text}]},
                },
            },
        })
    }
}
