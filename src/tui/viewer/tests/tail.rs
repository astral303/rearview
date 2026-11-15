//! A render of a conversation's last entries after the shown rows of a
//! render of its earlier entries, against a whole render with the same
//! options.

use super::rerender::{
    Modes, Shown, first_expanded_sets, interleaved_calls_across_a_day,
    run_with_results_in_later_entries, runs_around_midnight,
    runs_followed_by_entries_rendering_nothing, tool_runs_between_messages,
};
use super::*;
use crate::history::format::RecordLine;
use std::collections::HashSet;

/// The entries of `source`'s session at `file_path`, with the sub-agent
/// transcripts `subagents`, as the viewer's read takes them from the cache
/// once a load cached the session.
fn stored_entries(
    source: crate::history::Source,
    file_path: &Path,
    subagents: &[PathBuf],
) -> Vec<(RecordLine, LogEntry)> {
    use crate::history::environment::test_support::{cache_session, test_cache_base};

    let row = cache_session(source, file_path, subagents);
    crate::history::sqlite_cache::read_copy_waiting(
        &test_cache_base(),
        row.cache_address
            .as_ref()
            .expect("the load lists the row at its address"),
        |held| held.entries(),
    )
    .unwrap_or_else(|_| panic!("the cache holds the copy the load wrote"))
    .entries
}

/// The conversation of the first `count` of `entries`, as the viewer reads
/// a copy holding them alone.
fn conversation_of_first<E: Clone>(entries: &[E], count: usize) -> ParsedConversation
where
    DisplayEntries: From<Vec<E>>,
{
    parsed_conversation(entries[..count].to_vec().into())
}

/// `conversation`'s entries, each at its own index.
fn numbered_entries(conversation: &ParsedConversation) -> Vec<(usize, LogEntry)> {
    conversation
        .entries
        .iter()
        .map(|parsed| (parsed.entry_index, parsed.entry.clone()))
        .collect()
}

/// The ids of the calls and of the calls answered in `entry`.
fn tool_ids(entry: &LogEntry) -> impl Iterator<Item = (&str, bool)> {
    let blocks: &[crate::log_entry::ContentBlock] = match entry {
        LogEntry::Assistant { message, .. } => &message.content,
        LogEntry::User {
            message:
                crate::log_entry::UserMessage {
                    content: crate::log_entry::UserContent::Blocks(blocks),
                    ..
                },
            ..
        } => blocks,
        _ => &[],
    };
    blocks.iter().filter_map(|block| match block {
        crate::log_entry::ContentBlock::ToolUse { id, .. } => Some((id.as_str(), false)),
        crate::log_entry::ContentBlock::ToolResult { tool_use_id, .. } => {
            Some((tool_use_id.as_str(), true))
        }
        _ => None,
    })
}

/// The position of the first entry before `first` holding a call that a
/// result at or after `first` answers, in either conversation; both hold
/// the same entries before `first`.
fn earliest_call_answered_from(
    conversations: [&ParsedConversation; 2],
    first: usize,
) -> Option<usize> {
    let answered: HashSet<&str> = conversations
        .iter()
        .flat_map(|conversation| &conversation.entries[first.min(conversation.entries.len())..])
        .flat_map(|parsed| tool_ids(&parsed.entry))
        .filter_map(|(id, is_result)| is_result.then_some(id))
        .collect();
    conversations[1].entries[..first].iter().position(|parsed| {
        tool_ids(&parsed.entry).any(|(id, is_result)| !is_result && answered.contains(id))
    })
}

/// The first entry a tail renders and the row of `shown` its rows replace,
/// once `conversation` changed `shown_conversation`, drawn as `shown` with
/// `options`, from the entry numbered `first_changed` on; `None` for a
/// whole render. The tail starts at the first entry of the last message
/// shown before the change, moved back while a call before it is answered
/// at or after it. A whole render runs when the rosters or the session's
/// agents differ.
fn simplest_start(
    shown_conversation: &ParsedConversation,
    conversation: &ParsedConversation,
    shown: &Shown,
    first_changed: usize,
    options: &RenderOptions,
) -> Option<(usize, usize)> {
    if shown_conversation.roster != conversation.roster
        || session_agent(&shown_conversation.entries) != session_agent(&conversation.entries)
    {
        return None;
    }
    let mut before = first_changed;
    loop {
        let message = shown
            .messages
            .iter()
            .rposition(|message| message.entry_index < before)?;
        let entry_index = shown.messages[message].entry_index;
        let first = conversation
            .entries
            .partition_point(|parsed| parsed.entry_index < entry_index);
        match earliest_call_answered_from([shown_conversation, conversation], first) {
            Some(call) => before = conversation.entries[call].entry_index + 1,
            None => {
                let mut day_labels =
                    days::DayLabels::new(options.show_timing, options.today, options.zone);
                day_labels.resume_after(
                    conversation.entries[..first]
                        .iter()
                        .map(|parsed| &parsed.entry),
                );
                let label_rows = usize::from(
                    day_labels
                        .new_day_at(&conversation.entries[first].entry)
                        .is_some(),
                );
                return Some((entry_index, shown.messages[message].start_line - label_rows));
            }
        }
    }
}

