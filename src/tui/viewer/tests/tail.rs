//! A render of a conversation's tail after the shown rows of an earlier
//! copy, from the start `tail_render_start` gives, against a whole render of
//! the changed conversation with the same options.

use super::rerender::{
    Modes, Shown, conversation_of, conversation_of_records, first_expanded_sets,
    interleaved_calls_across_a_day, long_call, long_result, run_with_results_in_later_entries,
    runs_around_midnight, runs_followed_by_entries_rendering_nothing, text,
    tool_runs_between_messages,
};
use super::*;
use crate::history::Source;
use crate::history::format::RecordLine;
use crate::log_entry::{ContentBlock, SubagentIdentity, UserContent};
use crate::tui::viewer::tail::{
    RowsToRender, ShownRender, TailStart, splice_tail, tail_render_start,
};

/// The rows of a whole render of a copy of a conversation: the copy, the
/// render's message ranges and its options.
struct ShownCopy<'a> {
    conversation: &'a ParsedConversation,
    messages: &'a [MessageRange],
    options: &'a RenderOptions,
}

impl<'a> ShownCopy<'a> {
    /// The shown rows before a newer copy, changed from the entry numbered
    /// `first_changed` on, as an extension of the shown conversation leaves
    /// them.
    fn changed_from(&self, first_changed: usize) -> ShownRender<'a> {
        let entries = &self.conversation.entries;
        let kept = entries.partition_point(|parsed| parsed.entry_index < first_changed);
        ShownRender {
            replaced: &entries[kept..],
            links: &self.conversation.links,
            messages: self.messages,
            options: self.options,
        }
    }
}

/// The entries of `source`'s session at `file_path`, with the sub-agent
/// transcripts `subagents`, as the viewer reads them from the cache once a
/// load cached the session.
fn stored_entries(
    source: Source,
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
    .unwrap_or_else(|miss| panic!("the cache contains the copy the load wrote: {miss:?}"))
    .entries
}

/// The conversation of the first `count` of `entries`, as the viewer reads
/// a copy containing them alone.
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

/// `shown` with `tail` put in place of its rows from `row` on, as the
/// viewer puts a tail in place.
fn spliced(mut shown: Shown, tail: RenderedConversation, row: usize) -> Shown {
    let old_len = shown.lines.len();
    let tail_len = tail.lines.len();
    let spliced = splice_tail(shown.rows(), tail, row);
    assert_eq!(
        (
            spliced.replaced.old,
            spliced.replaced.new_len,
            spliced.removed.len()
        ),
        (row..old_len, tail_len, old_len - row),
        "the tail replaces the shown rows from row {row} on"
    );
    shown
}

/// The count of option sets of [`assert_change_renders_as_whole_render`]
/// that rendered a change from a tail and of those that rendered it whole,
/// and the entries the tails started at.
#[derive(Debug, Default, PartialEq)]
struct Starts {
    tails: usize,
    wholes: usize,
    tail_entries: BTreeSet<usize>,
}

impl std::ops::AddAssign for Starts {
    fn add_assign(&mut self, other: Self) {
        self.tails += other.tails;
        self.wholes += other.wholes;
        self.tail_entries.extend(other.tail_entries);
    }
}

/// A changed conversation and the option sets a check renders it with, each
/// with a whole render: every mode, with nothing expanded and in summary
/// mode every run expanded.
struct WholeRenders<'a> {
    conversation: &'a ParsedConversation,
    renders: Vec<(Modes, RenderOptions, Shown)>,
}

impl<'a> WholeRenders<'a> {
    fn of(conversation: &'a ParsedConversation) -> Self {
        let mut renders = Vec::new();
        for modes in Modes::every() {
            for expanded in first_expanded_sets(conversation, modes) {
                let options = modes.options(expanded);
                let whole = Shown::of(render_parsed_conversation(conversation, &options));
                renders.push((modes, options, whole));
            }
        }
        Self {
            conversation,
            renders,
        }
    }
}

