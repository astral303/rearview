//! `agent read`'s numbering of a transcript's messages. The session list's
//! row takes its message count and its semantic turns' message numbers from
//! it, so a semantic or hybrid hit's message number is `agent read`'s number
//! for the matched message.

use crate::agent::transcript::{AgentMessageRole, agent_user_text};
use crate::history::is_clear_metadata_message;
use crate::log_entry::{AssistantMessage, ContentBlock, LogEntry, UserContent};
use std::borrow::Cow;
use std::collections::HashMap;

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
    ordinals_by_reply_id: HashMap<String, usize>,
}

impl MessageNumbering {
    /// `entry`'s place among the messages; `None` when `entry` is not a
    /// message. A progress record is numbered by [`Self::number_progress`],
    /// from the caller's parse.
    pub(crate) fn number(&mut self, entry: &LogEntry) -> Option<Numbered> {
        match entry {
            LogEntry::User { message, .. } => self.user_message(&message.content),
            LogEntry::Assistant { message, .. } => self.reply(message),
            LogEntry::PiMetadata { searchable, .. } => searchable.then(|| self.next()),
            _ => None,
        }
    }

    /// A sub-agent progress record's place among the messages, from its
    /// `role` and `blocks`.
    pub(crate) fn number_progress(
        &mut self,
        role: AgentMessageRole,
        blocks: &[ContentBlock],
    ) -> Option<Numbered> {
        has_visible_block(role, blocks).then(|| self.next())
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

    fn reply(&mut self, message: &AssistantMessage) -> Option<Numbered> {
        if !self.has_user_message
            || !has_visible_block(AgentMessageRole::Assistant, &message.content)
        {
            return None;
        }
        if let Some(&ordinal) = message
            .id
            .as_ref()
            .and_then(|id| self.ordinals_by_reply_id.get(id))
        {
            return Some(Numbered {
                ordinal,
                starts_message: false,
            });
        }
        let numbered = self.next();
        if let Some(id) = &message.id {
            self.ordinals_by_reply_id
                .insert(id.clone(), numbered.ordinal);
        }
        Some(numbered)
    }

    fn next(&mut self) -> Numbered {
        self.count += 1;
        Numbered {
            ordinal: self.count,
            starts_message: true,
        }
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

fn has_visible_block(role: AgentMessageRole, blocks: &[ContentBlock]) -> bool {
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
    use crate::agent::transcript::AgentTranscript;
    use crate::history::format::RecordLine;
    use crate::history::parser::conversation_from_entries;
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
            entries,
            Vec::new(),
            None,
            None,
        )
        .expect("a session holding a prompt is listed")
    }

    /// Asserts that the row and `agent read` number `records` alike: `count`
    /// messages, and the reply at one ordinal.
    fn assert_numbered_alike(records: &[Value], count: usize) {
        let entries = entries(records);
        let path = PathBuf::from("session.jsonl");
        let row = row(&path, entries.clone());
        let lines = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| (RecordLine::from(index + 1), entry))
            .collect();
        let transcript = AgentTranscript::from_entries(&path, lines, Vec::new()).unwrap();
        assert_row_numbers_messages_as_transcript(&row, &transcript, count, &[REPLY]);
    }

    #[test]
    fn a_tool_result_with_no_text_is_a_message() {
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
    fn a_users_shell_command_with_empty_output_is_a_message() {
        let shell = user(json!([
            {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "true"}},
            {"type": "tool_result", "tool_use_id": "call_1", "content": ""},
        ]));
        assert_numbered_alike(&[prompt(), shell, reply()], 3);
    }

    #[test]
    fn replies_after_a_session_opens_with_a_tool_result_are_messages() {
        let opening_reply = assistant(json!([{"type": "text", "text": "The file is empty."}]));
        assert_numbered_alike(
            &[tool_result(json!("")), opening_reply, prompt(), reply()],
            4,
        );
    }

    #[test]
    fn user_text_of_whitespace_alone_isnt_a_message() {
        assert_numbered_alike(&[prompt(), user(json!("   ")), reply()], 2);
    }

    #[test]
    fn a_searchable_metadata_row_with_empty_text_is_a_message() {
        let row = json!({"type": "pi-metadata", "label": "Custom", "text": ""});
        assert_numbered_alike(&[prompt(), row, reply()], 3);
    }

    #[test]
    fn a_warmup_with_no_reply_leaves_the_next_reply_a_message() {
        assert_numbered_alike(&[user(json!("Warmup")), prompt(), reply()], 2);
    }

    #[test]
    fn a_reply_after_an_opening_tool_result_counts_as_an_answer() {
        let records = [tool_result(json!("")), reply(), prompt()];

        let row = row(Path::new("session.jsonl"), entries(&records));

        assert_eq!(row.assistant_messages, 1);
    }

    #[test]
    fn a_warmup_after_an_opening_tool_result_is_a_message() {
        assert_numbered_alike(&[tool_result(json!("")), user(json!("Warmup")), reply()], 3);
    }
}
