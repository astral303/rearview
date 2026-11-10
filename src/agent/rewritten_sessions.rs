//! `agent search` over sessions another rearview writes again or removes
//! while the search reads them: the ranking and the hit read each read the
//! copy the cache holds, whole, and a session the cache no longer lists
//! leaves the results.

use crate::agent::refs::AgentConversationKey;
use crate::agent::search::SearchedTranscript;
use crate::agent::service::{lexically_rank_scoped, read_hit_messages};
use crate::agent::transcript::agent_part_text;
use crate::clock::TestClock;
use crate::history::WaitStatus;
use crate::history::sqlite_cache::connection::test_hooks::{
    ReaderAction, ReaderOpening, run_as_a_reader_opens,
};
use crate::search::AgentRanking;
use crate::search::mode::SearchMode;
use crate::search::test_fixtures::{
    CachedClaudeSessions, cached_claude_sessions, claude_user_record, october,
};
use crate::time_filter::TimeFilter;

/// User messages with `texts`, an hour apart from noon on 10 October.
fn messages(texts: &[&str]) -> Vec<serde_json::Value> {
    texts
        .iter()
        .zip(12..)
        .map(|(text, hour)| claude_user_record(text, october(10, hour)))
        .collect()
}

fn rank(sessions: &CachedClaudeSessions, query: &str) -> AgentRanking {
    let scope = (0..sessions.rows.len()).collect::<Vec<_>>();
    lexically_rank_scoped(
        &sessions.rows,
        query,
        &scope,
        crate::agent::search::AgentSearchScope::Global,
        SearchMode::Lexical,
        &TimeFilter::default(),
        &TestClock::default(),
    )
    .unwrap()
}

/// The hit read of row `index` for `ranking`; `None` when its session left
/// the cache.
fn read_hits(
    sessions: &CachedClaudeSessions,
    ranking: &AgentRanking,
    index: usize,
) -> Option<SearchedTranscript> {
    let row = &sessions.rows[index];
    let key = AgentConversationKey::from_conversation(
        row,
        &crate::history::path::CanonicalDirectories::of_system(),
    )
    .unwrap();
    read_hit_messages(
        &TestClock::default(),
        &WaitStatus::default(),
        ranking,
        row,
        ranking.hit_messages.of(index).expect("the scan counted it"),
        &key,
    )
    .unwrap()
}

/// Each read message's number and text.
fn numbered_texts(read: &SearchedTranscript) -> Vec<(usize, String)> {
    read.transcript
        .messages
        .iter()
        .map(|message| {
            let text = message
                .parts
                .iter()
                .filter_map(agent_part_text)
                .collect::<Vec<_>>()
                .join(" ");
            (message.ordinal, text)
        })
        .collect()
}

/// A rewrite inserting a message before the hit and appending one holding
/// the query renumbers the session; the hit read reads the new copy's hit
/// messages, whether the rewrite lands before the read or as it opens its
/// reader, after another rewrite.
#[test]
fn hit_read_reads_hit_messages_of_copy_cache_holds_as_it_reads() {
    let renumbered = messages(&["inserted", "zebrafish one", "plain", "zebrafish three"]);
    let expected = [
        (2, "zebrafish one".to_owned()),
        (4, "zebrafish three".to_owned()),
    ];
    for rewrites_as_read_opens in [false, true] {
        let sessions =
            cached_claude_sessions(&[("session", messages(&["zebrafish one", "plain"]))]);
        let ranking = rank(&sessions, "zebrafish");
        assert_eq!(ranking.hit_messages.of(0).unwrap().ordinals, [1]);

        let rewrite = if rewrites_as_read_opens {
            sessions.rewrite(0, &messages(&["zebrafish elsewhere"]));
            let (writer, after) = (sessions.writer(0), renumbered.clone());
            Some(run_as_a_reader_opens(
                sessions.cache_path(),
                ReaderOpening::OnThisThread(1),
                move || writer.rewrite(&after),
            ))
        } else {
            sessions.rewrite(0, &renumbered);
            None
        };

        let read = read_hits(&sessions, &ranking, 0).expect("the cache holds the session");
        assert!(rewrite.as_ref().is_none_or(ReaderAction::has_run));
        assert_eq!(
            numbered_texts(&read),
            expected,
            "the hit read reads the copy the cache holds, rewritten as it read: \
             {rewrites_as_read_opens}"
        );
    }
}

/// A rewrite during the ranking scan, and another as the session is counted
/// again alone, leave the session ranked on the copy the cache holds last.
#[test]
fn ranking_counts_copy_cache_holds_after_rewrites_during_scan() {
    let sessions = cached_claude_sessions(&[("session", messages(&["plain"]))]);
    let (during_scan, as_recount_opens) = (sessions.writer(0), sessions.writer(0));
    let rewrites = [
        run_as_a_reader_opens(
            sessions.cache_path(),
            ReaderOpening::FirstOnAnotherThread,
            move || during_scan.rewrite(&messages(&["plain", "quagga appended"])),
        ),
        run_as_a_reader_opens(
            sessions.cache_path(),
            ReaderOpening::OnThisThread(2),
            move || as_recount_opens.rewrite(&messages(&["quagga first", "plain", "quagga last"])),
        ),
    ];

    let ranking = rank(&sessions, "quagga");

    assert!(rewrites.iter().all(ReaderAction::has_run));
    assert_eq!(ranking.ranked, [0], "the rewritten session ranks");
    assert_eq!(
        ranking.hit_messages.of(0).unwrap().ordinals,
        [1, 3],
        "the hit messages are the last copy's"
    );
}

/// A session removed during the ranking scan leaves the ranking; one
/// removed after it leaves the hit read.
#[test]
fn session_removed_during_search_leaves_ranking_or_hit_read() {
    let both = || {
        cached_claude_sessions(&[
            ("removed", messages(&["quagga here"])),
            ("kept", messages(&["quagga there"])),
        ])
    };
    let sessions = both();
    let removed = sessions.writer(0);
    let remove = run_as_a_reader_opens(
        sessions.cache_path(),
        ReaderOpening::FirstOnAnotherThread,
        move || removed.remove(),
    );

    let ranking = rank(&sessions, "quagga");

    assert!(remove.has_run());
    assert_eq!(ranking.ranked, [1], "the removed session is not ranked");
    assert_eq!(
        ranking.left_cache,
        [0],
        "the ranking names the removed session"
    );
    assert!(ranking.hit_messages.of(0).is_none());

    let sessions = both();
    let ranking = rank(&sessions, "quagga");
    sessions.remove(0);
    assert!(
        read_hits(&sessions, &ranking, 0).is_none(),
        "the hit read finds the session left the cache"
    );
    assert!(read_hits(&sessions, &ranking, 1).is_some());
}
