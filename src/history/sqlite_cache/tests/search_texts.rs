//! The search texts a cache write stores, against the texts the session's
//! content rebuilds from its stored entries.

use super::*;
use crate::history::Conversation;
use crate::history::parser::normalize_whitespace;
use crate::history::provider::Capability;
use crate::history::provider::contract_tests::registered_fixtures;
use crate::time_filter::TimeFilter;
use std::ops::ControlFlow;

/// A search text's message number and normalised text.
type PlacedText = (Option<usize>, String);

#[test]
fn write_stores_each_text_content_rebuilds_once_for_every_provider() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    for session in every_record_sessions() {
        let row = &session.row;
        let storage = storage_of(row.source);
        cache.admit_storage(storage).unwrap();
        let root = root_of(&session);
        let stub = stub_of(&session, 1);
        cache
            .write(&parsed_record(row.source, &root, &stub))
            .unwrap();
        let CatalogueSession::Listed(listed) = cache
            .catalogue_session(storage, row.source, &root, &stub.cache_key, false)
            .unwrap()
            .unwrap()
        else {
            panic!("listed fixture")
        };
        let copy = listed.cached_session.as_ref().unwrap();

        for (fields, search) in [
            (SearchedFields::List, "the list's search"),
            (SearchedFields::Agent, "`agent search`"),
        ] {
            let rebuilt = sorted(rebuilt_texts(copy, &listed, fields));
            let stored = sorted(stored_texts(copy, fields));

            assert!(
                !rebuilt.is_empty(),
                "{}: the content rebuilds the texts {search} reads",
                row.source.label()
            );
            assert_eq!(
                stored,
                rebuilt,
                "{}: each text {search} reads in the content is stored once, with its message \
                 number",
                row.source.label()
            );
        }
    }
}

/// The texts in `fields` that `listed`'s content rebuilds from the entries
/// of `copy`, its copy, as `Conversation::load_content` reads them. Semantic
/// text is not stored.
fn rebuilt_texts(
    copy: &CachedSession,
    listed: &Conversation,
    fields: SearchedFields,
) -> Vec<PlacedText> {
    let reads = |field| match (fields, field) {
        (_, SearchField::Semantic) => false,
        (SearchedFields::List, field) => field == SearchField::Full,
        (SearchedFields::Agent, _) => true,
    };
    let mut texts = Vec::new();
    copy.search_parts(listed, |part| {
        if reads(part.field) {
            texts.push((part.ordinal, part.text));
        }
    })
    .unwrap()
    .expect("the cache holds the copy");
    texts
}

/// The texts `copy` stores in `fields`, normalised as search reads them:
/// whitespace collapsed. Text left empty is dropped, as the content drops
/// it.
fn stored_texts(copy: &CachedSession, fields: SearchedFields) -> Vec<PlacedText> {
    let mut texts = Vec::new();
    copy.read_search_texts(fields, &TimeFilter::default(), |text| {
        let normalised = normalize_whitespace(text.text);
        if !normalised.is_empty() {
            texts.push((text.ordinal, normalised));
        }
        ControlFlow::Continue(())
    })
    .unwrap()
    .expect("the cache holds the copy");
    texts
}

/// The lines of a Claude session holding every kind of text the list's
/// search reads: a title and a summary, a reply written as two records, a
/// tool result's items after a text block in its entry, and a command's
/// derived text.
fn every_layout_kind_lines() -> Vec<String> {
    let user = |content: serde_json::Value| serde_json::json!({"type": "user", "message": {"role": "user", "content": content}});
    let reply = |content: serde_json::Value| {
        serde_json::json!({"type": "assistant", "message": {
            "id": "reply", "role": "assistant", "content": content}})
    };
    [
        serde_json::json!({"type": "custom-title", "customTitle": "Layout title"}),
        serde_json::json!({"type": "summary", "summary": "Layout summary", "leafUuid": "l"}),
        user(serde_json::json!("first question")),
        reply(serde_json::json!([{"type": "text", "text": "answer part one"}])),
        reply(serde_json::json!([
            {"type": "text", "text": "answer part two"},
            {"type": "tool_use", "id": "call", "name": "Bash", "input": {"command": "ls"}},
        ])),
        user(serde_json::json!([
            {"type": "tool_result", "tool_use_id": "call", "content": [
                {"type": "text", "text": "item one"},
                {"type": "text", "text": "item two"},
            ]},
            {"type": "text", "text": "after the result"},
        ])),
        user(serde_json::json!(
            "<command-name>/model</command-name>\n<command-args>opus</command-args>"
        )),
    ]
    .iter()
    .map(serde_json::Value::to_string)
    .collect()
}

