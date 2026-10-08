use super::{ParsedConversation, ToolOutputId};
use crate::history::EntryOrigin;
use std::collections::HashMap;

/// Where each entry of an earlier read of a conversation sits in a later
/// read, found by the record it was read from. When a sub-agent writes more,
/// its new turns are spliced in before later entries, shifting their indices;
/// each entry's origin stays.
#[derive(Debug)]
pub struct EntryRelocation {
    earlier: Vec<EntryOrigin>,
    later: HashMap<EntryOrigin, usize>,
}

impl EntryRelocation {
    pub fn between(earlier: &ParsedConversation, later: &ParsedConversation) -> Self {
        Self {
            earlier: earlier.origins.clone(),
            later: later
                .origins
                .iter()
                .cloned()
                .enumerate()
                .map(|(entry_index, origin)| (origin, entry_index))
                .collect(),
        }
    }

    /// The later index of the entry at `old_index`; `None` when the later read
    /// no longer holds its record.
    pub fn get(&self, old_index: usize) -> Option<usize> {
        self.later.get(self.earlier.get(old_index)?).copied()
    }

    /// The later index of the entry at `old_index`, or of the nearest earlier
    /// entry the later read still holds.
    pub fn get_or_previous(&self, old_index: usize) -> Option<usize> {
        (0..=old_index).rev().find_map(|index| self.get(index))
    }

