//! `agent sessions`: sessions newest first, or those with a message inside a
//! time window. Each record's values come from the session cache.

use crate::agent::diagnostic::{AgentWarning, format_warning_records, warning_atoms};
use crate::agent::protocol::{budget_atom, escape_atom, protocol_snippet, title_for_conversation};
use crate::agent::refs::ResolvedConversation;
use crate::agent::time_span::time_atoms;
use crate::history::{Conversation, MessagesInWindow};
use crate::time_filter::TimeFilter;
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
        .filter(|(conversation, _)| conversation.message_times.has_message_within(window))
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
    format_sessions_with_warnings(listing, budget, &[])
}

/// The `agent-sessions` records: as many `session` records, then warning
/// records, as fit in `budget`, and a `continue sessions` record naming the
/// offset of the next page. A budget omits warning records before `session`
/// records; the header keeps the warning count.
pub fn format_sessions_with_warnings(
    listing: &SessionListing,
    budget: Option<usize>,
    warnings: &[AgentWarning],
) -> String {
    let (warning_count, warning_records) = format_warning_records(warnings);
    let warning_suffix = warning_atoms(warning_count, &warning_records);
    let records = listing
        .sessions
        .iter()
        .map(|session| session_record(session, &listing.window))
        .collect::<Vec<_>>();
    let render = |listed: usize, warnings_listed: usize| {
        let cut = if listed == records.len() && warnings_listed == warning_records.len() {
            "none"
        } else {
            "tail"
        };
        let mut output = format!(
            "protocol agent-sessions cut={cut} chars={}{warning_suffix}\n",
            budget_atom(budget)
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
        for record in &warning_records[..warnings_listed] {
            output.push_str(record);
        }
        output
    };

    let mut listed = records.len();
    let mut warnings_listed = warning_records.len();
    let mut output = render(listed, warnings_listed);
    if let Some(budget) = budget {
        while output.chars().count() > budget && (listed > 0 || warnings_listed > 0) {
            if warnings_listed > 0 {
                warnings_listed -= 1;
            } else {
                listed -= 1;
            }
            output = render(listed, warnings_listed);
        }
        if output.chars().count() > budget {
            return output.chars().take(budget).collect();
        }
    }
    output
}

fn session_record(session: &ListedSession, window: &TimeFilter) -> String {
    let in_window = if window.is_active() {
        format!(" messages-in-window={}", session.in_window.count)
    } else {
        String::new()
    };
    format!(
        "session project={} uuid={} ref={} agent={} messages={}{in_window}{} | {}\n",
        escape_atom(&session.project_id),
        escape_atom(&session.conversation_uuid),
        escape_atom(&session.reference),
        session.agent,
        session.message_count,
        time_atoms(session.in_window.times),
        protocol_snippet(&session.title, AGENT_SESSIONS_TITLE_CHARS)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::resolved_conversation;
    use crate::agent::time_span::TimeSpan;
    use crate::history::MessageTimes;
    use crate::semantic::test_fixtures::SemanticConversationFixture;
    use chrono::{Local, TimeZone};

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
        let local = |minute| Local.timestamp_opt(epoch_second(minute), 0).unwrap();
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

    /// The smallest budget that holds a `cut=tail` header naming it, then
    /// `body` characters.
    fn budget_with_header(body: usize) -> usize {
        (body..)
            .find(|&budget| {
                let header = format!("protocol agent-sessions cut=tail chars={budget}\n");
                body + header.chars().count() <= budget
            })
            .unwrap()
    }

    #[test]
    fn a_session_is_listed_when_a_message_falls_inside_the_window() {
        let listing = listing(&sessions(), &window((11, 38), (11, 45)), 0, 10);

        assert_eq!(titles(&listing), ["spanning", "late", "early"]);
        assert_eq!(listing.matched, 3);
    }

    #[test]
    fn a_listed_session_has_the_count_and_span_of_its_messages_inside_the_window() {
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
    fn without_a_window_every_session_is_listed_newest_first_by_its_last_message() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);

        assert_eq!(
            titles(&listing),
            ["late", "idle", "spanning", "after", "early"]
        );
        assert_eq!(listing.sessions[0].in_window.count, 2);
    }

    #[test]
    fn an_offset_and_a_limit_page_through_the_sessions_in_order() {
        let sessions = sessions();
        let first = listing(&sessions, &TimeFilter::default(), 0, 2);
        let second = listing(&sessions, &TimeFilter::default(), 2, 2);

        assert_eq!(titles(&first), ["late", "idle"]);
        assert_eq!(titles(&second), ["spanning", "after"]);
        assert_eq!(second.matched, 5);
    }

    #[test]
    fn a_session_record_shows_its_agent_counts_times_and_title() {
        let listing = listing(&sessions(), &window((11, 38), (11, 45)), 0, 1);

        let output = format_sessions(&listing, None);

        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "protocol agent-sessions cut=none chars=none");
        assert_eq!(lines[1], "sessions matched=3 offset=0 listed=1");
        let record = lines[2];
        assert!(record.starts_with("session project=pr_"), "{record}");
        assert!(record.contains(" ref=ch_"), "{record}");
        assert!(
            record.contains(" agent=claude messages=1 messages-in-window=1 at="),
            "{record}"
        );
        assert!(record.contains(" to="), "{record}");
        assert!(record.ends_with(" | spanning"), "{record}");
        assert_eq!(
            lines[3],
            "continue sessions offset=1 action=rerun-with-offset"
        );
    }

    #[test]
    fn a_listing_without_a_window_doesnt_show_a_messages_in_window_count() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);

        let output = format_sessions(&listing, None);

        assert!(!output.contains("messages-in-window="), "{output}");
        assert!(!output.contains("continue sessions"), "{output}");
    }

    #[test]
    fn a_budgeted_listing_continues_from_the_first_session_it_omits() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);
        let unbudgeted = format_sessions(&listing, None);
        let sessions_line_and_two_records = unbudgeted
            .lines()
            .skip(1)
            .take(3)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>();
        let continue_record = "continue sessions offset=2 action=rerun-with-offset\n";
        let budget = budget_with_header(sessions_line_and_two_records + continue_record.len());

        let output = format_sessions(&listing, Some(budget));

        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with(&format!(
                "protocol agent-sessions cut=tail chars={budget}\n"
            )),
            "{output}"
        );
        assert!(output.contains("listed=2\n"), "{output}");
        assert!(output.ends_with(continue_record), "{output}");
    }

    #[test]
    fn a_budget_that_omits_a_warning_record_marks_the_cut() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);
        let warnings = [AgentWarning::ignored("an ignored term".to_string())];
        let unbudgeted = format_sessions_with_warnings(&listing, None, &warnings);
        let warning_record = unbudgeted.lines().last().unwrap();
        let budget = unbudgeted.chars().count() - warning_record.chars().count();

        let output = format_sessions_with_warnings(&listing, Some(budget), &warnings);

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
        assert!(output.contains("listed=5\n"), "{output}");
        assert!(!output.contains("protocol agent-warning"), "{output}");
    }

    #[test]
    fn the_output_is_truncated_to_a_budget_shorter_than_the_header() {
        let listing = listing(&sessions(), &TimeFilter::default(), 0, 10);

        let output = format_sessions(&listing, Some(20));

        assert_eq!(output.chars().count(), 20);
        assert!(!output.ends_with('\n'), "{output}");
    }

    #[test]
    fn a_session_is_identified_by_its_project_uuid_and_ref() {
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
