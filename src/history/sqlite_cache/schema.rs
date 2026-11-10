//! The session cache database's tables. Change [`SCHEMA_VERSION`] with any
//! change here: a database at another version is emptied and rebuilt.

/// The version of the tables below, stored in `meta` under
/// [`SCHEMA_VERSION_KEY`].
pub(super) const SCHEMA_VERSION: u32 = 13;
pub(super) const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Every table, dropped in this order when the schema changes.
pub(super) const TABLES: &[&str] = &[
    "search_texts",
    "subagent_search_parts",
    "result_items",
    "parts",
    "entries",
    "messages",
    "malformed_lines",
    "threads",
    "transcripts",
    "parse_errors",
    "session_files",
    "sessions",
    "meta",
];

/// The tables and their indexes. Every table below `sessions` is keyed by
/// the session it belongs to and deleted with it.
///
/// A `text` column the viewer shows holds its text without terminal codes;
/// the `text_codes` column beside it holds the codes, null for a text without
/// any (see `split_terminal_codes`). A search-only `text` column holds the
/// text without codes and drops them.
pub(super) const CREATE: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- The catalogue: one row per discovered session, with what the session list
-- and `agent sessions` show and filter on.
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The storage that found the session: `SessionCache::directory`.
    storage TEXT NOT NULL,
    source TEXT NOT NULL,
    root TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    -- As discovery spells it.
    locator TEXT NOT NULL,
    -- The locator's `FileKey`: the root's canonical path joined with the
    -- locator's path beneath the root, lowercased on a file system that
    -- ignores case.
    file_key TEXT NOT NULL,
    -- `writing` until every row of the session is written, then `listed`,
    -- `empty` or `unreadable`.
    status TEXT NOT NULL,
    -- The fingerprint of the session's transcripts the rows were read at.
    file_size INTEGER NOT NULL,
    mtime_secs INTEGER NOT NULL,
    mtime_nsecs INTEGER NOT NULL,
    session_id TEXT,
    -- `session_id` as a lookup compares it: lowercase for a UUID.
    session_id_key TEXT,
    -- The full digest of the session's `ch_` handle.
    handle TEXT,
    project_path TEXT,
    project_name TEXT,
    cwd TEXT,
    timestamp_ms INTEGER,
    preview_first TEXT,
    preview_last TEXT,
    summary TEXT,
    custom_title TEXT,
    model TEXT,
    total_tokens INTEGER,
    duration_minutes INTEGER,
    message_count INTEGER,
    assistant_messages INTEGER,
    is_missing_earlier_turns INTEGER,
    -- The `session_entries` failure for a listed session; its entries are
    -- then missing.
    entries_error TEXT,
    -- A number no earlier write of any session in this database carried,
    -- from the `last_generation` meta counter. Every write gives the row a
    -- new one, so a reader holding the old number sees the copy change.
    generation INTEGER NOT NULL,
    UNIQUE (storage, root, cache_key)
);
CREATE INDEX IF NOT EXISTS sessions_by_session_id ON sessions (session_id_key);
CREATE INDEX IF NOT EXISTS sessions_by_locator ON sessions (locator);
CREATE INDEX IF NOT EXISTS sessions_by_file_key ON sessions (file_key);
CREATE INDEX IF NOT EXISTS sessions_by_handle ON sessions (handle);
CREATE INDEX IF NOT EXISTS sessions_by_time ON sessions (timestamp_ms);

-- The session's own transcript (position 0) and each sub-agent transcript,
-- in the order discovery named them, each with its fingerprint when written.
CREATE TABLE IF NOT EXISTS session_files (
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('locator', 'subagent', 'continuation')),
    -- As discovery, or the transcript that names the file, spells it.
    path TEXT NOT NULL,
    -- The file's `FileKey`, as `sessions.file_key` keys the locator.
    key TEXT NOT NULL,
    file_size INTEGER,
    mtime_secs INTEGER,
    mtime_nsecs INTEGER,
    PRIMARY KEY (session, position)
) WITHOUT ROWID;

