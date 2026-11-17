//! The session list's search over each registered agent's every-record
//! session, read from a session cache into memory. It finds each searched
//! marker, as `agent search` does, unquoted and quoted: in the dialogue, a
//! tool call's header, thinking or a sub-agent's text. Neither finds a hidden
//! marker or a marker in a call's body. A found marker's context line shows
//! the marker, and a query the dialogue matches shows the dialogue.

use crate::history::Conversation;
use crate::history::provider::contract_tests::{AgentSession, every_record_sessions};
use crate::search::evidence::build_lexical_evidence;
use crate::search::lexical::{agent_search, precompute_agent_search_text};
use crate::search::literal::Literal;
use crate::search::preview::build_context_segments_from_ranges;
use crate::search::query::ParsedQuery;
use crate::search::test_fixtures::{cached_claude_sessions, october};
use crate::search::{SearchTextsInMemory, precompute_search_text, search_with_texts};
use crate::time_filter::TimeFilter;

/// One session's row as the session list holds it: read from a session
/// cache, with its search texts in memory.
struct ListedSession {
    rows: [Conversation; 1],
    texts: SearchTextsInMemory,
    /// Keeps the session cache's directory for the test.
    _cache: Box<dyn std::any::Any>,
}

impl ListedSession {
    fn of(session: &AgentSession) -> Self {
        let cache = tempfile::tempdir().unwrap();
        let row = crate::history::sqlite_cache::tests::cached_row(cache.path(), session);
        Self::holding(row, Box::new(cache))
    }

    fn holding(row: Conversation, cache: Box<dyn std::any::Any>) -> Self {
        let rows = [row];
        let texts = SearchTextsInMemory::read(&rows);
        assert!(
            texts.get(&rows[0]).is_some(),
            "the session's texts are read into memory"
        );
        Self {
            rows,
            texts,
            _cache: cache,
        }
    }

    fn is_found_by_list_search(&self, query: &str) -> bool {
        let searchable = precompute_search_text(&self.rows);
        !search_with_texts(&self.rows, &searchable, &self.texts, query, october(20, 12)).is_empty()
    }

    fn is_found_by_agent_search(&self, query: &str) -> bool {
        let texts = precompute_agent_search_text(&self.rows, &TimeFilter::default());
        !agent_search(&self.rows, &texts, query, october(20, 12)).is_empty()
    }

    /// The context line a row 200 columns wide shows for `query`; `None`
    /// when the row doesn't show one.
    fn context_line(&self, query: &str) -> Option<String> {
        let row = &self.rows[0];
        let evidence =
            build_lexical_evidence(row, self.texts.get(row), &ParsedQuery::parse(query))?;
        build_context_segments_from_ranges(&evidence.texts(row), &evidence.context_ranges, 200)
    }
}

fn quoted(text: &str) -> String {
    format!("\"{text}\"")
}

#[test]
fn session_list_search_and_agent_search_find_each_searched_marker_and_no_other() {
    let mut gaps = Vec::new();
    for session in every_record_sessions() {
        let listed = ListedSession::of(&session);
        let source = session.row.source;
        for marker in session.markers.iter().filter(|marker| marker.is_searched) {
            for query in [marker.text.to_owned(), quoted(marker.text)] {
                if !listed.is_found_by_list_search(&query) {
                    gaps.push(format!(
                        "{source:?}: the list misses the {} for {query}",
                        marker.record
                    ));
                }
                if !listed.is_found_by_agent_search(&query) {
                    gaps.push(format!(
                        "{source:?}: `agent search` misses the {} for {query}",
                        marker.record
                    ));
                }
            }
        }
        let unsearched = session
            .markers
            .iter()
            .filter(|marker| !marker.is_searched)
            .map(|marker| (marker.record, marker.text));
        for (record, text) in unsearched.chain(session.hidden.iter().copied()) {
            let query = quoted(text);
            if listed.is_found_by_list_search(&query) {
                gaps.push(format!("{source:?}: the list finds the {record}"));
            }
            if listed.is_found_by_agent_search(&query) {
                gaps.push(format!("{source:?}: `agent search` finds the {record}"));
            }
        }
    }

    assert!(
        gaps.is_empty(),
        "both searches find each searched marker, and neither finds another:\n{}",
        gaps.join("\n")
    );
}

#[test]
fn context_line_shows_each_searched_marker_the_preview_doesnt_show() {
    let mut gaps = Vec::new();
    let mut lines = 0;
    for session in every_record_sessions() {
        let listed = ListedSession::of(&session);
        let row = &listed.rows[0];
        for marker in session.markers.iter().filter(|marker| marker.is_searched) {
            let literal = Literal::new(marker.text.to_owned());
            match listed.context_line(&quoted(marker.text)) {
                Some(line) if literal.matches(&line) => lines += 1,
                Some(line) => gaps.push(format!(
                    "{:?}: the {}'s context line is {line:?}",
                    row.source, marker.record
                )),
                None if literal.matches(&row.preview) => {}
                None => gaps.push(format!(
                    "{:?}: the {} doesn't have a context line",
                    row.source, marker.record
                )),
            }
        }
    }

    assert!(
        gaps.is_empty(),
        "a marker outside the preview shows in its context line:\n{}",
        gaps.join("\n")
    );
    assert!(lines > 0, "some marker has a context line");
}

/// A Claude Code session whose dialogue holds `needle` in a reply, and whose
/// tool call holds `needle` and `haystack`.
fn session_matching_in_dialogue_and_tool_call() -> ListedSession {
    let at = |hour| october(10, hour).to_rfc3339();
    let user = |hour, text: &str| {
        serde_json::json!({"type": "user", "timestamp": at(hour),
            "message": {"role": "user", "content": text}})
    };
    let assistant = |hour, block: serde_json::Value| {
        serde_json::json!({"type": "assistant", "timestamp": at(hour),
            "message": {"role": "assistant", "content": [block]}})
    };
    let reply =
        |hour, text: &str| assistant(hour, serde_json::json!({"type": "text", "text": text}));
    let records = vec![
        user(10, "start the work"),
        reply(11, "the work is started"),
        user(12, "count them"),
        reply(13, "the needle count is three"),
        assistant(
            14,
            serde_json::json!({"type": "tool_use", "id": "call", "name": "Bash",
                "input": {"command": "grep needle haystack.txt"}}),
        ),
        user(15, "list the files"),
        // Longer than a context line, so a snippet of the dialogue's hit ends
        // ahead of the tool call.
        reply(16, &"the files are listed ".repeat(20)),
        user(17, "thanks, that is all"),
    ];
    let sessions = cached_claude_sessions(&[("session", records)]);
    ListedSession::holding(sessions.rows[0].clone(), Box::new(sessions))
}

#[test]
fn context_line_shows_dialogue_hit_and_tool_call_only_for_term_dialogue_doesnt_hold() {
    let listed = session_matching_in_dialogue_and_tool_call();

    let dialogue_line = listed.context_line("needle").unwrap_or_else(|| {
        panic!(
            "a hit outside the preview {:?} has a context line",
            listed.rows[0].preview
        )
    });
    assert!(
        dialogue_line.contains("the needle count is three") && !dialogue_line.contains("grep"),
        "a query the dialogue matches shows the dialogue alone: {dialogue_line:?}"
    );

    let tool_call_line = listed
        .context_line("needle haystack")
        .expect("a hit outside the preview has a context line");
    assert!(
        tool_call_line.contains("grep needle haystack.txt"),
        "a term the dialogue doesn't hold shows the tool call: {tool_call_line:?}"
    );
}