/// With each option set of `whole`: the shown rows of `shown_conversation`,
/// then the tail `tail_render_start` gives for the changed conversation,
/// changed from the entry numbered `first_changed` on, render as the whole
/// render.
fn assert_change_renders_as_whole_render(
    case: &str,
    shown_conversation: &ParsedConversation,
    whole: &WholeRenders<'_>,
    first_changed: usize,
) -> Starts {
    let mut starts = Starts::default();
    for (modes, options, whole_render) in &whole.renders {
        let shown = Shown::of(render_parsed_conversation(shown_conversation, options));
        let shown_render = ShownCopy {
            conversation: shown_conversation,
            messages: &shown.messages,
            options,
        }
        .changed_from(first_changed);
        let start =
            match tail_render_start(shown_render, whole.conversation, first_changed, options) {
                RowsToRender::Tail(start) => start,
                RowsToRender::Whole => {
                    starts.wholes += 1;
                    continue;
                }
            };
        let tail = render_tail_while(whole.conversation, options, start.entry_index, &|| true)
            .expect("a render that keeps going finishes");
        assert_eq!(
            spliced(shown, tail, start.row).shape(),
            whole_render.shape(),
            "{case} {modes:?}, changed from entry {first_changed}: the tail from {start:?} \
             after the shown rows renders as a whole render"
        );
        starts.tails += 1;
        starts.tail_entries.insert(start.entry_index);
    }
    starts
}

/// [`assert_change_renders_as_whole_render`] for each split of `entries`:
/// the first `split` entries shown, all of them in the changed copy.
fn assert_tails_render_as_whole_render<E: Clone>(name: &str, entries: &[E]) -> Starts
where
    DisplayEntries: From<Vec<E>>,
{
    let conversation = conversation_of_first(entries, entries.len());
    let whole = WholeRenders::of(&conversation);
    let mut starts = Starts::default();
    for split in 0..entries.len() {
        starts += assert_change_renders_as_whole_render(
            &format!("{name}, split at entry {split}"),
            &conversation_of_first(entries, split),
            &whole,
            split,
        );
    }
    starts
}

/// [`assert_change_renders_as_whole_render`] from `shown` to `changed`, two
/// lists of entries the viewer reads of one session, the same before the
/// entry numbered `first_changed`.
fn assert_entries_change_renders_as_whole_render(
    case: &str,
    shown: &[(RecordLine, LogEntry)],
    changed: &[(RecordLine, LogEntry)],
    first_changed: usize,
) -> Starts {
    let changed = conversation_of_first(changed, changed.len());
    assert_change_renders_as_whole_render(
        case,
        &conversation_of_first(shown, shown.len()),
        &WholeRenders::of(&changed),
        first_changed,
    )
}

/// In truncated mode with timing hidden, a render without day labels: the
/// tail starts at the entry numbered `entry_index` and replaces the shown
/// rows from its message's first row on.
fn assert_truncated_mode_tail_starts_at(
    shown_conversation: &ParsedConversation,
    conversation: &ParsedConversation,
    first_changed: usize,
    entry_index: usize,
) {
    let options = test_render_options(ToolDisplayMode::Truncated);
    let shown = render_parsed_conversation(shown_conversation, &options);
    let message = shown
        .messages
        .iter()
        .find(|message| message.entry_index == entry_index)
        .unwrap_or_else(|| panic!("a shown message starts at entry {entry_index}"));
    let start = tail_render_start(
        ShownCopy {
            conversation: shown_conversation,
            messages: &shown.messages,
            options: &options,
        }
        .changed_from(first_changed),
        conversation,
        first_changed,
        &options,
    );
    assert_eq!(
        start,
        RowsToRender::Tail(TailStart {
            entry_index,
            row: message.start_line,
        }),
        "the tail replaces the rows of the message at entry {entry_index} on"
    );
}

/// The every-record sessions of every agent, read from the cache.
pub(super) fn every_agents_stored_entries() -> Vec<(String, Vec<(RecordLine, LogEntry)>)> {
    use crate::history::provider::contract_tests::every_record_sessions;
    every_record_sessions()
        .iter()
        .map(|session| {
            let row = &session.row;
            (
                format!("{:?}", row.source),
                stored_entries(row.source, &row.path, &row.subagents),
            )
        })
        .collect()
}

/// An agent's every-record session, written below a temporary home where
/// discovery finds it. A test changes these files, not the fixture files
/// other tests read at the same time.
struct WrittenSession {
    _home: tempfile::TempDir,
    source: Source,
    transcript: PathBuf,
    subagents: Vec<PathBuf>,
}