-- Searchable child metadata omitted by the dialogue splice, one text part
-- per row. It has no message number and is excluded by active time windows.
CREATE TABLE IF NOT EXISTS subagent_search_parts (
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    -- The `session_files` position of the sub-agent transcript holding the
    -- text.
    file_position INTEGER NOT NULL,
    position INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (session, position)
) WITHOUT ROWID;

-- Each search text of an entry that no row holds as written, such as a tool
-- call's header or a task report's text: the text and its search field, as
-- on the rows that hold text (see `parts`).
CREATE TABLE IF NOT EXISTS search_texts (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    -- Gives the text its message number. Deleted with the session.
    entry INTEGER NOT NULL,
    search_field INTEGER NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS search_texts_by_session ON search_texts (session);
CREATE INDEX IF NOT EXISTS search_texts_by_entry ON search_texts (entry);

-- The session row's parse errors.
CREATE TABLE IF NOT EXISTS parse_errors (
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    line_number INTEGER NOT NULL,
    line_content TEXT NOT NULL,
    error_message TEXT NOT NULL,
    context_before TEXT NOT NULL,
    context_after TEXT NOT NULL,
    PRIMARY KEY (session, position)
) WITHOUT ROWID;

-- The transcripts other than the session's own that entries were read
-- from: sub-agents', and earlier transcripts a session continues.
CREATE TABLE IF NOT EXISTS transcripts (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    UNIQUE (session, path)
);

-- Thread 0 is the session's own; each later thread is one sub-agent's
-- turns: the transcript they came from and the agent label they carry.
-- Metadata versions share a number so a rename doesn't restart positions.
CREATE TABLE IF NOT EXISTS threads (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    transcript INTEGER REFERENCES transcripts (id),
    label TEXT,
    nickname TEXT,
    role TEXT,
    agent_path TEXT,
    agent_id TEXT,
    launch_call TEXT
);
CREATE INDEX IF NOT EXISTS threads_by_session ON threads (session, number);
CREATE INDEX IF NOT EXISTS threads_by_transcript ON threads (transcript)
    WHERE transcript IS NOT NULL;

-- Shared fields of a logical message. An ordinal is assigned when the
-- message has visible content; unnumbered source messages retain their
-- fields without taking a printed number. Times are Unix epoch milliseconds,
-- covering only records that contribute to the numbered message.
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    thread INTEGER NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    ordinal INTEGER,
    role TEXT,
    reply_id TEXT,
    first_ms INTEGER,
    last_ms INTEGER,
    model TEXT,
    agent TEXT,
    cwd TEXT,
    parent_tool_use_id TEXT,
    source_tool_use_id TEXT,
    -- NULL for an ordinary message, 0 for a finish notice, 1 for a report
    -- carrying text. Sender facts let bounded reads avoid the report body.
    report_has_text INTEGER,
    report_task_id TEXT,
    report_launch_call TEXT,
    usage_input INTEGER,
    usage_output INTEGER,
    usage_cache_creation INTEGER,
    usage_cache_read INTEGER,
    -- A hash of every column above except `thread` and `ordinal`, so a
    -- write compares a message with its new version without reading them.
    digest INTEGER NOT NULL,
    UNIQUE (session, ordinal)
);
CREATE INDEX IF NOT EXISTS messages_by_thread ON messages (thread, ordinal);
CREATE INDEX IF NOT EXISTS messages_by_time ON messages (last_ms, first_ms)
    WHERE ordinal IS NOT NULL;