/// `shown`'s rows before `row`, then `tail`'s rows, ranges and calls moved
/// down to follow them.
fn spliced(mut shown: Shown, tail: RenderedConversation, row: usize) -> Shown {
    shown.lines.truncate(row);
    shown.lines.extend(tail.lines);
    let kept = shown
        .messages
        .partition_point(|message| message.start_line < row);
    shown.messages.truncate(kept);
    shown
        .messages
        .extend(tail.messages.into_iter().map(|message| MessageRange {
            start_line: message.start_line + row,
            end_line: message.end_line + row,
            ..message
        }));
    let kept = shown
        .calls
        .partition_point(|call| call.input.start_line < row);
    shown.calls.truncate(kept);
    shown.calls.extend(tail.calls.into_iter().map(|mut call| {
        for area in std::iter::once(&mut call.input).chain(call.result.as_mut()) {
            area.start_line += row;
            area.end_line += row;
        }
        call
    }));
    let kept = shown
        .detail_calls
        .partition_point(|call| call.rows.input.start < row);
    shown.detail_calls.truncate(kept);
    shown
        .detail_calls
        .extend(tail.detail_calls.into_iter().map(|mut call| {
            call.rows.input = call.rows.input.start + row..call.rows.input.end + row;
            if let Some(result) = call.rows.result.as_mut() {
                *result = result.start + row..result.end + row;
            }
            call
        }));
    shown
}

/// For each split of `entries` and in every mode, nothing expanded and in
/// summary mode every run expanded: the rows of the first `split` entries,
/// then the tail [`simplest_start`] gives, render as a whole render of every
/// entry. Returns how many splits rendered a tail.
fn assert_tails_render_as_whole_render<E: Clone>(name: &str, entries: &[E]) -> usize
where
    DisplayEntries: From<Vec<E>>,
{
    let conversation = conversation_of_first(entries, entries.len());
    let mut tails = 0;
    for modes in Modes::every() {
        for expanded in first_expanded_sets(&conversation, modes) {
            let options = modes.options(expanded);
            let whole = Shown::of(render_parsed_conversation(&conversation, &options));
            for split in 0..entries.len() {
                let shown_conversation = conversation_of_first(entries, split);
                let shown = Shown::of(render_parsed_conversation(&shown_conversation, &options));
                let Some((entry_index, row)) =
                    simplest_start(&shown_conversation, &conversation, &shown, split, &options)
                else {
                    continue;
                };
                let tail = render_tail_while(&conversation, &options, entry_index, &|| true)
                    .expect("a render that keeps going finishes");
                assert_eq!(
                    spliced(shown, tail, row).shape(),
                    whole.shape(),
                    "{name} {modes:?}, split at entry {split}: the tail from entry {entry_index} \
                     after the shown rows renders as a whole render"
                );
                tails += 1;
            }
        }
    }
    tails
}

#[test]
fn every_agents_tail_renders_after_its_earlier_rows_as_whole_render() {
    use crate::history::provider::contract_tests::every_record_sessions;
    for session in every_record_sessions() {
        let row = &session.row;
        let name = format!("{:?}", row.source);
        let entries = stored_entries(row.source, &row.path, &row.subagents);
        let tails = assert_tails_render_as_whole_render(&name, &entries);
        assert!(tails > 0, "{name}: a split renders a tail");
    }
}

#[test]
fn tails_of_calls_runs_and_day_labels_render_after_earlier_rows_as_whole_render() {
    let mut fixtures = vec![
        ("interleaved", interleaved_calls_across_a_day()),
        ("runs", tool_runs_between_messages()),
        ("late results", run_with_results_in_later_entries()),
        (
            "entries rendering nothing",
            runs_followed_by_entries_rendering_nothing(),
        ),
    ];
    fixtures.extend(
        runs_around_midnight()
            .into_iter()
            .map(|(name, conversation, _)| (name, conversation)),
    );
    for (name, conversation) in fixtures {
        let tails = assert_tails_render_as_whole_render(name, &numbered_entries(&conversation));
        assert!(tails > 0, "{name}: a split renders a tail");
    }
}