/// [`WrittenSession`]s of every agent.
fn written_every_record_sessions() -> Vec<WrittenSession> {
    use crate::history::provider::contract_tests::registered_fixtures;
    registered_fixtures()
        .into_iter()
        .map(|fixture| {
            let home = tempfile::tempdir().unwrap();
            let session = fixture.every_record_session();
            fixture.write_every_record_session(home.path(), &session);
            let stubs = fixture.discover_under(home.path());
            let [stub] = stubs.as_slice() else {
                panic!(
                    "{:?}: discovery finds the session written: {stubs:?}",
                    session.row.source
                );
            };
            WrittenSession {
                source: session.row.source,
                transcript: stub.locator.clone(),
                subagents: stub.subagents.clone(),
                _home: home,
            }
        })
        .collect()
}

/// The session of `file_path` the viewer reads before and after `change`
/// rewrites its files, and the first changed entry, by the copies' entry
/// hashes.
fn copies_around(
    source: Source,
    file_path: &Path,
    subagents: &[PathBuf],
    change: impl FnOnce(),
) -> (ViewedConversation, ViewedConversation, usize) {
    let shown = viewed_copy_of(source, file_path, subagents);
    change();
    let changed = viewed_copy_of(source, file_path, subagents);
    let first_changed = shown
        .entry_hashes
        .first_change_in(&changed.entry_hashes)
        .expect("the change changes an entry");
    (shown, changed, first_changed)
}

/// The first `count` lines of `path`, then the rest once the returned
/// closure runs.
fn shorten_until_restored(path: &Path, count: usize) -> impl FnOnce() + use<> {
    let whole = std::fs::read_to_string(path).unwrap();
    let kept: String = whole
        .split_inclusive('\n')
        .take(count)
        .collect::<Vec<_>>()
        .concat();
    std::fs::write(path, kept).unwrap();
    let path = path.to_path_buf();
    move || std::fs::write(path, whole).unwrap()
}

#[test]
fn every_agents_tail_renders_after_its_earlier_rows_as_whole_render() {
    for (name, entries) in every_agents_stored_entries() {
        let starts = assert_tails_render_as_whole_render(&name, &entries);
        assert!(
            starts.tails > 0,
            "{name}: a split renders a tail: {starts:?}"
        );
    }
}

/// Day labels, interleaved calls, tool runs whose results arrive later,
/// runs around midnight, and a day label without a row after it, which the
/// shown render drops: every split renders as a whole render.
#[test]
fn tails_of_calls_runs_and_day_labels_render_after_earlier_rows_as_whole_render() {
    let mut fixtures = vec![
        ("runs", tool_runs_between_messages()),
        ("interleaved", interleaved_calls_across_a_day()),
        ("late results", run_with_results_in_later_entries()),
        (
            "entries rendering nothing",
            runs_followed_by_entries_rendering_nothing(),
        ),
        (
            "label without a row after it",
            conversation_of(vec![
                stamped_message(0, "user", &day_one(), "first question"),
                stamped_message(1, "assistant", &day_one_later(), "first answer"),
                stamped_subagent_message(2, &day_three(), "sub-agent work"),
                stamped_message(3, "user", &day_three_later(), "next question"),
            ]),
        ),
    ];
    fixtures.extend(
        runs_around_midnight()
            .into_iter()
            .map(|(name, conversation, _)| (name, conversation)),
    );
    for (name, conversation) in fixtures {
        let starts = assert_tails_render_as_whole_render(name, &numbered_entries(&conversation));
        assert!(
            starts.tails > 0,
            "{name}: a split renders a tail: {starts:?}"
        );
    }
}

/// A reply appended after a prompt: the tail starts at the prompt, the last
/// shown message.
#[test]
fn appended_reply_renders_from_last_shown_message_as_whole_render() {
    let at = Some("2026-09-26T12:00:00Z");
    let entries = numbered_entries(&conversation_of_records(&[
        text("user", at, "Start the build."),
        text("assistant", at, "The build runs."),
        text("user", at, "Did it pass?"),
        text("assistant", at, "It passed."),
    ]));
    let shown = conversation_of_first(&entries, 3);
    let changed = conversation_of_first(&entries, entries.len());

    let starts =
        assert_change_renders_as_whole_render("appended", &shown, &WholeRenders::of(&changed), 3);

    assert_eq!(
        starts,
        Starts {
            tails: 12,
            wholes: 0,
            tail_entries: BTreeSet::from([2])
        },
        "every option set renders a tail from the last shown message"
    );
    assert_truncated_mode_tail_starts_at(&shown, &changed, 3, 2);
}