/// `body` after the row's title and summary, each after a separator: the
/// text the list's evidence reads.
fn after_leading_texts(row: &Conversation, body: &str) -> String {
    let mut text = String::new();
    for piece in row.leading_texts().chain([body]) {
        crate::history::message_texts::push_separated(&mut text, piece);
    }
    text
}

/// The parse lays out a row's list search texts in each entry's block
/// order, whitespace collapsed. The cache's copy keeps the text as written
/// and groups each message's texts by kind: blocks, then results' items,
/// then derived texts. The two orders differ only in the snippets a context
/// line picks: scores read each text alone. The session's derived text is a
/// message of its own: the test doesn't observe its place after the blocks.
/// The order doesn't change `agent search`'s output: it doesn't match a
/// query across the separator between two texts, tool-call headers
/// included, and reads its hits from the transcript.
#[test]
fn parse_lays_out_texts_in_block_order_and_cache_groups_them_by_kind() {
    let laid_out_by_kind = [
        "Layout title",
        "Layout summary",
        "first question",
        "answer part one",
        "answer part two",
        "after the result",
        "item one\nitem two",
        "/model opus",
    ]
    .join(" \u{1e} ");
    let laid_out_in_block_order = [
        "Layout title",
        "Layout summary",
        "first question",
        "answer part one",
        "answer part two",
        "item one item two",
        "after the result",
        "/model opus",
    ]
    .join(" \u{1e} ");
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("layout.jsonl");
    let lines = every_layout_kind_lines();
    std::fs::write(&path, lines.join("\n")).unwrap();
    let session = AgentSession::new(Source::Claude, path.clone(), Vec::new(), Vec::new());
    let (_, parsed) =
        parsed_record_and_row(Source::Claude, &root_of(&session), &stub_of(&session, 1));
    assert_eq!(
        parsed.full_text_after_leading_texts(),
        laid_out_in_block_order,
        "the parse lays out the texts in block order"
    );

    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    cache.admit_storage(storage_of(Source::Claude)).unwrap();
    let mut laid_out = Vec::new();
    for (written, read) in [(lines.len(), "a write"), (4, "an update")] {
        std::fs::write(&path, lines[..written].join("\n")).unwrap();
        let (record, root) = source_record(Source::Claude, &path);
        cache.write(&record).unwrap();
        if written < lines.len() {
            std::fs::write(&path, lines.join("\n")).unwrap();
            let (record, _) = source_record(Source::Claude, &path);
            cache.write(&record).unwrap();
        }
        let CatalogueSession::Listed(listed) = cache
            .catalogue_session(
                storage_of(Source::Claude),
                Source::Claude,
                &root,
                CACHE_KEY,
                false,
            )
            .unwrap()
            .unwrap()
        else {
            panic!("the session is listed")
        };
        let copy = listed.cached_session.as_ref().unwrap();
        let cached = crate::search::lexical::SessionSearchTexts::read(copy)
            .unwrap()
            .expect("the cache holds the copy");
        laid_out.push((
            format!("the copy {read} left"),
            after_leading_texts(&listed, cached.joined()),
        ));
        let rows = [*listed];
        let scanned = crate::search::SearchTextsInMemory::read(&rows);
        laid_out.push((
            format!("a scan of the copy {read} left"),
            after_leading_texts(&rows[0], scanned.get(&rows[0]).unwrap().joined()),
        ));
    }
    for (read, text) in laid_out {
        assert_eq!(text, laid_out_by_kind, "{read} groups the texts by kind");
    }
}

