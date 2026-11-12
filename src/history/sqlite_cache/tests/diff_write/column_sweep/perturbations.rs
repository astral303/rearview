//! The sweep's perturbations: for each stored column, a change to the
//! session's parse that reaches it and what the change proves, or the
//! reason the sweep doesn't change it.

use super::*;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::parser::SubagentMetadataPart;
use crate::history::sqlite_cache::catalogue::{CatalogueRow, SessionFile, SessionStatus};
use crate::history::{MalformedLine, MalformedLineDetail, ParseError};
use crate::log_entry::{TokenUsage, Tool, UserMessage};
use Claim::DigestAndBind;
use serde_json::json;

/// The columns the sweep doesn't change, with the reason.
pub(super) const NOT_PERTURBED: &[(&str, &[&str], &str)] = &[
    (
        "entries",
        &[
            "message",
            "contributes",
            "position",
            "sequence",
            "record_transcript",
            "child",
        ],
        "the entry's place: `update_entries` compares it by value",
    ),
    (
        "entries",
        &["columns_digest"],
        "a hash of the entry's columns besides its place (`message`, `contributes`, `position`, \
         `sequence`, `record_transcript`, `child`)",
    ),
    (
        "entries",
        &["digest"],
        "a hash of `columns_digest` and the entry's parts",
    ),
    (
        "messages",
        &["thread", "ordinal"],
        "the message's place: `update_messages` compares it by value",
    ),
    (
        "messages",
        &["digest"],
        "a hash of the message's columns besides `thread` and `ordinal`",
    ),
    (
        "parts",
        &["entry", "part"],
        "the part's key: `parts::Writer::update` matches an entry's parts by number",
    ),
    (
        "parts",
        &["entry_sequence", "message_ordinal"],
        "the part's search place: `place_entry` sets it from the entry's place",
    ),
    (
        "parts",
        &["search_position"],
        "the layout of the entry's search texts sets it, and the entry's digest hashes it",
    ),
    (
        "parts",
        &["digest"],
        "a hash of the part's columns besides `entry`, `part`, `entry_sequence`, \
         `message_ordinal`, `search_position` and `file_position`, and of its result items",
    ),
    (
        "result_items",
        &["part", "position"],
        "the item's key: a write deletes a changed part's items and inserts them again",
    ),
    (
        "sessions",
        &["storage", "root", "cache_key"],
        "the row's key: a write under another key writes another session",
    ),
    ("session_files", &["position"], "the row's key"),
    (
        "session_files",
        &["kind", "root", "path"],
        "the positional write matches rows by the file's kind and place (`file_key_of`)",
    ),
    (
        "session_files",
        &["key"],
        "`StoredPath::key` derives it from `path`: the path as a lookup compares it",
    ),
    ("transcript_reads", &["position"], "the row's key"),
    (
        "transcript_reads",
        &["root", "path"],
        "the positional write matches rows by the file's place (`read_key_of`)",
    ),
    ("parse_errors", &["position"], "the row's key"),
    ("malformed_lines", &["position"], "the row's key"),
    (
        "threads",
        &["number"],
        "`match_threads` updates the thread number by value",
    ),
    (
        "threads",
        &["transcript"],
        "`match_threads` matches threads by their transcript, with the rest of their origin",
    ),
    (
        "transcripts",
        &["root", "path"],
        "`match_transcripts` matches transcripts by their place",
    ),
    (
        "semantic_turns",
        &["sequence", "ordinal"],
        "the turn's place: `place_entry` moves it with its entry",
    ),
    (
        "session_checkpoints",
        &["sequence", "checkpoint", "ledger"],
        "`CheckpointRecord` keeps its fields private to `catalogue`; the `resumed` tests read \
         on from the stored checkpoint and ledger, and compare the three columns with a fresh \
         write",
    ),
];

/// The tables a digest covers: `parts.digest` covers a part's result items.
pub(super) const DIGESTED_TABLES: &[&str] = &["entries", "messages", "parts", "result_items"];

/// The value a changed column holds after the write.
#[derive(Debug)]
pub(super) enum ExpectedValue {
    /// One row more than before holds this text.
    Text(&'static str),
    /// One row more than before holds this integer.
    Integer(i64),
    /// One row more than before holds these bytes.
    Blob(&'static [u8]),
    /// The rows whose `text` column holds `text` hold `values`, one each,
    /// in the stored order: a row's natural key first.
    InRowsOfText {
        text: &'static str,
        values: Vec<rusqlite::types::Value>,
    },
    /// The column's values differ from those before the change, row by row
    /// in the stored order: a row's natural key first.
    Changed,
}

/// The claim a perturbation proves about a column it reaches.
#[derive(Clone, Copy)]
pub(super) enum Claim {
    /// The change alters one input of the row's digest: a digest missing
    /// the column keeps the stored row, and the write fails the comparison
    /// with a fresh write. The column also holds the changed value at its
    /// own place.
    DigestAndBind,
    /// The write rewrites the row for the reason given, whether or not its
    /// digest holds this column: the sweep checks the stored value and its
    /// place alone.
    BindOnly(&'static str),
}

impl std::fmt::Display for Claim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DigestAndBind => formatter.write_str("digest and bind"),
            Self::BindOnly(reason) => write!(formatter, "bind only: {reason}"),
        }
    }
}