/// A result answering a call two messages back, across a text message: the
/// tail starts at the call.
#[test]
fn result_for_call_messages_back_renders_from_call_as_whole_render() {
    let at = Some("2026-09-26T12:00:00Z");
    let entries = numbered_entries(&conversation_of_records(&[
        text("user", at, "Start the build."),
        long_call(at, "toolu_build"),
        text("assistant", at, "The build runs in the background."),
        text("user", at, "Tell me when it ends."),
        long_result(at, "toolu_build"),
        text("assistant", at, "It passed."),
    ]));
    let shown = conversation_of_first(&entries, 4);
    let changed = conversation_of_first(&entries, entries.len());

    let starts = assert_change_renders_as_whole_render(
        "late result",
        &shown,
        &WholeRenders::of(&changed),
        4,
    );

    assert_eq!(
        starts,
        Starts {
            tails: 16,
            wholes: 0,
            tail_entries: BTreeSet::from([1])
        },
        "every option set renders a tail from the call"
    );
    assert_truncated_mode_tail_starts_at(&shown, &changed, 4, 1);
}

/// A Claude session's task report, handed back in the tail, doesn't change
/// an earlier row: every split renders as a whole render.
#[test]
fn task_report_in_tail_renders_as_whole_render() {
    use crate::history::format::claude::subagent_report::test_support::write_handback_session;
    let project = tempfile::tempdir().unwrap();
    let (transcript, subagents) = write_handback_session(project.path(), true);
    let entries = stored_entries(Source::Claude, &transcript, &subagents);

    let starts = assert_tails_render_as_whole_render("task report", &entries);

    assert!(starts.tails > 0, "a split renders a tail: {starts:?}");
}

/// Each sub-agent transcript of each agent's every-record session, first
/// half its length, then whole: the turns it gains splice among the
/// parent's entries.
#[test]
fn sub_agents_turns_spliced_among_parents_entries_render_as_whole_render() {
    let mut spliced_for = Vec::new();
    for session in written_every_record_sessions() {
        let subagents: Vec<PathBuf> = session
            .subagents
            .iter()
            .filter(|subagent| subagent.is_file())
            .cloned()
            .collect();
        for subagent in &subagents {
            let lines = std::fs::read_to_string(subagent).unwrap().lines().count();
            let restore = shorten_until_restored(subagent, (lines / 2).max(1));
            let (shown, changed, first_changed) =
                copies_around(session.source, &session.transcript, &subagents, restore);
            let starts = assert_change_renders_as_whole_render(
                &format!(
                    "{:?} sub-agent {:?}",
                    session.source,
                    subagent.file_name().unwrap()
                ),
                &shown.conversation,
                &WholeRenders::of(&changed.conversation),
                first_changed,
            );
            let splices_among_shown_entries = first_changed < shown.conversation.origins.len();
            if starts.tails > 0 && splices_among_shown_entries {
                spliced_for.push(session.source);
            }
        }
    }
    spliced_for.dedup();
    assert_eq!(
        spliced_for,
        [
            Source::Claude,
            Source::Codex,
            Source::Kimi,
            Source::Grok,
            Source::Omp
        ],
        "the agents whose sub-agent turns splice before entries the shown copy contains render a tail"
    );
}