    /// `id` in the later read; `None` when its entry is gone.
    pub fn tool_output_id(&self, id: &ToolOutputId) -> Option<ToolOutputId> {
        id.with_entry_index(self.get(id.entry_index()?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::DisplayEntries;
    use crate::history::format::RecordLine;
    use crate::log_entry::{ContentBlock, LogEntry, UserContent, UserMessage};
    use crate::tui::viewer::parsed_conversation;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;

    fn user(text: &str) -> LogEntry {
        LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::String(text.to_owned()),
            },
            timestamp: None,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: None,
        }
    }

    /// `entry` read from line `line` of the session's own transcript.
    fn own(line: usize, entry: LogEntry) -> (RecordLine, LogEntry) {
        (RecordLine::from(line), entry)
    }

    /// A sub-agent turn as the splice records it, read from the first line of
    /// `transcript`.
    fn spliced(transcript: &str, text: &str) -> (RecordLine, LogEntry) {
        let record = RecordLine {
            transcript: Some(Arc::from(Path::new(transcript))),
            line: 1,
        };
        let entry = LogEntry::Progress {
            data: json!({
                "type": "agent_progress",
                "agentId": "Explore",
                "message": {
                    "type": "user",
                    "message": {"role": "user", "content": [{"type": "text", "text": text}]}
                }
            }),
            extra: json!({}),
        };
        (record, entry)
    }

    fn conversation(entries: Vec<(RecordLine, LogEntry)>) -> ParsedConversation {
        parsed_conversation(DisplayEntries::from(entries))
    }

    #[test]
    fn a_sub_agent_turn_spliced_in_before_an_entry_moves_it() {
        let earlier = conversation(vec![own(1, user("a")), own(2, user("b"))]);
        let later = conversation(vec![
            own(1, user("a")),
            spliced("agent-x.jsonl", "sub-agent turn"),
            own(2, user("b")),
        ]);

        let relocation = EntryRelocation::between(&earlier, &later);

        assert_eq!(relocation.get(0), Some(0));
        assert_eq!(relocation.get(1), Some(2));
    }

    #[test]
    fn an_entry_that_grew_keeps_its_place() {
        let earlier = conversation(vec![own(1, user("a")), own(2, user("partial"))]);
        let later = conversation(vec![
            own(1, user("a")),
            own(2, user("partial, then more")),
            own(3, user("c")),
        ]);

        let relocation = EntryRelocation::between(&earlier, &later);

        assert_eq!(relocation.get(1), Some(1));
        let id = ToolOutputId("entry:1:parent:top:kind:summary".to_owned());
        assert_eq!(relocation.tool_output_id(&id), Some(id));
    }

    #[test]
    fn entries_with_one_line_number_in_different_transcripts_stay_apart() {
        let earlier = conversation(vec![
            own(1, user("parent")),
            spliced("agent-x.jsonl", "x"),
            spliced("agent-y.jsonl", "y"),
        ]);
        let later = conversation(vec![
            own(1, user("parent")),
            spliced("agent-y.jsonl", "y"),
            spliced("agent-x.jsonl", "x"),
        ]);

        let relocation = EntryRelocation::between(&earlier, &later);

        assert_eq!(relocation.get(1), Some(2));
        assert_eq!(relocation.get(2), Some(1));
    }

    /// The index of the entry holding the tool call `call_id`.
    fn call_index(conversation: &ParsedConversation, call_id: &str) -> usize {
        conversation
            .entries
            .iter()
            .find(|parsed| match &parsed.entry {
                LogEntry::Assistant { message, .. } => message.content.iter().any(
                    |block| matches!(block, ContentBlock::ToolUse { id, .. } if id == call_id),
                ),
                _ => false,
            })
            .map(|parsed| parsed.entry_index)
            .unwrap()
    }

    #[test]
    fn an_expanded_opencode_call_stays_on_it_when_an_earlier_call_finishes() {
        use crate::history::format::SessionFormat;
        use crate::history::format::opencode::{OPENCODE_DB, fixture, session_ref};

        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = fixture::create_database(&database);
        fixture::insert_session(
            &connection,
            &fixture::SessionSpec {
                id: "ses_live",
                parent_id: None,
                directory: "/tmp/live",
                title: "",
                created_ms: 1_755_000_000_000,
                updated_ms: 1_755_000_000_000,
                archived_ms: None,
            },
        );
        let message = |id: &str, role: &str, time_ms: i64| {
            fixture::insert_message(
                &connection,
                id,
                "ses_live",
                time_ms,
                &json!({ "role": role, "time": { "created": time_ms } }),
            );
        };
        message("msg_user", "user", 1_755_000_000_000);
        fixture::insert_part(
            &connection,
            "prt_user",
            "msg_user",
            "ses_live",
            1_755_000_000_000,
            &json!({ "type": "text", "text": "list the files twice" }),
        );
        message("msg_asst", "assistant", 1_755_000_001_000);
        let running = |call_id: &str| {
            json!({
                "type": "tool",
                "tool": "bash",
                "callID": call_id,
                "state": { "status": "running", "input": { "command": "ls" } },
            })
        };
        for (part, call_id, time_ms) in [
            ("prt_first", "call_first", 1_755_000_002_000),
            ("prt_second", "call_second", 1_755_000_003_000),
        ] {
            fixture::insert_part(
                &connection,
                part,
                "msg_asst",
                "ses_live",
                time_ms,
                &running(call_id),
            );
        }
        let read = || {
            parsed_conversation(DisplayEntries::from(
                OPENCODE_DB
                    .session_entries(&session_ref(&database, "ses_live"), &[])
                    .unwrap()
                    .unwrap(),
            ))
        };
        let earlier = read();

        let mut finished = running("call_first");
        finished["state"] =
            json!({ "status": "completed", "input": { "command": "ls" }, "output": "a.txt" });
        connection
            .execute(
                "UPDATE part SET data = ?1 WHERE id = 'prt_first'",
                [finished.to_string()],
            )
            .unwrap();
        let later = read();

        let relocation = EntryRelocation::between(&earlier, &later);
        let expanded = ToolOutputId(format!(
            "entry:{}:parent:top:kind:summary",
            call_index(&earlier, "call_second")
        ));
        assert_eq!(
            relocation
                .tool_output_id(&expanded)
                .and_then(|id| id.entry_index()),
            Some(call_index(&later, "call_second")),
        );
        assert_ne!(
            call_index(&earlier, "call_second"),
            call_index(&later, "call_second"),
            "the first call's result lands before the second call"
        );
    }

    #[test]
    fn a_removed_entry_relocates_to_the_nearest_earlier_one() {
        let earlier = conversation(vec![
            own(1, user("a")),
            own(2, user("b")),
            own(3, user("c")),
        ]);
        let later = conversation(vec![own(1, user("a")), own(3, user("c"))]);

        let relocation = EntryRelocation::between(&earlier, &later);

        assert_eq!(relocation.get(1), None);
        assert_eq!(relocation.get_or_previous(1), Some(0));
        assert_eq!(relocation.get(2), Some(1));
    }
}
