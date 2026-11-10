//! `agent sessions`: sessions newest first, or those with a message inside a
//! time window. Each record's values come from the session cache.

use crate::agent::diagnostic::{AgentWarning, format_warning_records, warning_atoms};
use crate::agent::protocol::{
    budget_atom, cut_short, escape_atom, protocol_snippet, title_for_conversation,
};
use crate::agent::refs::ResolvedConversation;
use crate::agent::time_span::{output_offset, time_atoms, tz_atom};
use crate::clock::Zone;
use crate::history::listing::{self, SessionOrigin, SessionTimes};
use crate::history::{Conversation, MessagesInWindow};
use crate::time_filter::TimeFilter;
use chrono::{DateTime, FixedOffset};
use std::cmp::Reverse;

const AGENT_SESSIONS_TITLE_CHARS: usize = 120;

#[derive(Clone, Debug, PartialEq)]
pub struct ListedSession {
    pub project_id: String,
    pub conversation_uuid: String,
    pub reference: String,
    pub agent: &'static str,
    pub message_count: usize,
    pub in_window: MessagesInWindow,
    pub title: String,
}

/// One page of `agent sessions`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionListing {
    pub sessions: Vec<ListedSession>,
    /// How many sessions have a message inside the window, across every page.
    pub matched: usize,
    pub offset: usize,
    pub window: TimeFilter,
}

/// The page of `candidates` from `offset`, at most `limit`: each session with a
/// message inside `window`, newest first by its last message inside it. For a
/// window without bounds, every session counts.
pub fn list_sessions<'a>(
    candidates: impl IntoIterator<Item = (&'a Conversation, &'a ResolvedConversation)>,
    window: &TimeFilter,
    offset: usize,
    limit: usize,
) -> SessionListing {
    let mut matching = candidates
        .into_iter()
        .filter(|(conversation, _)| {
            listing::is_listed(
                SessionOrigin::Discovered,
                SessionTimes::Messages(&conversation.message_times),
                window,
            )
        })
        .map(|(conversation, resolved)| {
            (
                conversation,
                resolved,
                conversation.message_times.in_window(window),
            )
        })
        .collect::<Vec<_>>();
    matching.sort_by_key(|(conversation, _, in_window)| {
        (
            Reverse(in_window.times.map_or_else(
                || conversation.timestamp.timestamp(),
                |times| times.last_second(),
            )),
            conversation.path.clone(),
        )
    });
    SessionListing {
        matched: matching.len(),
        sessions: matching
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(conversation, resolved, in_window)| ListedSession {
                project_id: resolved.key.project_id(),
                conversation_uuid: resolved.reference.uuid(),
                reference: resolved.reference.canonical(),
                agent: resolved.key.source.provider().labels().name,
                message_count: conversation.message_count,
                in_window,
                title: title_for_conversation(conversation),
            })
            .collect(),
        offset,
        window: *window,
    }
}

#[cfg(test)]
pub fn format_sessions(listing: &SessionListing, budget: Option<usize>) -> String {
    format_sessions_with_warnings(
        listing,
        budget,
        &[],
        crate::agent::test_support::written_at(),
    )
}