/// A Codex session whose `spawn_agent` result, then a reply, show before
/// the sub-agent's rollout exists. Once it does, the result shows the
/// sub-agent's nickname and role: the tail starts at the call the result
/// answers.
#[test]
fn spawn_result_shown_before_sub_agents_identity_renders_as_whole_render() {
    let dir = tempfile::tempdir().unwrap();
    let parent_thread = "019f0000-0000-7000-8000-0000000000a1";
    let parent = dir.path().join("rollout-parent.jsonl");
    let parent_rollout = [
        codex_record(
            0,
            "session_meta",
            &format!(
                r#"{{"id":"{parent_thread}","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/project"}}"#
            ),
        ),
        codex_record(
            0,
            "response_item",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Scout the modules."}]}"#,
        ),
        codex_record(
            1,
            "response_item",
            r#"{"type":"function_call","call_id":"call_scout","name":"spawn_agent","arguments":"{\"task_name\":\"scout\",\"agent_type\":\"explorer\",\"message\":\"List the modules.\"}"}"#,
        ),
        codex_record(
            1,
            "response_item",
            r#"{"type":"function_call_output","call_id":"call_scout","output":"{\"task_name\":\"/root/scout\"}"}"#,
        ),
        codex_reply(1, "The scout lists the modules."),
        codex_closing_prompt(),
    ];
    std::fs::write(&parent, parent_rollout.join("\n") + "\n").unwrap();
    let sub_agent = dir.path().join("rollout-scout.jsonl");
    let sub_agent_rollout = [
        codex_record(
            2,
            "session_meta",
            &format!(
                r#"{{"id":"{SUB_AGENT_THREAD}","timestamp":"2026-08-01T10:00:02.000Z","cwd":"/tmp/project","parent_thread_id":"{parent_thread}","agent_nickname":"Lorentz","agent_role":"explorer","agent_path":"/root/scout"}}"#
            ),
        ),
        codex_reply(3, "The modules are listed."),
    ];

    let shown = viewed_copy_of(Source::Codex, &parent, &[]);
    std::fs::write(&sub_agent, sub_agent_rollout.join("\n") + "\n").unwrap();
    let changed = viewed_copy_of(Source::Codex, &parent, std::slice::from_ref(&sub_agent));
    let first_changed = shown
        .entry_hashes
        .first_change_in(&changed.entry_hashes)
        .expect("the sub-agent's turns change the copy");
    let whole = rendered_text(&render_parsed_conversation(
        &changed.conversation,
        &test_render_options(ToolDisplayMode::Truncated),
    ));
    assert!(
        whole.contains("Lorentz (explorer) · /root/scout"),
        "the result shows the sub-agent it started:\n{whole}"
    );
    let call = changed
        .conversation
        .entries
        .iter()
        .find(|parsed| {
            parsed.entry.content_blocks().iter().any(
                |block| matches!(block, ContentBlock::ToolUse { id, .. } if id == "call_scout"),
            )
        })
        .expect("the session contains the spawn call")
        .entry_index;

    let starts = assert_change_renders_as_whole_render(
        "spawn result",
        &shown.conversation,
        &WholeRenders::of(&changed.conversation),
        first_changed,
    );

    assert_eq!(
        starts.tail_entries,
        BTreeSet::from([call]),
        "every tail starts at the spawn call: {starts:?}"
    );
    assert_truncated_mode_tail_starts_at(
        &shown.conversation,
        &changed.conversation,
        first_changed,
        call,
    );
}

/// An answer arriving in the tail, after a reply below the question's
/// result: the tail starts at the question call, whose result gains the
/// `Answer` row.
#[test]
fn answer_in_tail_renders_its_questions_result_again() {
    let conversation = codex_session_of(&[
        codex_record(
            0,
            "response_item",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Ship the notes."}]}"#,
        ),
        codex_question_call(1, serde_json::json!([{"title": "Ship it now?"}])),
        codex_reply(2, "I'll wait for your answer."),
        codex_answer(30, 0, "Ship it now?", "Yes"),
    ]);
    let entries = numbered_entries(&conversation);
    let first_changed = entries.len() - 1;
    let shown = conversation_of_first(&entries, first_changed);
    let call = 1;

    let starts = assert_change_renders_as_whole_render(
        "answer in tail",
        &shown,
        &WholeRenders::of(&conversation),
        first_changed,
    );

    assert!(starts.tails > 0, "a tail renders the answer: {starts:?}");
    assert_truncated_mode_tail_starts_at(&shown, &conversation, first_changed, call);
}

/// A Codex session whose result received without a call shows before any
/// reply names the agent: once a reply does, summary mode renders whole.
#[test]
fn session_agent_named_in_tail_renders_summary_mode_whole() {
    let rollout = [
        codex_record(
            0,
            "session_meta",
            r#"{"id":"019f0000-0000-7000-8000-0000000000a1","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/project"}"#,
        ),
        codex_record(
            1,
            "response_item",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Wait for the scout."}]}"#,
        ),
        codex_record(
            2,
            "response_item",
            r#"{"type":"function_call_output","id":"item_received","name":"send_message_to_thread","output":"The scout is done."}"#,
        ),
        codex_record(
            3,
            "response_item",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Go on."}]}"#,
        ),
        codex_reply(4, "Done."),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, rollout.join("\n") + "\n").unwrap();
    let entries = stored_entries(Source::Codex, &path, &[]);
    let reply = entries
        .iter()
        .position(|(_, entry)| matches!(entry, LogEntry::Assistant { .. }))
        .expect("the session contains a reply");

    let starts = assert_entries_change_renders_as_whole_render(
        "session agent",
        &entries[..reply],
        &entries,
        reply,
    );

    assert_eq!(
        (starts.tails, starts.wholes),
        (8, 8),
        "summary mode renders whole, both detail modes from a tail: {starts:?}"
    );
}

