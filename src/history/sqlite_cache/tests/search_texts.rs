//! The search texts a cache write stores, against the texts the session's
//! content rebuilds from its stored entries.

use super::*;
use crate::history::Conversation;
use crate::history::parser::normalize_whitespace;
use crate::time_filter::TimeFilter;
use std::ops::ControlFlow;

/// A search text's field, message number and normalised text.
type FieldText = (SearchField, Option<usize>, String);

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
            .write(
                &SessionRecord::listed(
                    crate::history::FOR_FIXTURES,
                    storage,
                    row.source,
                    &root,
                    &stub,
                    row,
                )
                .unwrap(),
            )
            .unwrap();
        let CatalogueSession::Listed(listed) = cache
            .catalogue_session(storage, row.source, &root.path, &stub.cache_key, false)
            .unwrap()
            .unwrap()
        else {
            panic!("listed fixture")
        };
        let copy = listed.cached_session.as_ref().unwrap();

        let rebuilt = sorted(rebuilt_texts(copy, &listed));
        let stored = sorted(stored_texts(copy));

        assert!(
            !rebuilt.is_empty(),
            "{}: the content rebuilds search texts",
            row.source.label()
        );
        assert_eq!(
            stored,
            rebuilt,
            "{}: each text the content rebuilds is stored once, with its field and message number",
            row.source.label()
        );
    }
}

/// The texts the content of `listed`, whose copy is `copy`, rebuilds from
/// its entries, as `Conversation::load_content` reads them; semantic text
/// is not stored.
fn rebuilt_texts(copy: &CachedSession, listed: &Conversation) -> Vec<FieldText> {
    let mut texts = Vec::new();
    copy.search_parts(listed, |part| {
        if part.field != SearchField::Semantic {
            texts.push((part.field, part.ordinal, part.text));
        }
    })
    .unwrap()
    .expect("the cache holds the copy");
    texts
}

/// The texts `copy` stores in `agent search`'s fields, normalised as search
/// reads them: whitespace collapsed. Text left empty is dropped, as the
/// content drops it.
fn stored_texts(copy: &CachedSession) -> Vec<FieldText> {
    let mut texts = Vec::new();
    copy.visit_search_texts(SearchedFields::Agent, &TimeFilter::default(), |text| {
        let normalised = normalize_whitespace(text.text);
        if !normalised.is_empty() {
            texts.push((text.field, text.ordinal, normalised));
        }
        ControlFlow::Continue(())
    })
    .unwrap()
    .expect("the cache holds the copy");
    texts
}

#[test]
fn code_inside_word_doesnt_split_it_in_any_field_and_display_reads_it_back() {
    let red = "re\u{1b}[31md";
    let records = || {
        message_schema::transcript(vec![
            serde_json::json!({"type": "custom-title", "customTitle": red}),
            serde_json::json!({"type": "summary", "summary": red}),
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

    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(
        format!("{:?}", read.entries),
        format!("{:?}", records().entries),
        "the display read gives back each text with its terminal codes"
    );
    let mut fields_holding_red = cache
        .connection
        .prepare(
            "SELECT search_field, text FROM parts WHERE search_field IS NOT NULL \
             AND text IS NOT NULL UNION ALL SELECT p.search_field, i.text FROM result_items i \
             JOIN parts p ON p.id = i.part WHERE i.searched UNION ALL \
             SELECT search_field, text FROM search_texts UNION ALL \
             SELECT search_field, text FROM entries WHERE search_field IS NOT NULL",
        )
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
        [
            SearchField::Full,
            SearchField::Thinking,
            SearchField::Leading
        ]
        .map(SearchField::code),
        "each field reads the word whole"
    );
}

fn sorted(mut texts: Vec<FieldText>) -> Vec<FieldText> {
    texts.sort_by(
        |(field, ordinal, text), (other_field, other_ordinal, other_text)| {
            (field.code(), ordinal, text).cmp(&(other_field.code(), other_ordinal, other_text))
        },
    );
    texts
}