/// A session's title and summary, trimmed.
type TitleAndSummary = (Option<String>, Option<String>);

#[test]
fn search_reads_the_title_and_summary_the_parse_names_for_every_provider() {
    let mut titled = 0;
    for session in every_record_sessions() {
        let source = session.row.source;
        let (searched, parsed) = searched_and_parsed_titles(&session);
        titled += usize::from(parsed.0.is_some());
        assert_eq!(
            searched,
            parsed,
            "{}: search reads the title and summary the parse names",
            source.label()
        );

        let (_copy, renamed) = renamed_copy(session, "  Renamed by hand  ");
        let (searched, parsed) = searched_and_parsed_titles(&renamed);
        assert_eq!(
            searched.0.as_deref(),
            Some("Renamed by hand"),
            "{}: search reads the title the rename gave",
            source.label()
        );
        assert_eq!(
            searched,
            parsed,
            "{}: search reads the renamed title and the summary the parse names",
            source.label()
        );
    }
    assert!(titled > 1, "some fixtures hold titles before the rename");

    for (case, _copy, session) in sessions_with_generated_titles() {
        let (searched, parsed) = searched_and_parsed_titles(&session);
        assert!(searched.1.is_some(), "{case}: search reads a summary");
        assert_eq!(
            searched, parsed,
            "{case}: search reads the summary the parse names"
        );
    }
}

/// The title and the summary search reads in the row of `session`'s copy
/// once a load writes it, and those its parse names.
fn searched_and_parsed_titles(session: &AgentSession) -> (TitleAndSummary, TitleAndSummary) {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let row = &session.row;
    let storage = storage_of(row.source);
    cache.admit_storage(storage).unwrap();
    let root = root_of(session);
    let stub = stub_of(session, 1);
    let (record, parsed) = parsed_record_and_row(row.source, &root, &stub);
    cache.write(&record).unwrap();
    let CatalogueSession::Listed(listed) = cache
        .catalogue_session(storage, row.source, &root, &stub.cache_key, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
    };
    let trimmed = |text: Option<String>| text.map(|text| text.trim().to_owned());
    let mut leading = listed.leading_texts().map(|text| text.trim().to_owned());
    let parsed = (trimmed(parsed.custom_title), trimmed(parsed.summary));
    let searched = match parsed {
        (Some(_), Some(_)) => (leading.next(), leading.next()),
        (Some(_), None) => (leading.next(), None),
        (None, _) => (None, leading.next()),
    };
    assert_eq!(
        leading.next(),
        None,
        "search reads two leading texts at most"
    );
    (searched, parsed)
}

/// `session` renamed to `title` through its agent's renamer, on a copy when
/// its transcripts are the repository's fixtures; with the copy's directory.
fn renamed_copy(session: AgentSession, title: &str) -> (Option<tempfile::TempDir>, AgentSession) {
    let (copy, session) = writable(session);
    rename(session.row.source, &session.row.path, title);
    (copy, session)
}

/// `source`'s session at `path` renamed to `title` through its agent's
/// renamer.
pub(super) fn rename(source: Source, path: &Path, title: &str) {
    let Capability::Capable(renamer) = source.provider().rename() else {
        panic!("{}: every agent renames a session", source.label());
    };
    let renamed_at = chrono::DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    renamer
        .rename_session(crate::history::FOR_FIXTURES, path, title, renamed_at)
        .unwrap();
}

/// A copy of `session` in an agent home, where its agent's discovery finds
/// it and the agent's own files beside it, such as a title index, can be
/// written; with the home.
pub(super) fn writable(session: AgentSession) -> (Option<tempfile::TempDir>, AgentSession) {
    let source = session.row.source;
    let fixture = registered_fixtures()
        .into_iter()
        .find(|fixture| fixture.provider().source() == source)
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    fixture.write_every_record_session(home.path(), &session);
    let [stub] = <[_; 1]>::try_from(fixture.discover_under(home.path()))
        .unwrap_or_else(|stubs| panic!("{}: one session in the home: {stubs:?}", source.label()));
    let copy = AgentSession::new(source, stub.locator, stub.subagents, Vec::new());
    (Some(home), copy)
}

