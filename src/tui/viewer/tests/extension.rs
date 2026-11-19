//! A conversation extended with a newer copy's tail, against the
//! conversation of a whole read of that copy.

use super::rerender::{long_call, long_result, text};
use super::tail::every_agents_stored_entries;
use super::*;

/// The conversation of `shown`, a copy's entries, extended with the tail of
/// `changed`, a newer copy's entries, from the entry numbered
/// `first_changed` on: it equals the conversation of `changed` in every
/// field, and the extension returns the shown copy's replaced entries,
/// their origins and its links.
fn assert_extension_equals_whole_read<E: Clone>(
    case: &str,
    shown: &[E],
    changed: &[E],
    first_changed: usize,
) where
    DisplayEntries: From<Vec<E>>,
{
    let shown_copy = parsed_conversation(shown.to_vec().into());
    let mut conversation = parsed_conversation(shown.to_vec().into());
    let tail = ConversationTail::of(first_changed, changed[first_changed..].to_vec().into());

    let replaced = conversation.extend_with(tail);

    let whole = parsed_conversation(changed.to_vec().into());
    assert_eq!(
        fields_differing(&conversation, &whole),
        Vec::<&str>::new(),
        "{case}, changed from entry {first_changed}: the extended conversation equals a whole \
         read's in every field"
    );
    let kept = shown_copy
        .entries
        .partition_point(|parsed| parsed.entry_index < first_changed);
    assert_eq!(
        format!("{:?}", replaced.entries),
        format!("{:?}", &shown_copy.entries[kept..]),
        "{case}, changed from entry {first_changed}: the extension returns the replaced entries"
    );
    assert!(
        replaced.origins == shown_copy.origins[first_changed..]
            && replaced.links_before(&conversation) == &shown_copy.links,
        "{case}, changed from entry {first_changed}: the extension returns the replaced \
         origins and the links before it"
    );
}

/// The extension returns the links it replaced, and doesn't return the
/// lanes or the activity span: the test reads the links alone.
#[test]
fn extension_without_changed_entries_doesnt_replace_links() {
    for (name, entries) in every_agents_stored_entries() {
        let mut conversation = parsed_conversation(entries.clone().into());
        let tail = ConversationTail::of(entries.len(), entries[entries.len()..].to_vec().into());

        let replaced = conversation.extend_with(tail);

        assert!(
            replaced.links.is_none(),
            "{name}: an extension without changed entries keeps the conversation's links"
        );
    }
}

/// Every cut of `entries`, as a session changes between two reads: entries
/// appended after the cut, the last two shown entries changed with them, the
/// transcript cut back, and one entry removed.
fn assert_every_cut_extends_to_whole_read<E: Clone>(name: &str, entries: &[E])
where
    DisplayEntries: From<Vec<E>>,
{
    for cut in 0..=entries.len() {
        assert_extension_equals_whole_read(
            &format!("{name}, appended after {cut} entries"),
            &entries[..cut],
            entries,
            cut,
        );
        assert_extension_equals_whole_read(
            &format!("{name}, appended after {cut} entries, the last two changed"),
            &entries[..cut],
            entries,
            cut.saturating_sub(2),
        );
        assert_extension_equals_whole_read(
            &format!("{name}, cut back to {cut} entries"),
            entries,
            &entries[..cut],
            cut,
        );
    }
    for removed in 0..entries.len() {
        let mut edited = entries.to_vec();
        edited.remove(removed);
        assert_extension_equals_whole_read(
            &format!("{name}, an entry removed"),
            entries,
            &edited,
            removed,
        );
    }
}

#[test]
fn every_agents_conversation_extended_at_every_cut_equals_whole_reads() {
    for (name, entries) in every_agents_stored_entries() {
        assert_every_cut_extends_to_whole_read(&name, &entries);
    }
}

/// Two calls open beside each other, the first answered last: a cut before
/// either result leaves a kept call that the tail answers.
#[test]
fn kept_call_answered_in_tail_gets_its_lane_as_in_whole_read() {
    let records = [
        text("user", None, "Run both."),
        long_call(None, "toolu_a"),
        long_call(None, "toolu_b"),
        long_result(None, "toolu_b"),
        long_result(None, "toolu_a"),
        text("user", None, "Thanks."),
    ];
    let entries: Vec<(usize, LogEntry)> = records
        .iter()
        .enumerate()
        .map(|(line, record)| (line, claude_entry(&record.to_string())))
        .collect();
    let whole = parsed_conversation(entries.clone().into());
    assert_eq!(
        (whole.lanes.get("toolu_a"), whole.lanes.get("toolu_b")),
        (Some(0), Some(1)),
        "the whole read gives each call a lane"
    );

    assert_every_cut_extends_to_whole_read("interleaved calls", &entries);
}

/// Message times out of order: the latest time is in an entry the tail
/// replaces, and the earliest in the tail.
#[test]
fn activity_span_after_replaced_entries_is_whole_reads() {
    let at = |time: &str, words: &str| {
        claude_entry(&text("user", Some(&format!("2026-09-26T{time}Z")), words).to_string())
    };
    let shown = vec![
        (1, at("12:00:00", "first")),
        (2, at("12:00:05", "second")),
        (3, at("18:00:00", "the latest, replaced")),
    ];
    let changed = vec![
        shown[0].clone(),
        shown[1].clone(),
        (3, at("09:00:00", "the earliest, in the tail")),
        (4, at("12:00:09", "fourth")),
    ];

    assert_extension_equals_whole_read("times out of order", &shown, &changed, 2);

    let whole = parsed_conversation(changed.into());
    assert_eq!(
        whole
            .activity_span()
            .map(|(earliest, latest)| (earliest.to_rfc3339(), latest.to_rfc3339())),
        Some((
            "2026-09-26T09:00:00+00:00".to_owned(),
            "2026-09-26T12:00:09+00:00".to_owned()
        )),
        "the whole read's span starts in the tail and doesn't include the replaced time"
    );
}