const NO_DIGEST: Claim = Claim::BindOnly("the row doesn't have a digest");
const CHANGES_TEXT: Claim = Claim::BindOnly("the change also changes `text`");
const OUTSIDE_EVERY_ENTRY: Claim =
    Claim::BindOnly("a part outside every entry doesn't have a digest");

/// A change to a session's parse. It returns false, leaving the parse as it
/// was, when the parse doesn't hold the parsed field it changes.
pub(super) type Change = Box<dyn Fn(&mut SessionRecord) -> bool>;

/// A column a perturbation changes, the value it then holds, and what the
/// change proves about it.
pub(super) struct Reach {
    pub(super) table: &'static str,
    pub(super) column: &'static str,
    pub(super) stored: ExpectedValue,
    pub(super) proves: Claim,
}

impl Reach {
    pub(super) fn name(&self) -> String {
        format!("{}.{}", self.table, self.column)
    }
}

/// A change to one parsed field and each column it reaches.
pub(super) struct Perturbation {
    /// Sets the other parsed fields the change needs, when they are missing
    /// from each every-record fixture. The sweep stores the seeded parse,
    /// then writes the change over it.
    pub(super) seed: Option<Change>,
    change: Change,
    pub(super) reaches: Vec<Reach>,
    /// The columns, as `(table, column)`, the change leaves as they were.
    pub(super) keeps: Vec<(&'static str, &'static str)>,
    /// True when no other perturbation joins the write of this one.
    pub(super) needs_own_write: bool,
}

impl Perturbation {
    fn new(change: impl Fn(&mut SessionRecord) -> bool + 'static) -> Self {
        Self {
            seed: None,
            change: Box::new(change),
            reaches: Vec::new(),
            keeps: Vec::new(),
            needs_own_write: false,
        }
    }

    fn seeded(
        seed: impl Fn(&mut SessionRecord) -> bool + 'static,
        change: impl Fn(&mut SessionRecord) -> bool + 'static,
    ) -> Self {
        Self {
            seed: Some(Box::new(seed)),
            ..Self::new(change)
        }
    }

    fn reaching(
        mut self,
        table: &'static str,
        column: &'static str,
        stored: ExpectedValue,
        proves: Claim,
    ) -> Self {
        self.reaches.push(Reach {
            table,
            column,
            stored,
            proves,
        });
        self
    }

    fn keeping(mut self, table: &'static str, column: &'static str) -> Self {
        self.keeps.push((table, column));
        self
    }

    fn in_own_write(mut self) -> Self {
        self.needs_own_write = true;
        self
    }