/// The `agent-sessions` records: as many `session` records as fit in
/// `budget` beside every warning record, a `continue sessions` record naming
/// the offset of the next page, then the warning records, whole. Their times
/// are in `written_at`'s zone, and `written_at` sets the `tz=` offset of a
/// listing that does not show a time.
pub fn format_sessions_with_warnings(
    listing: &SessionListing,
    budget: Option<usize>,
    warnings: &[AgentWarning],
    written_at: DateTime<Zone>,
) -> String {
    let (warning_count, warning_records) = format_warning_records(warnings);
    let warning_suffix = warning_atoms(warning_count, &warning_records);
    let offset = output_offset(
        listing
            .sessions
            .iter()
            .find_map(|session| session.in_window.times),
        written_at,
    );
    let records = listing
        .sessions
        .iter()
        .map(|session| session_record(session, &listing.window, offset, written_at.timezone()))
        .collect::<Vec<_>>();
    let render = |listed: usize| {
        let cut = if listed == records.len() {
            "none"
        } else {
            "tail"
        };
        let mut output = format!(
            "protocol agent-sessions cut={cut} chars={}{}{warning_suffix}\n",
            budget_atom(budget),
            tz_atom(offset)
        );
        output.push_str(&format!(
            "sessions matched={} offset={} listed={listed}\n",
            listing.matched, listing.offset
        ));
        for record in &records[..listed] {
            output.push_str(record);
        }
        let next_offset = listing.offset + listed;
        if next_offset < listing.matched {
            output.push_str(&format!(
                "continue sessions offset={next_offset} action=rerun-with-offset\n"
            ));
        }
        for record in &warning_records {
            output.push_str(record);
        }
        output
    };

    let mut listed = records.len();
    let mut output = render(listed);
    if let Some(budget) = budget {
        while output.chars().count() > budget && listed > 0 {
            listed -= 1;
            output = render(listed);
        }
        if output.chars().count() > budget {
            return cut_short(&output, budget);
        }
    }
    output
}