/// The first `count` entries of a sub-agent's turn, keyed `toolu_parent`,
/// between two exchanges, with `links`.
fn sub_agent_turn_between_exchanges(count: usize, links: ConversationLinks) -> ParsedConversation {
    let mut entries = vec![
        stamped_message(0, "user", &day_one(), "first question"),
        stamped_message(1, "assistant", &day_one_later(), "first answer"),
        stamped_subagent_message(2, &day_one_later(), "sub-agent work"),
        stamped_message(3, "user", &day_one_later(), "next question"),
        stamped_message(4, "assistant", &day_one_later(), "next answer"),
    ];
    entries.truncate(count);
    ParsedConversation {
        links,
        ..conversation_of(entries)
    }
}

/// Links labelling the sub-agent `toolu_parent` with `nickname`.
fn links_naming(nickname: &str) -> ConversationLinks {
    ConversationLinks::from_identities([(
        "toolu_parent",
        &SubagentIdentity {
            nickname: Some(nickname.to_owned()),
            ..SubagentIdentity::default()
        },
    )])
}

/// A label for a shown sub-agent, new in the changed links: the tail starts
/// at the sub-agent's turn while it shows, and at the message before it
/// while thinking is hidden.
#[test]
fn new_label_of_shown_sub_agent_renders_from_its_turn_as_whole_render() {
    let shown = sub_agent_turn_between_exchanges(4, ConversationLinks::default());
    let changed = sub_agent_turn_between_exchanges(5, links_naming("Lorentz"));

    let starts =
        assert_change_renders_as_whole_render("new label", &shown, &WholeRenders::of(&changed), 4);

    assert_eq!(
        starts,
        Starts {
            tails: 12,
            wholes: 0,
            tail_entries: BTreeSet::from([1, 2])
        },
        "every option set renders a tail from the sub-agent's turn or the message before it"
    );
}

/// The sub-agent the shown links label, with another label in the changed links:
/// the whole conversation renders again.
#[test]
fn relabelled_sub_agent_renders_whole() {
    let shown = sub_agent_turn_between_exchanges(4, links_naming("Lorentz"));
    let changed = sub_agent_turn_between_exchanges(5, links_naming("Hubble"));

    let starts =
        assert_change_renders_as_whole_render("relabelled", &shown, &WholeRenders::of(&changed), 4);

    assert_eq!(
        (starts.tails, starts.wholes),
        (0, 12),
        "every option set renders whole: {starts:?}"
    );
}

/// `entry` with `blocks` in place of its content blocks.
fn with_blocks(entry: &LogEntry, blocks: Vec<ContentBlock>) -> LogEntry {
    let mut entry = entry.clone();
    match &mut entry {
        LogEntry::Assistant { message, .. } => message.content = blocks,
        LogEntry::User { message, .. } => message.content = UserContent::Blocks(blocks),
        _ => {}
    }
    entry
}

/// Earlier versions of `entry`, as an agent that updates an entry in place
/// writes them: without its last block, and with the first half of its
/// last text alone.
fn earlier_versions(entry: &LogEntry) -> Vec<(&'static str, LogEntry)> {
    let blocks = entry.content_blocks();
    let mut versions = Vec::new();
    if blocks.len() > 1 {
        let without_last_block = blocks[..blocks.len() - 1].to_vec();
        versions.push((
            "without its last block",
            with_blocks(entry, without_last_block),
        ));
    }
    let mut half_text = blocks.to_vec();
    if let Some(ContentBlock::Text { text }) = half_text
        .iter_mut()
        .rev()
        .find(|block| matches!(block, ContentBlock::Text { .. }))
        && text.chars().count() > 1
    {
        *text = text.chars().take(text.chars().count() / 2).collect();
        versions.push(("with half its last text", with_blocks(entry, half_text)));
    }
    versions
}