    pub(super) fn name(&self) -> String {
        self.reaches
            .iter()
            .map(Reach::name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn tables(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.reaches.iter().map(|reach| reach.table)
    }

    /// Applies the seed, then the change, to `record`. Returns false,
    /// leaving `record` as it was, when the seed doesn't apply, or, without
    /// a seed, when the change doesn't. Panics when the change doesn't apply
    /// to the parse its seed made.
    pub(super) fn apply(&self, record: &mut SessionRecord) -> bool {
        let Some(seed) = &self.seed else {
            return (self.change)(record);
        };
        if !seed(record) {
            return false;
        }
        assert!(
            (self.change)(record),
            "{}: the change applies to the parse its seed made",
            self.name()
        );
        true
    }
}

const TEXT_WITH_CODES: &str = "\u{1b}[31mperturbed text\u{1b}[0m";
/// The `text_codes` of [`TEXT_WITH_CODES`]: per run of codes, its offset
/// in the text after the previous run's and its length, each one LEB128
/// byte, then its bytes.
const TEXT_CODES: &[u8] = b"\x00\x05\x1b[31m\x0e\x04\x1b[0m";
/// 2100-01-01T00:00:00Z, in milliseconds.
const LATE_MS: i64 = 4_102_444_800_000;
/// The session file a sub-agent metadata part is added on: Claude's second
/// sub-agent transcript.
const METADATA_FILE: usize = 2;
/// The thinking block's text in the search field perturbation. The change
/// sets the text block's text to it.
const SHARED_TEXT: &str = "perturbed shared text";

pub(super) fn all() -> Vec<Perturbation> {
    let mut perturbations = entry_perturbations();
    perturbations.extend(message_perturbations());
    perturbations.extend(part_perturbations());
    perturbations.extend(catalogue_perturbations());
    perturbations.extend(thread_perturbations());
    perturbations
}

fn entry_perturbations() -> Vec<Perturbation> {
    use ExpectedValue::{Blob, Changed, Integer, Text};
    vec![
        Perturbation::new(|record| {
            entries_of(record)
                .entries
                .last_mut()
                .map(|(line, _)| line.line = 70_001)
                .is_some()
        })
        .reaching(
            "entries",
            "record_line",
            Integer(70_001),
            Claim::BindOnly("the line is the entry's match key: the row is deleted and inserted"),
        ),
        Perturbation::new(|record| set_unspliced_progress_time(record, "1970-01-01T00:01:10.002Z"))
            .reaching("entries", "timestamp_ms", Integer(70_002), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match timestamp_of(entry) {
                Some(timestamp) if timestamp.ends_with('Z') => {
                    *timestamp = timestamp.replace('Z', "+00:00");
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "splice_key", Changed, DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::User { message, .. } => match &message.content {
                    UserContent::String(text) => {
                        message.content =
                            UserContent::Blocks(vec![ContentBlock::Text { text: text.clone() }]);
                        true
                    }
                    UserContent::Blocks(_) => false,
                },
                _ => false,
            })
        })
        .reaching("entries", "content", Text("blocks"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::Summary { summary: title }
                | LogEntry::CustomTitle {
                    custom_title: title,
                }
                | LogEntry::AiTitle { ai_title: title } => {
                    *entry = LogEntry::AgentName {
                        agent_name: title.clone(),
                    };
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "kind", Text("agent-name"), DigestAndBind),
        Perturbation::new(|record| set_first_entry_text(record, "perturbed entry text")).reaching(
            "entries",
            "text",
            Text("perturbed entry text"),
            DigestAndBind,
        ),
        Perturbation::new(|record| set_first_entry_text(record, TEXT_WITH_CODES)).reaching(
            "entries",
            "text_codes",
            Blob(TEXT_CODES),
            CHANGES_TEXT,
        ),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::System { subtype: label, .. } | LogEntry::PiMetadata { label, .. } => {
                    *label = "perturbed label".to_owned();
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "label", Text("perturbed label"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::User { uuid, .. } | LogEntry::Assistant { uuid, .. } => {
                    *uuid = Some("perturbed-uuid".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "uuid", Text("perturbed-uuid"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::System { level, .. } => {
                    *level = Some("perturbed-level".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "level", Text("perturbed-level"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::System { duration_ms, .. } => {
                    *duration_ms = Some(70_003);
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "duration_ms", Integer(70_003), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::System { parent_uuid, .. } => {
                    *parent_uuid = Some("perturbed-parent-uuid".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching(
            "entries",
            "parent_uuid",
            Text("perturbed-parent-uuid"),
            DigestAndBind,
        ),
        Perturbation::new(|record| set_metadata_usage(record, |usage| usage.input_tokens = 70_011))
            .reaching("entries", "usage_input", Integer(70_011), DigestAndBind),
        Perturbation::new(|record| {
            set_metadata_usage(record, |usage| usage.output_tokens = 70_012)
        })
        .reaching("entries", "usage_output", Integer(70_012), DigestAndBind),
        Perturbation::new(|record| {
            set_metadata_usage(record, |usage| usage.cache_creation_input_tokens = 70_013)
        })
        .reaching(
            "entries",
            "usage_cache_creation",
            Integer(70_013),
            DigestAndBind,
        ),
        Perturbation::new(|record| {
            set_metadata_usage(record, |usage| usage.cache_read_input_tokens = 70_014)
        })
        .reaching(
            "entries",
            "usage_cache_read",
            Integer(70_014),
            DigestAndBind,
        ),
        // The change also renumbers the session: a searchable metadata row
        // is a numbered message.
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::PiMetadata {
                    searchable: searchable @ true,
                    ..
                } => {
                    *searchable = false;
                    true
                }
                _ => false,
            })
        })
        .reaching("entries", "flag", Integer(0), DigestAndBind)
        .reaching("sessions", "message_count", Changed, NO_DIGEST),
    ]
}

fn message_perturbations() -> Vec<Perturbation> {
    use ExpectedValue::{Integer, Text};
    vec![
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::Assistant { message, .. } => {
                    message.role = "perturbed role".to_owned();
                    true
                }
                _ => false,
            })
        })
        .reaching("messages", "role", Text("perturbed role"), DigestAndBind),
        Perturbation::new(|record| rename_first_reply(record, "perturbed-reply")).reaching(
            "messages",
            "reply_id",
            Text("perturbed-reply"),
            DigestAndBind,
        ),
        Perturbation::new(|record| set_first_reply_timestamp(record, "1970-01-01T00:01:10.002Z"))
            .reaching("messages", "first_ms", Integer(70_002), DigestAndBind)
            .keeping("messages", "last_ms"),
        Perturbation::new(|record| set_first_reply_timestamp(record, "2100-01-01T00:00:00Z"))
            .reaching("messages", "last_ms", Integer(LATE_MS), DigestAndBind)
            .keeping("messages", "first_ms"),
        Perturbation::new(|record| {
            change_last_entry(record, |entry| match entry {
                LogEntry::Assistant { message, .. } => {
                    message.model = Some("perturbed-model".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching("messages", "model", Text("perturbed-model"), DigestAndBind),
        Perturbation::new(|record| {
            change_last_entry(record, |entry| match entry {
                LogEntry::Assistant { agent, .. } => {
                    *agent = Some("perturbed-agent".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching("messages", "agent", Text("perturbed-agent"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::User { cwd, .. } => {
                    *cwd = Some("perturbed-cwd".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching("messages", "cwd", Text("perturbed-cwd"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::User {
                    parent_tool_use_id, ..
                } => {
                    *parent_tool_use_id = Some("perturbed-thread-call".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching(
            "messages",
            "parent_tool_use_id",
            Text("perturbed-thread-call"),
            Claim::BindOnly(
                "the change moves the message to another thread: `Numbering::place` keys a \
                 message's thread on this call, and `update_messages` rewrites a message whose \
                 thread changed",
            ),
        )
        .reaching("threads", "label", Text("perturbed-thread-call"), NO_DIGEST)
        .reaching(
            "threads",
            "launch_call",
            Text("perturbed-thread-call"),
            NO_DIGEST,
        ),
        Perturbation::new(|record| {
            change_first_entry(record, |entry| match entry {
                LogEntry::User {
                    source_tool_use_id, ..
                } => {
                    *source_tool_use_id = Some("perturbed-source-call".to_owned());
                    true
                }
                _ => false,
            })
        })
        .reaching(
            "messages",
            "source_tool_use_id",
            Text("perturbed-source-call"),
            DigestAndBind,
        ),
        Perturbation::new(|record| set_report_tag(record, "result", None)).reaching(
            "messages",
            "report_has_text",
            Integer(0),
            DigestAndBind,
        ),
        Perturbation::new(|record| set_report_tag(record, "task-id", Some("perturbed-task")))
            .reaching(
                "messages",
                "report_task_id",
                Text("perturbed-task"),
                DigestAndBind,
            ),
        Perturbation::new(|record| set_report_tag(record, "tool-use-id", Some("perturbed-launch")))
            .reaching(
                "messages",
                "report_launch_call",
                Text("perturbed-launch"),
                DigestAndBind,
            ),
        Perturbation::new(|record| set_reply_usage(record, |usage| usage.input_tokens = 70_031))
            .reaching("messages", "usage_input", Integer(70_031), DigestAndBind),
        Perturbation::new(|record| set_reply_usage(record, |usage| usage.output_tokens = 70_032))
            .reaching("messages", "usage_output", Integer(70_032), DigestAndBind),
        Perturbation::new(|record| {
            set_reply_usage(record, |usage| usage.cache_creation_input_tokens = 70_033)
        })
        .reaching(
            "messages",
            "usage_cache_creation",
            Integer(70_033),
            DigestAndBind,
        ),
        Perturbation::new(|record| {
            set_reply_usage(record, |usage| usage.cache_read_input_tokens = 70_034)
        })
        .reaching(
            "messages",
            "usage_cache_read",
            Integer(70_034),
            DigestAndBind,
        ),
    ]
}

fn part_perturbations() -> Vec<Perturbation> {
    use ExpectedValue::{Blob, Changed, Integer, Text};
    vec![
        // An empty text and an empty thinking block differ in their kind
        // alone: search reads neither.
        Perturbation::seeded(
            |record| set_first_block_text(record, ""),
            |record| {
                change_first_block(record, |block| match block {
                    ContentBlock::Text { text } if text.is_empty() => {
                        *block = ContentBlock::Thinking {
                            thinking: String::new(),
                            signature: String::new(),
                        };
                        true
                    }
                    _ => false,
                })
            },
        )
        .reaching("parts", "kind", Text("thinking"), DigestAndBind),
        // Search reads a reply's text in the full field and its thinking in
        // the thinking field, each from the first block holding it. Once
        // the text equals the thinking, the two blocks swap fields: the
        // thinking block keeps its kind and its text.
        Perturbation::seeded(
            |record| {
                change_first_entry(record, |entry| match entry {
                    LogEntry::Assistant { message, .. } => {
                        message.content = vec![
                            ContentBlock::Thinking {
                                thinking: SHARED_TEXT.to_owned(),
                                signature: String::new(),
                            },
                            ContentBlock::Text {
                                text: "perturbed reply text".to_owned(),
                            },
                        ];
                        true
                    }
                    _ => false,
                })
            },
            |record| {
                change_first_entry(record, |entry| match entry {
                    LogEntry::Assistant { message, .. } => match message.content.as_mut_slice() {
                        [
                            ContentBlock::Thinking { thinking, .. },
                            ContentBlock::Text { text },
                        ] if thinking == SHARED_TEXT => {
                            *text = SHARED_TEXT.to_owned();
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                })
            },
        )
        .reaching(
            "parts",
            "search_field",
            ExpectedValue::InRowsOfText {
                text: SHARED_TEXT,
                values: [SearchField::Full, SearchField::Thinking]
                    .map(|field| rusqlite::types::Value::Integer(field.code()))
                    .into(),
            },
            DigestAndBind,
        ),
        Perturbation::new(|record| {
            change_first_block(record, |block| match block {
                ContentBlock::ToolUse { id, .. } => {
                    *id = "perturbed-call".to_owned();
                    true
                }
                _ => false,
            })
        })
        .reaching("parts", "call_id", Text("perturbed-call"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_block(record, |block| match block {
                ContentBlock::ToolUse { name, .. } => {
                    *name = "perturbed-tool-name".to_owned();
                    true
                }
                _ => false,
            })
        })
        .reaching("parts", "name", Text("perturbed-tool-name"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_block(record, |block| match block {
                ContentBlock::ToolUse { tool, .. } if *tool != Tool::WebSearch => {
                    *tool = Tool::WebSearch;
                    true
                }
                _ => false,
            })
        })
        .reaching("parts", "tool", Text("web_search"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_block(record, |block| match block {
                ContentBlock::ToolUse { input, .. } => {
                    *input = json!({"perturbed": 70_041});
                    true
                }
                _ => false,
            })
        })
        .reaching(
            "parts",
            "json",
            Text(r#"{"perturbed":70041}"#),
            DigestAndBind,
        ),
        Perturbation::new(|record| {
            change_first_block(record, |block| match block {
                ContentBlock::ToolUse {
                    tool: Tool::Agent,
                    input,
                    ..
                } => {
                    *input = json!({"description": "perturbed description"});
                    true
                }
                _ => false,
            })
        })
        .reaching(
            "parts",
            "call_context",
            Text(r#"{"description":"perturbed description"}"#),
            Claim::BindOnly("the change also changes `json`"),
        )
        .reaching(
            "parts",
            "json",
            Text(r#"{"description":"perturbed description"}"#),
            Claim::BindOnly("the change also changes `call_context`"),
        ),
        Perturbation::new(|record| {
            change_first_block(record, |block| match block {
                ContentBlock::ToolResult {
                    content: Some(content @ serde_json::Value::String(_)),
                    ..
                } => {
                    *content = json!({"perturbed": 70_042});
                    true
                }
                _ => false,
            })
        })
        .reaching(
            "parts",
            "result_shape",
            Text("json"),
            Claim::BindOnly("the change also changes `text`, `json` and `search_field`"),
        )
        .reaching(
            "parts",
            "json",
            Text(r#"{"perturbed":70042}"#),
            Claim::BindOnly("the change also changes `text`, `result_shape` and `search_field`"),
        ),
        Perturbation::new(|record| set_first_block_text(record, "perturbed part text"))
            .reaching("parts", "text", Text("perturbed part text"), DigestAndBind)
            .reaching(
                "semantic_turns",
                "text",
                Changed,
                Claim::BindOnly("the change also changes `parts.text`"),
            ),
        Perturbation::new(|record| set_first_block_text(record, TEXT_WITH_CODES)).reaching(
            "parts",
            "text_codes",
            Blob(TEXT_CODES),
            CHANGES_TEXT,
        ),
        Perturbation::new(|record| {
            add_subagent_metadata(record, METADATA_FILE, "perturbed metadata text")
        })
        .reaching(
            "parts",
            "kind",
            Text("subagent-metadata"),
            OUTSIDE_EVERY_ENTRY,
        )
        .reaching(
            "parts",
            "search_field",
            Integer(SearchField::Supplement.code()),
            OUTSIDE_EVERY_ENTRY,
        )
        .reaching(
            "parts",
            "file_position",
            Integer(METADATA_FILE as i64),
            OUTSIDE_EVERY_ENTRY,
        )
        .reaching(
            "parts",
            "text",
            Text("perturbed metadata text"),
            OUTSIDE_EVERY_ENTRY,
        ),
        // A string item and an object item holding only a text differ in
        // their kind alone.
        Perturbation::seeded(
            |record| {
                change_first_result_item(record, |item| {
                    item.as_object_mut()
                        .is_some_and(|object| object.remove("type").is_some())
                })
            },
            |record| {
                change_first_result_item(record, |item| {
                    let typeless = item
                        .as_object()
                        .is_some_and(|object| !object.contains_key("type"));
                    match item.get("text").and_then(serde_json::Value::as_str) {
                        Some(text) if typeless => {
                            *item = json!(text);
                            true
                        }
                        _ => false,
                    }
                })
            },
        )
        .reaching("result_items", "kind", Text("string"), DigestAndBind),
        Perturbation::new(|record| {
            change_first_result_item(record, |item| {
                item["text"] = json!("perturbed item text");
                true
            })
        })
        .reaching(
            "result_items",
            "text",
            Text("perturbed item text"),
            DigestAndBind,
        ),
        Perturbation::new(|record| {
            change_first_result_item(record, |item| {
                item["type"] = json!("perturbed item type");
                true
            })
        })
        .reaching(
            "result_items",
            "item_type",
            Text("perturbed item type"),
            DigestAndBind,
        ),
        Perturbation::new(|record| {
            change_first_result_item(record, |item| {
                item["text"] = json!(TEXT_WITH_CODES);
                true
            })
        })
        .reaching("result_items", "text_codes", Blob(TEXT_CODES), CHANGES_TEXT),
    ]
}

/// A `sessions` column, the change to the catalogue row that sets it, and
/// the value it then holds.
type SessionColumn = (&'static str, fn(&mut CatalogueRow), ExpectedValue);

/// A `transcript_reads` column, the change to the session file that sets
/// it, and the value it then holds.
type ReadColumn = (&'static str, fn(&mut SessionFile), ExpectedValue);

fn catalogue_perturbations() -> Vec<Perturbation> {
    use ExpectedValue::{Blob, Integer, Text};
    let sessions: [SessionColumn; 20] = [
        (
            "source",
            |row| row.source = "perturbed-source",
            Text("perturbed-source"),
        ),
        (
            "file_size",
            |row| row.fingerprint.file_size = 70_101,
            Integer(70_101),
        ),
        (
            "mtime_secs",
            |row| row.fingerprint.mtime_secs = 70_102,
            Integer(70_102),
        ),
        (
            "mtime_nsecs",
            |row| row.fingerprint.mtime_nsecs = 70_103,
            Integer(70_103),
        ),
        (
            "session_id",
            |row| row.session_id = Some("perturbed-id".to_owned()),
            Text("perturbed-id"),
        ),
        (
            "session_id_key",
            |row| row.session_id_key = Some("perturbed-key".to_owned()),
            Text("perturbed-key"),
        ),
        (
            "handle",
            |row| row.handle = Some("perturbed-handle".to_owned()),
            Text("perturbed-handle"),
        ),
        (
            "project_path",
            |row| row.project_path = Some("perturbed-project-path".to_owned()),
            Text("perturbed-project-path"),
        ),
        (
            "project_name",
            |row| row.project_name = Some("perturbed-project-name".to_owned()),
            Text("perturbed-project-name"),
        ),
        (
            "cwd",
            |row| row.cwd = Some("perturbed-session-cwd".to_owned()),
            Text("perturbed-session-cwd"),
        ),
        (
            "timestamp_ms",
            |row| row.timestamp_ms = Some(70_104),
            Integer(70_104),
        ),
        (
            "preview_first",
            |row| row.preview_first = Some("perturbed-first".to_owned()),
            Text("perturbed-first"),
        ),
        (
            "preview_last",
            |row| row.preview_last = Some("perturbed-last".to_owned()),
            Text("perturbed-last"),
        ),
        (
            "summary",
            |row| row.summary = Some("perturbed-summary".to_owned()),
            Text("perturbed-summary"),
        ),
        (
            "custom_title",
            |row| row.custom_title = Some("perturbed-title".to_owned()),
            Text("perturbed-title"),
        ),
        (
            "model",
            |row| row.model = Some("perturbed-session-model".to_owned()),
            Text("perturbed-session-model"),
        ),
        (
            "total_tokens",
            |row| row.total_tokens = Some(70_105),
            Integer(70_105),
        ),
        (
            "duration_minutes",
            |row| row.duration_minutes = Some(70_106),
            Integer(70_106),
        ),
        (
            "assistant_messages",
            |row| row.assistant_messages = Some(70_107),
            Integer(70_107),
        ),
        (
            "is_missing_earlier_turns",
            |row| row.is_missing_earlier_turns = Some(true),
            Integer(1),
        ),
    ];
    let reads: [ReadColumn; 7] = [
        (
            "cursor",
            |file| file.read.cursor = b"perturbed cursor".to_vec(),
            Blob(b"perturbed cursor"),
        ),
        (
            "stable_cursor",
            |file| file.read.stable_cursor = b"perturbed stable cursor".to_vec(),
            Blob(b"perturbed stable cursor"),
        ),
        (
            "identity",
            |file| file.read.identity = b"perturbed identity".to_vec(),
            Blob(b"perturbed identity"),
        ),
        (
            "rewrite_check",
            |file| file.read.rewrite_check = b"perturbed rewrite check".to_vec(),
            Blob(b"perturbed rewrite check"),
        ),
        (
            "parser_state",
            |file| file.read.parser_state = b"perturbed parser state".to_vec(),
            Blob(b"perturbed parser state"),
        ),
        (
            "stable_line",
            |file| file.read.stable_line = Some(70_301),
            Integer(70_301),
        ),
        (
            "fold_state",
            |file| file.fold = b"perturbed fold".to_vec(),
            Blob(b"perturbed fold"),
        ),
    ];
    let session_perturbations = sessions.into_iter().map(|(column, set, stored)| {
        Perturbation::new(move |record| {
            set(&mut record.catalogue);
            true
        })
        .reaching("sessions", column, stored, NO_DIGEST)
    });
    let read_perturbations = reads.into_iter().map(|(column, set, stored)| {
        Perturbation::new(move |record| record.catalogue.files.last_mut().map(set).is_some())
            .reaching("transcript_reads", column, stored, NO_DIGEST)
    });
    let mut perturbations: Vec<Perturbation> =
        session_perturbations.chain(read_perturbations).collect();
    perturbations.extend([
        // In a write of its own: the stored copy of an empty session is not
        // a listed copy, and the write changing it back inserts the session
        // whole, without comparing an entry change's digest.
        Perturbation::new(|record| {
            record.catalogue.status = SessionStatus::Empty;
            true
        })
        .reaching("sessions", "status", Text("empty"), NO_DIGEST)
        .in_own_write(),
        // The change drops the parse's entries: no change to them applies
        // after it.
        Perturbation::new(|record| {
            record.catalogue.status = SessionStatus::Unreadable;
            record.entries = None;
            record.unlisted_reason = Some("perturbed unlisted reason".to_owned());
            record.subagent_metadata.clear();
            record.checkpoint = None;
            true
        })
        .reaching(
            "sessions",
            "entries_error",
            Text("perturbed unlisted reason"),
            NO_DIGEST,
        )
        .reaching("sessions", "status", Text("unreadable"), NO_DIGEST)
        .in_own_write(),
        Perturbation::new(|record| {
            record
                .catalogue
                .files
                .last_mut()
                .map(|file| {
                    file.fingerprint = Some(CachedFingerprint {
                        file_size: 70_201,
                        mtime_secs: 70_202,
                        mtime_nsecs: 70_203,
                    })
                })
                .is_some()
        })
        .reaching("session_files", "file_size", Integer(70_201), NO_DIGEST)
        .reaching("session_files", "mtime_secs", Integer(70_202), NO_DIGEST)
        .reaching("session_files", "mtime_nsecs", Integer(70_203), NO_DIGEST),
        Perturbation::new(set_parse_error)
            .reaching("parse_errors", "line_number", Integer(70_401), NO_DIGEST)
            .reaching(
                "parse_errors",
                "line_content",
                Text("perturbed error line"),
                NO_DIGEST,
            )
            .reaching(
                "parse_errors",
                "error_message",
                Text("perturbed error message"),
                NO_DIGEST,
            )
            .reaching(
                "parse_errors",
                "context_before",
                Text(r#"["perturbed error before"]"#),
                NO_DIGEST,
            )
            .reaching(
                "parse_errors",
                "context_after",
                Text(r#"["perturbed error after"]"#),
                NO_DIGEST,
            ),
        Perturbation::new(set_malformed_line)
            .reaching("malformed_lines", "line_number", Integer(70_501), NO_DIGEST)
            .reaching(
                "malformed_lines",
                "line_content",
                Text("perturbed malformed line"),
                NO_DIGEST,
            )
            .reaching(
                "malformed_lines",
                "error_message",
                Text("perturbed malformed message"),
                NO_DIGEST,
            )
            .reaching(
                "malformed_lines",
                "context_before",
                Text(r#"["perturbed malformed before"]"#),
                NO_DIGEST,
            )
            .reaching(
                "malformed_lines",
                "context_after",
                Text(r#"["perturbed malformed after"]"#),
                NO_DIGEST,
            ),
    ]);
    perturbations
}

fn thread_perturbations() -> Vec<Perturbation> {
    [
        ("nickname", "nickname", "perturbed-nickname"),
        ("role", "role", "perturbed-thread-role"),
        ("agentPath", "agent_path", "perturbed-agent-path"),
        ("agentId", "agent_id", "perturbed-agent-id"),
    ]
    .into_iter()
    .map(|(key, column, value)| {
        Perturbation::new(move |record| set_progress_identity(record, key, value)).reaching(
            "threads",
            column,
            ExpectedValue::Text(value),
            NO_DIGEST,
        )
    })
    .collect()
}

/// Gives the session a parse error, in place of its first.
fn set_parse_error(record: &mut SessionRecord) -> bool {
    let error = ParseError {
        line_number: 70_401,
        line_content: "perturbed error line".to_owned(),
        error_message: "perturbed error message".to_owned(),
        context_before: vec!["perturbed error before".to_owned()],
        context_after: vec!["perturbed error after".to_owned()],
    };
    let errors = &mut record.catalogue.parse_errors;
    match errors.first_mut() {
        Some(first) => *first = error,
        None => errors.push(error),
    }
    true
}

/// Gives the session a malformed line, in place of its first.
fn set_malformed_line(record: &mut SessionRecord) -> bool {
    let line = MalformedLine {
        line_number: 70_501,
        detail: Some(MalformedLineDetail {
            line_content: "perturbed malformed line".to_owned(),
            error_message: "perturbed malformed message".to_owned(),
            context_before: vec!["perturbed malformed before".to_owned()],
            context_after: vec!["perturbed malformed after".to_owned()],
        }),
    };
    let lines = &mut entries_of(record).malformed_lines;
    match lines.first_mut() {
        Some(first) => *first = line,
        None => lines.push(line),
    }
    true
}

/// Sets `key` of the agent identity the first sub-agent progress record
/// carries.
fn set_progress_identity(record: &mut SessionRecord, key: &str, value: &str) -> bool {
    change_first_entry(record, |entry| match entry {
        LogEntry::Progress { data, .. }
            if data.get("type").and_then(serde_json::Value::as_str) == Some("agent_progress")
                && data
                    .get("agentId")
                    .is_some_and(serde_json::Value::is_string) =>
        {
            data["identity"][key] = json!(value);
            true
        }
        _ => false,
    })
}

/// Adds a sub-agent metadata part holding `text` on the session file at
/// `position`, a sub-agent transcript.
fn add_subagent_metadata(record: &mut SessionRecord, position: usize, text: &str) -> bool {
    let on_subagent = record
        .catalogue
        .files
        .get(position)
        .is_some_and(|file| file.kind == "subagent");
    if on_subagent {
        record.subagent_metadata.push(SubagentMetadataPart {
            file_position: position,
            text: text.to_owned(),
        });
    }
    on_subagent
}

/// Applies `change` to the first entry of `record` it applies to; false
/// when it applies to none.
fn change_first_entry(
    record: &mut SessionRecord,
    mut change: impl FnMut(&mut LogEntry) -> bool,
) -> bool {
    entries_of(record)
        .entries
        .iter_mut()
        .any(|(_, entry)| change(entry))
}

/// Applies `change` to the last entry of `record` it applies to. A message
/// keeps the last of its entries' model and agent.
fn change_last_entry(
    record: &mut SessionRecord,
    mut change: impl FnMut(&mut LogEntry) -> bool,
) -> bool {
    entries_of(record)
        .entries
        .iter_mut()
        .rev()
        .any(|(_, entry)| change(entry))
}

fn change_first_block(
    record: &mut SessionRecord,
    mut change: impl FnMut(&mut ContentBlock) -> bool,
) -> bool {
    change_first_entry(record, |entry| {
        blocks_of(entry).is_some_and(|blocks| blocks.iter_mut().any(&mut change))
    })
}

/// Applies `change` to the first item of the first result whose first item
/// is a text item.
fn change_first_result_item(
    record: &mut SessionRecord,
    change: impl Fn(&mut serde_json::Value) -> bool,
) -> bool {
    change_first_block(record, |block| match block {
        ContentBlock::ToolResult {
            content: Some(serde_json::Value::Array(items)),
            ..
        } => items.first_mut().is_some_and(|item| {
            item.get("text").is_some_and(serde_json::Value::is_string) && change(item)
        }),
        _ => false,
    })
}

/// The timestamp a user, assistant or metadata entry carries.
fn timestamp_of(entry: &mut LogEntry) -> Option<&mut String> {
    match entry {
        LogEntry::User { timestamp, .. }
        | LogEntry::Assistant { timestamp, .. }
        | LogEntry::PiMetadata { timestamp, .. } => timestamp.as_mut(),
        _ => None,
    }
}

/// Sets the time of the first `agent_progress` record of the session's own
/// transcript. Its `splice_key` is null: the change leaves that column
/// alone.
fn set_unspliced_progress_time(record: &mut SessionRecord, time: &str) -> bool {
    entries_of(record)
        .entries
        .iter_mut()
        .any(|(line, entry)| match entry {
            LogEntry::Progress { data, extra }
                if line.transcript.is_none()
                    && data.get("type").and_then(serde_json::Value::as_str)
                        == Some("agent_progress")
                    && extra.get("timestamp").is_some() =>
            {
                extra["timestamp"] = json!(time);
                true
            }
            _ => false,
        })
}

/// Sets the time of the first assistant entry holding text. The entry
/// contributes to a numbered message, and the message row stores the
/// earliest and latest times of its contributing records.
fn set_first_reply_timestamp(record: &mut SessionRecord, time: &str) -> bool {
    change_first_entry(record, |entry| match entry {
        LogEntry::Assistant {
            message,
            timestamp: Some(timestamp),
            ..
        } if message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { .. })) =>
        {
            *timestamp = time.to_owned();
            true
        }
        _ => false,
    })
}

/// Gives every record of the first reply the id `id`: a reply's records
/// share its id.
fn rename_first_reply(record: &mut SessionRecord, id: &str) -> bool {
    let entries = &mut entries_of(record).entries;
    let Some(first) = entries.iter().find_map(|(_, entry)| match entry {
        LogEntry::Assistant { message, .. } => message.id.clone(),
        _ => None,
    }) else {
        return false;
    };
    for (_, entry) in entries.iter_mut() {
        if let LogEntry::Assistant { message, .. } = entry
            && message.id.as_deref() == Some(first.as_str())
        {
            message.id = Some(id.to_owned());
        }
    }
    true
}

fn set_first_entry_text(record: &mut SessionRecord, new: &str) -> bool {
    change_first_entry(record, |entry| match entry {
        LogEntry::Summary { summary: text }
        | LogEntry::CustomTitle { custom_title: text }
        | LogEntry::AiTitle { ai_title: text }
        | LogEntry::AgentName { agent_name: text }
        | LogEntry::PiMetadata { text, .. } => {
            *text = new.to_owned();
            true
        }
        _ => false,
    })
}

/// Changes one count, with `set`, of the first metadata row that is not
/// searchable and carries usage. Such a row is outside every message, and
/// its entry row stores its usage.
fn set_metadata_usage(record: &mut SessionRecord, set: impl Fn(&mut TokenUsage)) -> bool {
    change_first_entry(record, |entry| match entry {
        LogEntry::PiMetadata {
            searchable: false,
            usage: Some(usage),
            ..
        } => {
            set(usage);
            true
        }
        _ => false,
    })
}

/// Changes one count, with `set`, of the last reply carrying usage: a
/// message keeps the usage of the last of its entries carrying one.
fn set_reply_usage(record: &mut SessionRecord, set: impl Fn(&mut TokenUsage)) -> bool {
    entries_of(record)
        .entries
        .iter_mut()
        .rev()
        .find_map(|(_, entry)| match entry {
            LogEntry::Assistant { message, .. } => message.usage.as_mut(),
            _ => None,
        })
        .map(set)
        .is_some()
}

/// Sets the text of the tag `tag` in the first task report a user entry
/// holds as its text, or removes the tag for `None`.
fn set_report_tag(record: &mut SessionRecord, tag: &str, text: Option<&str>) -> bool {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    change_first_entry(record, |entry| match entry {
        LogEntry::User {
            message:
                UserMessage {
                    content: UserContent::String(report),
                    ..
                },
            ..
        } if report.starts_with("<task-notification>") => {
            let (Some(start), Some(end)) = (report.find(&open), report.find(&close)) else {
                return false;
            };
            let replacement = text
                .map(|text| format!("{open}{text}{close}"))
                .unwrap_or_default();
            report.replace_range(start..end + close.len(), &replacement);
            true
        }
        _ => false,
    })
}

fn set_first_block_text(record: &mut SessionRecord, new: &str) -> bool {
    change_first_block(record, |block| match block {
        ContentBlock::Text { text } => {
            *text = new.to_owned();
            true
        }
        _ => false,
    })
}