/// Sessions holding a summary an agent generated, each named by its case,
/// with the directory of a copy the session reads: Claude's summary and AI
/// title records, and a title Kimi generated.
fn sessions_with_generated_titles() -> Vec<(&'static str, Option<tempfile::TempDir>, AgentSession)>
{
    let first_summary =
        "{\"type\":\"summary\",\"summary\":\" First summary \",\"leafUuid\":\"l\"}\n";
    let second_summary =
        "{\"type\":\"summary\",\"summary\":\"Second summary\",\"leafUuid\":\"l\"}\n";
    let ai_title = "{\"type\":\"ai-title\",\"aiTitle\":\" Generated by the model \"}\n";
    let (summaries_copy, summaries) =
        session_with_records(Source::Claude, &[first_summary, second_summary]);
    let (ai_title_copy, ai_titled) =
        session_with_records(Source::Claude, &[first_summary, ai_title]);

    let (kimi_copy, kimi) = writable(every_record_session_of(Source::Kimi));
    let state = crate::history::format::kimi::wire_location(&kimi.row.path)
        .session_dir
        .join("state.json");
    let mut value: serde_json::Value = std::fs::read_to_string(&state).map_or_else(
        |_| serde_json::json!({}),
        |text| serde_json::from_str(&text).unwrap(),
    );
    value["title"] = " Kimi generated title ".into();
    value["titleKind"] = "generated".into();
    std::fs::write(&state, value.to_string()).unwrap();
    vec![
        ("claude summaries", summaries_copy, summaries),
        ("claude AI title after a summary", ai_title_copy, ai_titled),
        ("kimi generated title", kimi_copy, kimi),
    ]
}

fn every_record_session_of(source: Source) -> AgentSession {
    every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == source)
        .unwrap()
}

/// `source`'s every-record session, writable, with `records` appended to its
/// transcript.
fn session_with_records(
    source: Source,
    records: &[&str],
) -> (Option<tempfile::TempDir>, AgentSession) {
    let (copy, session) = writable(every_record_session_of(source));
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(&session.row.path)
        .unwrap();
    for record in records {
        std::io::Write::write_all(&mut transcript, record.as_bytes()).unwrap();
    }
    (copy, session)
}

/// The session's title and summary are not among these texts: search reads
/// them from the session row as the agent wrote them, and agents don't write
/// a terminal code into one.
#[test]
fn code_inside_word_doesnt_split_it_in_any_field_and_display_reads_it_back() {
    let red = "re\u{1b}[31md";
    let records = || {
        message_schema::transcript(vec![
            serde_json::json!({"type": "user", "message": {"role": "user", "content": red}}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": red, "signature": ""},
                {"type": "text", "text": red},
                {"type": "tool_use", "id": "call", "name": "Bash", "input": {"command": red}},
            ]}}),
            serde_json::json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call", "content": red},
                {"type": "tool_result", "tool_use_id": "call", "content": [{"type": "text", "text": red}]},
            ]}}),
        ])
    };
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let (storage, root) = write_test_entries(&mut cache, records());

    let read = cache.entries(storage, &root, CACHE_KEY).unwrap().unwrap();
    assert_eq!(
        format!("{:?}", read.entries),
        format!("{:?}", records().entries),
        "the display read gives back each text with its terminal codes"
    );
    let mut fields_holding_red = cache
        .connection
        .prepare("SELECT search_field, text FROM parts WHERE search_field IS NOT NULL")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .filter(|(_, text)| {
            text.split(|ch: char| !ch.is_alphanumeric())
                .any(|word| word == "red")
        })
        .map(|(field, _)| field)
        .collect::<Vec<_>>();
    fields_holding_red.sort_unstable();
    fields_holding_red.dedup();
    assert_eq!(
        fields_holding_red,
        [SearchField::Full, SearchField::Thinking].map(SearchField::code),
        "each field reads the word whole"
    );
}

fn sorted(mut texts: Vec<PlacedText>) -> Vec<PlacedText> {
    texts.sort();
    texts
}