/// Each entry of each agent's every-record session, shown as an earlier
/// version of itself, then whole: an entry changed in place.
#[test]
fn entry_changed_in_place_renders_as_whole_render() {
    for (name, entries) in every_agents_stored_entries() {
        let changed = conversation_of_first(&entries, entries.len());
        let whole = WholeRenders::of(&changed);
        let mut starts = Starts::default();
        for (index, (_, entry)) in entries.iter().enumerate() {
            for (version, earlier) in earlier_versions(entry) {
                let mut shown = entries.clone();
                shown[index].1 = earlier;
                starts += assert_change_renders_as_whole_render(
                    &format!("{name}, entry {index} {version}"),
                    &conversation_of_first(&shown, shown.len()),
                    &whole,
                    index,
                );
            }
        }
        assert!(
            starts.tails > 0,
            "{name}: an entry changed in place renders a tail: {starts:?}"
        );
    }
}

/// The every-record session of Pi and of OMP, its transcript with the first
/// branch alone, then with the branch it moved to: the entries from the
/// branch point on change.
#[test]
fn branch_moved_renders_as_whole_render_for_pi_and_omp() {
    let mut branched = Vec::new();
    for session in written_every_record_sessions() {
        if !matches!(session.source, Source::Pi | Source::Omp) {
            continue;
        }
        // The session record and the first branch's four messages.
        let restore = shorten_until_restored(&session.transcript, 5);
        let (shown, changed, first_changed) = copies_around(
            session.source,
            &session.transcript,
            &session.subagents,
            restore,
        );
        assert!(
            first_changed < shown.conversation.origins.len(),
            "{:?}: the branch moves an entry the first branch shows",
            session.source
        );

        let starts = assert_change_renders_as_whole_render(
            &format!("{:?} branch", session.source),
            &shown.conversation,
            &WholeRenders::of(&changed.conversation),
            first_changed,
        );

        assert!(
            starts.tails > 0,
            "{:?}: the branch renders a tail: {starts:?}",
            session.source
        );
        branched.push(session.source);
    }
    assert_eq!(
        branched,
        [Source::Pi, Source::Omp],
        "Pi's and OMP's sessions move their branch"
    );
}

/// Each agent's every-record session shown whole, then with its last two
/// entries removed, and with its middle entry removed.
#[test]
fn truncated_or_edited_transcript_renders_as_whole_render() {
    for (name, entries) in every_agents_stored_entries() {
        let truncated = &entries[..entries.len() - 2];
        let mut edited = entries.clone();
        edited.remove(entries.len() / 2);
        let mut starts = assert_entries_change_renders_as_whole_render(
            &format!("{name} truncated"),
            &entries,
            truncated,
            truncated.len(),
        );
        starts += assert_entries_change_renders_as_whole_render(
            &format!("{name} edited"),
            &entries,
            &edited,
            entries.len() / 2,
        );
        assert!(
            starts.tails > 0,
            "{name}: an edit renders a tail: {starts:?}"
        );
    }
}

/// A transcript cut back to before its result for an earlier call, and to
/// before the message that started a day: the shown rows of the call's
/// connector and of the day label are removed.
#[test]
fn transcript_cut_back_before_result_or_day_renders_as_whole_render() {
    let day = day_one();
    let at = Some(day.as_str());
    let answered_later = numbered_entries(&conversation_of_records(&[
        text("user", at, "Start the build."),
        long_call(at, "toolu_build"),
        text("assistant", at, "The build runs in the background."),
        long_result(at, "toolu_build"),
        text("user", at, "Thanks."),
    ]));
    let next_day = numbered_entries(&conversation_of(vec![
        stamped_message(0, "user", &day_one(), "first question"),
        stamped_message(1, "assistant", &day_one_later(), "first answer"),
        stamped_message(2, "user", &day_three(), "next question"),
    ]));

    // Summary mode expands the run of the call too: 16 option sets, or 12.
    for (name, entries, kept, option_sets) in [
        ("result removed", &answered_later, 3, 16),
        ("day removed", &next_day, 2, 12),
    ] {
        let shown = conversation_of_first(entries, entries.len());
        let cut_back = conversation_of_first(entries, kept);

        let starts =
            assert_change_renders_as_whole_render(name, &shown, &WholeRenders::of(&cut_back), kept);

        assert_eq!(
            (starts.tails, starts.wholes),
            (option_sets, 0),
            "{name}: every option set renders a tail: {starts:?}"
        );
    }
}

