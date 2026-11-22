use super::{every_record_rows, every_record_sessions};
use crate::agent::sessions::OwnMessageCounts;
use crate::agent::transcript::AgentTranscript;
use crate::clock::Zone;
use crate::history::Conversation;
use crate::time_filter::TimeFilter;
use chrono::TimeZone;

/// The window from `after` to `before`, in whole seconds, either unbounded.
fn window(after: Option<i64>, before: Option<i64>) -> TimeFilter {
    let local = |second| Zone::System.timestamp_opt(second, 0).single().unwrap();
    TimeFilter {
        after: after.map(local),
        before: before.map(local),
    }
}

/// Windows whose bounds fall on each message's first and last seconds, and
/// one second past its last, besides the window without bounds.
fn windows_around_each_message(row: &Conversation) -> Vec<TimeFilter> {
    let mut windows = vec![TimeFilter::default()];
    for span in (1..=row.message_count).filter_map(|ordinal| row.message_times.span(ordinal)) {
        let (first, last) = (span.first_second(), span.last_second());
        windows.extend([
            window(Some(last), None),
            window(Some(last + 1), None),
            window(None, Some(first)),
            window(None, Some(first - 1)),
            window(Some(first), Some(last)),
        ]);
    }
    windows
}

#[test]
fn every_agents_cached_own_message_counts_match_its_transcripts_own_messages_in_each_window() {
    let base = tempfile::tempdir().unwrap();
    for (session, row) in every_record_sessions()
        .iter()
        .zip(every_record_rows(base.path()))
    {
        let fixture = &session.row;
        let transcript = AgentTranscript::load_cached_for_test(
            fixture.source,
            &fixture.path,
            &fixture.subagents,
        )
        .unwrap();
        let own = transcript
            .messages
            .iter()
            .filter(|message| message.subagent.is_none())
            .map(|message| message.ordinal)
            .collect::<Vec<_>>();
        let copy = row.cached_session.as_ref().unwrap();
        for window in windows_around_each_message(&row) {
            assert_eq!(
                copy.read_copy(|held| held.own_message_counts(&window))
                    .unwrap(),
                OwnMessageCounts::of_ordinals(own.iter().copied(), &row.message_times, &window),
                "{:?}: own messages counted in the cache for {window:?}",
                row.source
            );
        }
    }
}