/// The `session` record of `session`, its times in `zone`, below a header
/// whose `tz=` names `header_offset`.
fn session_record(
    session: &ListedSession,
    window: &TimeFilter,
    header_offset: FixedOffset,
    zone: Zone,
) -> String {
    let in_window = if window.is_active() {
        format!(" messages-in-window={}", session.in_window.count)
    } else {
        String::new()
    };
    format!(
        "session project={} uuid={} ref={} agent={} messages={}{in_window}{} : {}\n",
        escape_atom(&session.project_id),
        escape_atom(&session.conversation_uuid),
        escape_atom(&session.reference),
        session.agent,
        session.message_count,
        time_atoms(session.in_window.times, header_offset, zone),
        protocol_snippet(&session.title, AGENT_SESSIONS_TITLE_CHARS)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{resolved_conversation, written_at};
    use crate::agent::time_span::TimeSpan;
    use crate::history::MessageTimes;
    use crate::semantic::test_fixtures::SemanticConversationFixture;
    use chrono::{Local, Offset, TimeZone};

    /// An hour and a minute on 2026-10-14, local time.
    type Minute = (u32, u32);

    fn epoch_second((hour, minute): Minute) -> i64 {
        Local
            .with_ymd_and_hms(2026, 10, 14, hour, minute, 0)
            .unwrap()
            .timestamp()
    }

    /// A session titled `title`, one message for each of `spans`: its first
    /// and last time.
    fn session(title: &str, spans: &[(Minute, Minute)]) -> Conversation {
        let mut conversation =
            SemanticConversationFixture::new(format!("/sessions/{title}.jsonl"), [""; 0])
                .with_title(title)
                .build();
        conversation.message_times = MessageTimes::new(
            &spans
                .iter()
                .map(|&(first, last)| {
                    Some(TimeSpan::from_seconds(
                        epoch_second(first),
                        epoch_second(last),
                    ))
                })
                .collect::<Vec<_>>(),
        );
        conversation.message_count = spans.len();
        conversation
    }

    /// The window from `first` to `last`, both included.
    fn window(first: Minute, last: Minute) -> TimeFilter {
        let local = |minute| Zone::System.timestamp_opt(epoch_second(minute), 0).unwrap();
        TimeFilter {
            after: Some(local(first)),
            before: Some(local(last)),
        }
    }

    fn listing(
        sessions: &[Conversation],
        window: &TimeFilter,
        offset: usize,
        limit: usize,
    ) -> SessionListing {
        let resolved = sessions
            .iter()
            .map(resolved_conversation)
            .collect::<Vec<_>>();
        list_sessions(sessions.iter().zip(&resolved), window, offset, limit)
    }

    fn titles(listing: &SessionListing) -> Vec<&str> {
        listing
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect()
    }

    /// Five sessions around the window from 11:38 to 11:45: `early` (11:00,
    /// 11:39), `spanning` (one message from 11:30 to 11:50), `idle` (11:00,
    /// 11:50), `late` (11:44, 12:30) and `after` (11:46).
    fn sessions() -> Vec<Conversation> {
        vec![
            session("early", &[((11, 0), (11, 0)), ((11, 39), (11, 39))]),
            session("spanning", &[((11, 30), (11, 50))]),
            session("idle", &[((11, 0), (11, 0)), ((11, 50), (11, 50))]),
            session("late", &[((11, 44), (11, 44)), ((12, 30), (12, 30))]),
            session("after", &[((11, 46), (11, 46))]),
        ]
    }

    /// ` tz=` the offset of the times of `listing`'s first session.
    fn tz_of(listing: &SessionListing) -> String {
        tz_atom(output_offset(
            listing.sessions[0].in_window.times,
            written_at(),
        ))
    }

    /// The records of `listing` beside `warnings`, written at the test
    /// clock's time.
    fn records_with_warnings(
        listing: &SessionListing,
        budget: Option<usize>,
        warnings: &[AgentWarning],
    ) -> String {
        format_sessions_with_warnings(listing, budget, warnings, written_at())
    }

    /// January and July: in a zone with daylight saving time, one of them is
    /// in it.
    #[test]
    fn empty_listing_names_local_offset_at_time_it_is_written() {
        let listing = listing(&[], &TimeFilter::default(), 0, 10);

        for utc in ["2026-01-14T15:00:00Z", "2026-07-14T15:00:00Z"] {
            let written_at = DateTime::parse_from_rfc3339(utc)
                .unwrap()
                .with_timezone(&Zone::System);
            let local_offset = written_at.with_timezone(&Local).offset().fix();

            let output = format_sessions_with_warnings(&listing, None, &[], written_at);

            assert!(
                output.starts_with(&format!(
                    "protocol agent-sessions cut=none chars=none tz={local_offset}\n"
                )),
                "{output}"
            );
        }
    }

    /// The smallest budget that holds a `cut=tail` header naming it and the
    /// offset `tz`, then `body` characters.
    fn budget_with_header(body: usize, tz: &str) -> usize {
        (body..)
            .find(|&budget| {
                let header = format!("protocol agent-sessions cut=tail chars={budget}{tz}\n");
                body + header.chars().count() <= budget
            })
            .unwrap()
    }

    #[test]
    fn session_is_listed_when_message_falls_inside_window() {
        let listing = listing(&sessions(), &window((11, 38), (11, 45)), 0, 10);

        assert_eq!(titles(&listing), ["spanning", "late", "early"]);
        assert_eq!(listing.matched, 3);
    }

    #[test]
    fn listed_session_has_count_and_span_of_its_messages_inside_window() {
        let listing = listing(&sessions(), &window((11, 38), (11, 45)), 0, 10);
        let late = &listing.sessions[1];

        assert_eq!(late.message_count, 2);
        assert_eq!(
            late.in_window,
            MessagesInWindow {
                count: 1,
                times: Some(TimeSpan::from_seconds(
                    epoch_second((11, 44)),
                    epoch_second((11, 44))
                )),
            }
        );
    }

    #[test]
    fn without_window_every_session_is_listed_newest_first_by_its_last_message() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);

        assert_eq!(
            titles(&listing),
            ["late", "idle", "spanning", "after", "early"]
        );
        assert_eq!(listing.sessions[0].in_window.count, 2);
    }

    #[test]
    fn offset_and_limit_page_through_sessions_in_order() {
        let sessions = sessions();
        let first = listing(&sessions, &TimeFilter::default(), 0, 2);
        let second = listing(&sessions, &TimeFilter::default(), 2, 2);

        assert_eq!(titles(&first), ["late", "idle"]);
        assert_eq!(titles(&second), ["spanning", "after"]);
        assert_eq!(second.matched, 5);
    }

    #[test]
    fn session_record_shows_its_agent_counts_times_and_title() {
        let listing = listing(&sessions(), &window((11, 38), (11, 45)), 0, 1);

        let output = format_sessions(&listing, None);

        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            format!(
                "protocol agent-sessions cut=none chars=none{}",
                tz_of(&listing)
            )
        );
        assert_eq!(lines[1], "sessions matched=3 offset=0 listed=1");
        let record = lines[2];
        assert!(record.starts_with("session project=pr_"), "{record}");
        assert!(record.contains(" ref=ch_"), "{record}");
        assert!(
            record.contains(" agent=claude messages=1 messages-in-window=1 at="),
            "{record}"
        );
        assert!(record.contains(" to="), "{record}");
        assert!(record.ends_with(" : spanning"), "{record}");
        assert_eq!(
            lines[3],
            "continue sessions offset=1 action=rerun-with-offset"
        );
    }

    #[test]
    fn listing_without_window_doesnt_show_messages_in_window_count() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);

        let output = format_sessions(&listing, None);

        assert!(!output.contains("messages-in-window="), "{output}");
        assert!(!output.contains("continue sessions"), "{output}");
    }

    #[test]
    fn budgeted_listing_continues_from_first_session_it_omits() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);
        let unbudgeted = format_sessions(&listing, None);
        let sessions_line_and_two_records = unbudgeted
            .lines()
            .skip(1)
            .take(3)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>();
        let continue_record = "continue sessions offset=2 action=rerun-with-offset\n";
        let tz = tz_of(&listing);
        let budget = budget_with_header(sessions_line_and_two_records + continue_record.len(), &tz);

        let output = format_sessions(&listing, Some(budget));

        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with(&format!(
                "protocol agent-sessions cut=tail chars={budget}{tz}\n"
            )),
            "{output}"
        );
        assert!(output.contains("listed=2\n"), "{output}");
        assert!(output.ends_with(continue_record), "{output}");
    }

    #[test]
    fn budget_short_of_every_record_omits_session_and_keeps_warning_whole() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);
        let warnings = [AgentWarning::ignored("an ignored term".to_string())];
        let unbudgeted = records_with_warnings(&listing, None, &warnings);
        let warning_record = format!("{}\n", unbudgeted.lines().last().unwrap());
        let roomy = records_with_warnings(&listing, Some(100_000), &warnings);
        let budget = roomy.chars().count() - 30;

        let output = records_with_warnings(&listing, Some(budget), &warnings);

        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with("protocol agent-sessions cut=tail "),
            "{output}"
        );
        assert!(
            output
                .lines()
                .next()
                .unwrap()
                .ends_with(" warnings=1 warning-records=1"),
            "{output}"
        );
        assert!(output.contains("listed=4\n"), "{output}");
        assert!(output.ends_with(&warning_record), "{output}");
    }

    #[test]
    fn budget_too_small_for_any_session_keeps_warning_whole() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);
        let warnings = [AgentWarning::ignored("an ignored term".to_string())];
        let unbudgeted = records_with_warnings(&listing, None, &warnings);
        let warning_record = format!("{}\n", unbudgeted.lines().last().unwrap());
        let body = [
            " warnings=1 warning-records=1",
            "sessions matched=5 offset=0 listed=0\n",
            "continue sessions offset=0 action=rerun-with-offset\n",
            &warning_record,
        ]
        .concat();
        let budget = budget_with_header(body.chars().count(), &tz_of(&listing));

        let output = records_with_warnings(&listing, Some(budget), &warnings);

        assert!(output.chars().count() <= budget, "{output}");
        assert!(output.contains("listed=0\n"), "{output}");
        assert!(output.ends_with(&warning_record), "{output}");
    }

    #[test]
    fn output_is_truncated_to_budget_shorter_than_header() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);

        let output = format_sessions(&listing, Some(20));

        assert_eq!(output.chars().count(), 20);
        assert!(!output.ends_with('\n'), "{output}");
    }

    #[test]
    fn session_is_identified_by_its_project_uuid_and_ref() {
        let sessions = sessions();
        let listing = listing(&sessions, &TimeFilter::default(), 0, 1);
        let expected = resolved_conversation(&sessions[3]);

        let late = &listing.sessions[0];

        assert_eq!(late.reference, expected.reference.canonical());
        assert_eq!(late.conversation_uuid, expected.reference.uuid());
        assert_eq!(late.project_id, expected.key.project_id());
        assert_eq!(late.agent, "claude");
    }
}