/// The rows shown of the first three messages of four, and the tail of
/// all four from the third message, in truncated mode.
fn shown_rows_and_tail() -> (Shown, RenderedConversation, usize) {
    let at = Some("2026-09-26T12:00:00Z");
    let entries = numbered_entries(&conversation_of_records(&[
        text("user", at, "Start the build."),
        text("assistant", at, "The build runs."),
        text("user", at, "Did it pass?"),
        text("assistant", at, "It passed."),
    ]));
    let options = test_render_options(ToolDisplayMode::Truncated);
    let shown = Shown::of(render_parsed_conversation(
        &conversation_of_first(&entries, 3),
        &options,
    ));
    let row = shown.messages[2].start_line;
    let tail = render_tail_while(
        &conversation_of_first(&entries, entries.len()),
        &options,
        2,
        &|| true,
    )
    .expect("a render that keeps going finishes");
    (shown, tail, row)
}

#[test]
fn tail_put_in_place_keeps_kept_rows_where_they_are_while_capacity_allows() {
    let (mut shown, tail, row) = shown_rows_and_tail();
    shown.lines.reserve(tail.lines.len());
    let kept_rows = shown.lines.as_ptr();

    splice_tail(shown.rows(), tail, row);

    assert_eq!(
        shown.lines.as_ptr(),
        kept_rows,
        "the kept row records aren't copied while the rows have room for the tail"
    );
}

#[test]
fn tail_outgrowing_shown_rows_capacity_leaves_spare_rows() {
    let (mut shown, tail, row) = shown_rows_and_tail();
    shown.lines.shrink_to_fit();

    splice_tail(shown.rows(), tail, row);

    let spare = shown.lines.capacity() - shown.lines.len();
    assert!(
        spare >= crate::tui::viewer::rerender::SPARE_ROWS_AFTER_GROWTH,
        "the rows the tail outgrew gain spare room for later growth: {spare}"
    );
}

#[test]
fn other_options_than_shown_rows_render_whole() {
    let conversation = interleaved_calls_across_a_day();
    let shown_options = test_render_options(ToolDisplayMode::Truncated);
    let shown = render_parsed_conversation(&conversation, &shown_options);
    let first_changed = conversation.entries.len() - 1;
    let start = |options: &RenderOptions| {
        tail_render_start(
            ShownCopy {
                conversation: &conversation,
                messages: &shown.messages,
                options: &shown_options,
            }
            .changed_from(first_changed),
            &conversation,
            first_changed,
            options,
        )
    };
    assert!(
        matches!(start(&shown_options), RowsToRender::Tail(_)),
        "the shown rows' options render a tail"
    );

    let shown_rows = || test_render_options(ToolDisplayMode::Truncated);
    let other_options = [
        (
            "display mode",
            RenderOptions {
                tool_display: ToolDisplayMode::Full,
                ..shown_rows()
            },
        ),
        (
            "thinking",
            RenderOptions {
                show_thinking: !shown_options.show_thinking,
                ..shown_rows()
            },
        ),
        (
            "timing",
            RenderOptions {
                show_timing: !shown_options.show_timing,
                ..shown_rows()
            },
        ),
        (
            "width",
            RenderOptions {
                content_width: 40,
                ..shown_rows()
            },
        ),
        (
            "expanded set",
            RenderOptions {
                expanded_tool_outputs: BTreeSet::from([make_tool_output_id(
                    1,
                    None,
                    0,
                    ToolOutputKind::ToolCall,
                    Some("toolu_a"),
                )]),
                ..shown_rows()
            },
        ),
        (
            "expandability",
            RenderOptions {
                can_expand: !shown_options.can_expand,
                ..shown_rows()
            },
        ),
        (
            "date",
            RenderOptions {
                today: today().succ_opt().unwrap(),
                ..shown_rows()
            },
        ),
        (
            "time zone",
            RenderOptions {
                zone: Zone::UsEastern,
                ..shown_rows()
            },
        ),
    ];
    for (field, options) in &other_options {
        assert_eq!(
            start(options),
            RowsToRender::Whole,
            "another {field} than the shown rows' renders whole"
        );
    }
}