-- `session_entries`' entries. `sequence` is the entry's place in the
-- session's entry stream; `position` its place in its thread.
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    -- Null for titles, system records and other non-message metadata.
    message INTEGER REFERENCES messages (id) ON DELETE CASCADE,
    -- Empty/hidden source records can supply metadata without contributing
    -- content or time to the numbered message.
    contributes INTEGER NOT NULL CHECK (contributes IN (0, 1)),
    position INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    record_transcript INTEGER REFERENCES transcripts (id),
    record_line INTEGER NOT NULL,
    kind TEXT NOT NULL,
    -- The time the entry carries: its own, or a sub-agent turn's.
    timestamp_ms INTEGER,
    -- `string` for user content recorded as one string, `blocks` for content
    -- held in `parts`; null for an entry without content in parts.
    content TEXT,
    -- Set on the title and the summary search reads, as on `parts`.
    search_field INTEGER,
    -- The entry's own text: a summary, a title, an agent name, a metadata
    -- row's text.
    text TEXT,
    text_codes BLOB,
    -- A metadata row's label, or a system record's subtype.
    label TEXT,
    uuid TEXT,
    -- Independent usage metadata stays here; message usage is stored once
    -- on messages, using the last recorded snapshot for that reply.
    usage_input INTEGER,
    usage_output INTEGER,
    usage_cache_creation INTEGER,
    usage_cache_read INTEGER,
    level TEXT,
    duration_ms INTEGER,
    parent_uuid TEXT,
    -- A metadata row's `searchable`.
    flag INTEGER,
    -- A hash of what the row stores besides its place in the session and
    -- its message (`message`, `contributes`, `position`, `sequence`,
    -- `record_transcript`, `search_field`), with its parts, result items and
    -- search texts. A write keeps the row and its children when the new
    -- entry hashes the same.
    digest INTEGER NOT NULL,
    UNIQUE (session, sequence)
);
CREATE INDEX IF NOT EXISTS entries_by_message ON entries (message, sequence);
CREATE INDEX IF NOT EXISTS entries_by_record_transcript ON entries (record_transcript)
    WHERE record_transcript IS NOT NULL;
CREATE INDEX IF NOT EXISTS entries_by_time ON entries (timestamp_ms)
    WHERE timestamp_ms IS NOT NULL;

-- Each content part retains its original position, including reference-only
-- images and unknown blocks.
CREATE TABLE IF NOT EXISTS parts (
    id INTEGER PRIMARY KEY,
    entry INTEGER NOT NULL REFERENCES entries (id) ON DELETE CASCADE,
    part INTEGER NOT NULL,
    kind TEXT NOT NULL,
    -- The search field lexical search reads `text` in, or a result's
    -- `result_items` in; null when search does not read the part. Before
    -- `text`, so reading it does not read a long text's overflow pages.
    search_field INTEGER,
    -- Text, thinking, or a result's content recorded as one string.
    text TEXT,
    text_codes BLOB,
    -- A call's id, or the call a result answers.
    call_id TEXT,
    -- A call's tool name, or a standalone result's.
    name TEXT,
    -- A call's tool kind.
    tool TEXT,
    -- A call's input, or a result's content as recorded when it is neither
    -- text nor an array of text items. Image payloads stay in the source.
    json TEXT,
    -- `array` for a result whose items `result_items` holds, `json` for one
    -- `json` holds, null otherwise.
    result_shape TEXT,
    UNIQUE (entry, part)
);

-- A result array's original item positions, including reference-only
-- items. Text is kept separately from image and other opaque payloads.
CREATE TABLE IF NOT EXISTS result_items (
    part INTEGER NOT NULL REFERENCES parts (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    -- 1 for a text item of a result search reads: search reads the part's
    -- searched items as one text, joined by line breaks.
    searched INTEGER NOT NULL,
    text TEXT,
    text_codes BLOB,
    item_type TEXT,
    PRIMARY KEY (part, position)
) WITHOUT ROWID;

-- The lines of the session's transcript that did not parse as an entry.
CREATE TABLE IF NOT EXISTS malformed_lines (
    session INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    line_number INTEGER NOT NULL,
    -- All four are null for a format that records only the line's number.
    line_content TEXT,
    error_message TEXT,
    context_before TEXT,
    context_after TEXT,
    PRIMARY KEY (session, position)
) WITHOUT ROWID;
"#;
