//! A message rendered again in place after expanding or collapsing one of
//! its outputs, against a whole render with the same options.

use super::*;
use crate::tui::viewer::output::RowContinuation;
use crate::tui::viewer::rerender::ReplacedRows;
use crate::tui::viewer::text_blocks::TextBlockRow;
use std::ops::Range;

/// The fields a test compares of one row: its spans with their styles, its
/// output id, whether a click toggles it, how it continues the row above,
/// and the text block it shows.
type RowFields = (
    Vec<(String, LineStyle)>,
    Option<ToolOutputId>,
    bool,
    RowContinuation,
    usize,
    Option<TextBlockRow>,
);

/// The fields a test compares of one call of an expanded run: its input's
/// and its result's id, block and rows, and its lane.
type CallFields = (AreaFields, Option<AreaFields>, Option<usize>);

/// The fields a test compares of a call's input or result.
type AreaFields = (ToolOutputId, BlockLocation, Range<usize>);

fn area_fields(area: &CallArea) -> AreaFields {
    (area.id.clone(), area.location, area.rows())
}

/// The rows, messages and calls of a render, as a test compares them.
#[derive(Debug, PartialEq)]
pub(super) struct RenderShape {
    rows: Vec<RowFields>,
    messages: Vec<(usize, usize, usize)>,
    calls: Vec<CallFields>,
    detail_calls: Vec<DetailCall>,
}

/// A render's rows and indexes, owned the way the viewer keeps them.
#[derive(Clone)]
pub(super) struct Shown {
    pub(super) lines: Vec<RenderedLine>,
    pub(super) messages: Vec<MessageRange>,
    pub(super) calls: Vec<CallRange>,
    pub(super) detail_calls: Vec<DetailCall>,
}

impl Shown {
    pub(super) fn of(rendered: RenderedConversation) -> Self {
        Self {
            lines: rendered.lines,
            messages: rendered.messages,
            calls: rendered.calls,
            detail_calls: rendered.detail_calls,
        }
    }

    pub(super) fn rows(&mut self) -> ShownRows<'_> {
        ShownRows {
            lines: &mut self.lines,
            messages: &mut self.messages,
            calls: &mut self.calls,
            detail_calls: &mut self.detail_calls,
        }
    }

    pub(super) fn shape(&self) -> RenderShape {
        RenderShape {
            rows: self
                .lines
                .iter()
                .map(|line| {
                    (
                        line.spans.clone(),
                        line.tool_output_id.clone(),
                        line.clickable,
                        line.continuation,
                        line.continued_text_len,
                        line.text_block_row.clone(),
                    )
                })
                .collect(),
            messages: self
                .messages
                .iter()
                .map(|message| (message.entry_index, message.start_line, message.end_line))
                .collect(),
            calls: self
                .calls
                .iter()
                .map(|call| {
                    (
                        area_fields(&call.input),
                        call.result.as_ref().map(area_fields),
                        call.lane,
                    )
                })
                .collect(),
            detail_calls: self.detail_calls.clone(),
        }
    }
}

/// The options one case renders with.
#[derive(Clone, Copy, Debug)]
pub(super) struct Modes {
    tool_display: ToolDisplayMode,
    show_timing: bool,
    show_thinking: bool,
}

impl Modes {
    pub(super) fn every() -> Vec<Self> {
        let mut every = Vec::new();
        for tool_display in [
            ToolDisplayMode::Hidden,
            ToolDisplayMode::Truncated,
            ToolDisplayMode::Full,
        ] {
            for show_timing in [false, true] {
                for show_thinking in [false, true] {
                    every.push(Self {
                        tool_display,
                        show_timing,
                        show_thinking,
                    });
                }
            }
        }
        every
    }

    pub(super) fn options(self, expanded_tool_outputs: BTreeSet<ToolOutputId>) -> RenderOptions {
        RenderOptions {
            tool_display: self.tool_display,
            show_thinking: self.show_thinking,
            show_timing: self.show_timing,
            content_width: 60,
            expanded_tool_outputs,
            can_expand: true,
            today: today(),
            zone: Zone::System,
        }
    }
}

/// A query matching rows in most entries, above, inside and below the one
/// rendered again.
const QUERY: &str = "e";

/// Every output a click toggles in `lines`, in row order, each once.
fn clickable_ids(lines: &[RenderedLine]) -> Vec<ToolOutputId> {
    let mut ids: Vec<ToolOutputId> = Vec::new();
    for id in lines
        .iter()
        .filter(|line| line.clickable)
        .filter_map(|line| line.tool_output_id.clone())
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// The rows `outcome` replaced; panics, reporting `case`, when it didn't
/// replace any.
fn replaced_rows(outcome: MessageInPlace, case: &str) -> ReplacedRows {
    match outcome {
        MessageInPlace::Replaced(replaced) => replaced,
        outcome => panic!("{case}: the message renders again in place: {outcome:?}"),
    }
}

/// Toggles every output a click toggles in `conversation`, one at a time,
/// in every mode, from two first renders: one with nothing expanded, and in
/// summary mode one with every tool run expanded, whose outputs inside the
/// runs then toggle too, and whose runs toggle above other expanded runs.
/// Each output's message renders again in place, then again with the output
/// toggled back. Each result equals a whole render with the same options.
/// Returns how many outputs toggled.
fn assert_outputs_render_in_place_as_whole_render(
    name: &str,
    conversation: &ParsedConversation,
) -> usize {
    let mut toggled = 0;
    for modes in Modes::every() {
        for first_expanded in first_expanded_sets(conversation, modes) {
            let first = modes.options(first_expanded.clone());
            let whole = Shown::of(render_parsed_conversation(conversation, &first));
            for id in clickable_ids(&whole.lines) {
                let case = format!("{name} {modes:?}, {} expanded", first_expanded.len());
                let entry = message_toggling(&case, conversation, &first, &whole, &id);
                toggled += 1;
                let mut toggled_set = first_expanded.clone();
                if !toggled_set.remove(&id) {
                    toggled_set.insert(id.clone());
                }
                let after = modes.options(toggled_set);
                let mut shown = whole.clone();
                let mut matches = find_matches(&shown.lines, QUERY);

                let replaced = replaced_rows(
                    render_message_in_place(conversation, &after, shown.rows(), entry),
                    &format!("{case}: toggling {id:?}"),
                );
                let expected = Shown::of(render_parsed_conversation(conversation, &after));
                assert_eq!(
                    shown.shape(),
                    expected.shape(),
                    "{case}: toggling {id:?} in place renders as a whole render"
                );
                update_matches_in_replaced_rows(&mut matches, &shown.lines, QUERY, &replaced);
                assert_eq!(
                    matches,
                    find_matches(&shown.lines, QUERY),
                    "{case}: the matches updated after toggling {id:?} are a search's"
                );

                let replaced = replaced_rows(
                    render_message_in_place(conversation, &first, shown.rows(), entry),
                    &format!("{case}: toggling {id:?} back"),
                );
                assert_eq!(
                    shown.shape(),
                    whole.shape(),
                    "{case}: toggling {id:?} back renders as the first render"
                );
                update_matches_in_replaced_rows(&mut matches, &shown.lines, QUERY, &replaced);
                assert_eq!(
                    matches,
                    find_matches(&shown.lines, QUERY),
                    "{case}: the matches updated after toggling {id:?} back are a search's"
                );
            }
        }
    }
    toggled
}

/// Nothing expanded; in summary mode, also every tool run expanded.
pub(super) fn first_expanded_sets(
    conversation: &ParsedConversation,
    modes: Modes,
) -> Vec<BTreeSet<ToolOutputId>> {
    let collapsed = render_parsed_conversation(conversation, &modes.options(BTreeSet::new()));
    let runs: BTreeSet<ToolOutputId> = clickable_ids(&collapsed.lines)
        .into_iter()
        .filter(|id| id.0.ends_with(":kind:summary"))
        .collect();
    if runs.is_empty() {
        vec![BTreeSet::new()]
    } else {
        vec![BTreeSet::new(), runs]
    }
}

/// The entry of the message that toggling `id` renders again, as the app
/// finds it: the id's entry when it renders alone, or in summary mode the
/// first entry of the tool run whose rows in `shown` show the id. Fails
/// when `id` neither renders alone nor shows in a run.
fn message_toggling(
    case: &str,
    conversation: &ParsedConversation,
    options: &RenderOptions,
    shown: &Shown,
    id: &ToolOutputId,
) -> usize {
    match entry_rendering_alone(conversation, options, id) {
        Some(entry) => {
            assert!(
                id.toggles_alone_in(options),
                "{case}: {id:?}, which a click toggles, toggles alone"
            );
            entry
        }
        None => {
            assert!(
                options.tool_display.is_summary(),
                "{case}: {id:?}, which a click toggles, renders alone outside summary mode"
            );
            message_showing(&shown.lines, &shown.messages, id)
                .unwrap_or_else(|| panic!("{case}: a tool run's rows show {id:?}"))
        }
    }
}

/// `entries` with their lanes and empty links.
pub(super) fn conversation_of(entries: Vec<RenderableEntry>) -> ParsedConversation {
    let links = ConversationLinks::default();
    let lanes = Lanes::of(top_level_tool_blocks(&entries), &links);
    ParsedConversation {
        entries,
        origins: Vec::new(),
        subagent_identities: Vec::new(),
        links,
        lanes,
        activity_span: None,
    }
}

fn numbered_lines(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|line| format!("{prefix} {line}"))
        .collect::<Vec<_>>()
        .join("\\n")
}

/// Two interleaved calls with long bodies and results, and a third call
/// alone. The results arrive two days after the calls, so the connectors
/// cross a day label while timing is shown.
pub(super) fn interleaved_calls_across_a_day() -> ParsedConversation {
    let day_one = day_one();
    let day_three = day_three();
    let old = numbered_lines("old", 6);
    let new = numbered_lines("new", 6);
    let records = [
        format!(
            r#"{{"type":"user","timestamp":"{day_one}","message":{{"role":"user","content":"Edit a.rs and list the files."}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","timestamp":"{day_one}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_a","name":"Edit","input":{{"file_path":"a.rs","old_string":"{old}","new_string":"{new}"}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","timestamp":"{day_one}","message":{{"role":"assistant","content":[{{"type":"text","text":"And the listing."}},{{"type":"tool_use","id":"toolu_b","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"user","timestamp":"{day_three}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_a","content":"{}"}}]}}}}"#,
            numbered_lines("edited", 8)
        ),
        format!(
            r#"{{"type":"user","timestamp":"{day_three}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_b","content":"{}"}}]}}}}"#,
            numbered_lines("file", 9)
        ),
        format!(
            r#"{{"type":"assistant","timestamp":"{day_three}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_c","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"user","timestamp":"{day_three}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_c","content":"{}"}}]}}}}"#,
            numbered_lines("test", 7)
        ),
        format!(
            r#"{{"type":"assistant","timestamp":"{day_three}","message":{{"role":"assistant","content":[{{"type":"text","text":"Done."}}]}}}}"#
        ),
    ];
    conversation_of(
        records
            .iter()
            .enumerate()
            .map(|(entry_index, record)| RenderableEntry {
                entry_index,
                entry: claude_entry(record),
            })
            .collect(),
    )
}

#[test]
fn interleaved_calls_render_in_place_as_in_whole_render() {
    let toggled = assert_outputs_render_in_place_as_whole_render(
        "interleaved",
        &interleaved_calls_across_a_day(),
    );
    assert!(
        toggled >= 10,
        "the fixture toggles outputs in each mode: {toggled}"
    );
}

/// The modes in truncated mode, where tool calls and results toggle.
fn truncated_modes() -> impl Iterator<Item = Modes> {
    Modes::every()
        .into_iter()
        .filter(|modes| modes.tool_display == ToolDisplayMode::Truncated)
}

#[test]
fn call_and_its_result_in_two_entries_render_in_one_pass_as_whole_render() {
    let conversation = interleaved_calls_across_a_day();
    for modes in truncated_modes() {
        let collapsed = modes.options(BTreeSet::new());
        let mut shown = Shown::of(render_parsed_conversation(&conversation, &collapsed));
        // The first call's input in entry 1, and its result in entry 3.
        let ids: BTreeSet<ToolOutputId> = clickable_ids(&shown.lines)
            .into_iter()
            .filter(|id| matches!(id.entry_index(), Some(1 | 3)))
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "{modes:?}: the call's input and result toggle"
        );
        let expanded = modes.options(ids);

        for entry in [1, 3] {
            replaced_rows(
                render_message_in_place(&conversation, &expanded, shown.rows(), entry),
                &format!("{modes:?}: entry {entry}"),
            );
        }

        let expected = Shown::of(render_parsed_conversation(&conversation, &expanded));
        assert_eq!(
            shown.shape(),
            expected.shape(),
            "{modes:?}: both entries rendered in place render as a whole render"
        );
    }
}

/// A message, then an assistant entry of two calls whose inputs are
/// truncated.
fn two_truncated_calls_in_one_entry() -> ParsedConversation {
    let call = |id: &str, prefix: &str| {
        format!(
            r#"{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"{}"}}}}"#,
            numbered_lines(prefix, 6)
        )
    };
    let records = [
        r#"{"type":"user","message":{"role":"user","content":"Run both."}}"#.to_owned(),
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{},{}]}}}}"#,
            call("toolu_a", "first"),
            call("toolu_b", "second")
        ),
    ];
    conversation_of(
        records
            .iter()
            .enumerate()
            .map(|(entry_index, record)| RenderableEntry {
                entry_index,
                entry: claude_entry(record),
            })
            .collect(),
    )
}

#[test]
fn second_output_of_entry_expands_in_place_beside_first_as_whole_render() {
    let conversation = two_truncated_calls_in_one_entry();
    for modes in truncated_modes() {
        let ids = clickable_ids(
            &render_parsed_conversation(&conversation, &modes.options(BTreeSet::new())).lines,
        );
        assert_eq!(ids.len(), 2, "{modes:?}: both calls toggle");
        let first_expanded = modes.options(BTreeSet::from([ids[0].clone()]));
        let mut shown = Shown::of(render_parsed_conversation(&conversation, &first_expanded));
        let both_expanded = modes.options(ids.iter().cloned().collect());

        replaced_rows(
            render_message_in_place(&conversation, &both_expanded, shown.rows(), 1),
            &format!("{modes:?}: expanding the second call"),
        );

        let expected = Shown::of(render_parsed_conversation(&conversation, &both_expanded));
        assert_eq!(
            shown.shape(),
            expected.shape(),
            "{modes:?}: the second call expanded beside the first renders as a whole render"
        );
    }
}

/// `count` words, `<prefix>01` onwards, long enough to wrap.
fn numbered_words(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|word| format!("{prefix}{word:02}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A message, an assistant entry of text and a call whose input is
/// truncated, and a message. Each entry's text wraps across rows: its words
/// start with `before`, `inside` and `after`.
fn wrapped_text_around_call() -> ParsedConversation {
    let records = [
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
            numbered_words("before", 40)
        ),
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}},{{"type":"tool_use","id":"toolu_a","name":"Bash","input":{{"command":"{}"}}}}]}}}}"#,
            numbered_words("inside", 40),
            numbered_lines("line", 6)
        ),
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
            numbered_words("after", 40)
        ),
    ];
    conversation_of(
        records
            .iter()
            .enumerate()
            .map(|(entry_index, record)| RenderableEntry {
                entry_index,
                entry: claude_entry(record),
            })
            .collect(),
    )
}

/// For each of `prefixes`, a phrase of two words of `lines` that a row
/// break at a space splits: the last word of one row and the first word
/// continuing it on the next.
fn phrases_across_rows(lines: &[RenderedLine], prefixes: &[&str]) -> Vec<String> {
    prefixes
        .iter()
        .map(|prefix| {
            lines
                .windows(2)
                .filter(|pair| pair[1].continuation != RowContinuation::None)
                .find_map(|pair| {
                    let above = pair[0].text();
                    let continued = pair[1].text();
                    let breaks_at_space =
                        above.ends_with(' ') || pair[1].continuation == RowContinuation::AfterSpace;
                    if !breaks_at_space {
                        return None;
                    }
                    let last = above.split_whitespace().last()?;
                    let first = continued[continued.len() - pair[1].continued_text_len..]
                        .split_whitespace()
                        .next()?;
                    (last.starts_with(prefix) && first.starts_with(prefix))
                        .then(|| format!("{last} {first}"))
                })
                .unwrap_or_else(|| panic!("a `{prefix}` phrase wraps across rows"))
        })
        .collect()
}

#[test]
fn matches_of_phrases_wrapped_in_and_around_entry_update_as_search() {
    let conversation = wrapped_text_around_call();
    let modes = Modes {
        tool_display: ToolDisplayMode::Truncated,
        show_timing: false,
        show_thinking: false,
    };
    let collapsed = modes.options(BTreeSet::new());
    let whole = Shown::of(render_parsed_conversation(&conversation, &collapsed));
    let call = clickable_ids(&whole.lines)
        .pop()
        .expect("the call's input toggles");
    let expanded = modes.options(BTreeSet::from([call]));

    for phrase in phrases_across_rows(&whole.lines, &["before", "inside", "after"]) {
        let mut shown = whole.clone();
        let mut matches = find_matches(&shown.lines, &phrase);
        assert!(
            matches.iter().any(|found| found.rows().len() == 2),
            "{phrase:?} covers two rows"
        );
        for (options, toggle) in [(&expanded, "expanding"), (&collapsed, "collapsing")] {
            let replaced = replaced_rows(
                render_message_in_place(&conversation, options, shown.rows(), 1),
                &format!("{toggle} the call"),
            );
            update_matches_in_replaced_rows(&mut matches, &shown.lines, &phrase, &replaced);
            assert_eq!(
                matches,
                find_matches(&shown.lines, &phrase),
                "the matches of {phrase:?} updated after {toggle} the call are a search's"
            );
        }
    }
}

#[test]
fn two_expands_after_rows_grow_resize_them_once() {
    let conversation = interleaved_calls_across_a_day();
    let modes = Modes {
        tool_display: ToolDisplayMode::Truncated,
        show_timing: false,
        show_thinking: false,
    };
    let mut expanded = BTreeSet::new();
    let mut shown = Shown::of(render_parsed_conversation(
        &conversation,
        &modes.options(expanded.clone()),
    ));
    shown.lines.shrink_to_fit();
    let ids: Vec<ToolOutputId> = clickable_ids(&shown.lines).into_iter().take(2).collect();
    let mut capacities = vec![shown.lines.capacity()];

    for id in ids {
        let entry = entry_rendering_alone(&conversation, &modes.options(expanded.clone()), &id)
            .expect("the output renders alone");
        expanded.insert(id.clone());
        let replaced = replaced_rows(
            render_message_in_place(
                &conversation,
                &modes.options(expanded.clone()),
                shown.rows(),
                entry,
            ),
            &format!("expanding {id:?}"),
        );
        assert!(replaced.new_len > replaced.old.len(), "{id:?} adds rows");
        capacities.push(shown.lines.capacity());
    }

    capacities.dedup();
    assert_eq!(
        capacities.len(),
        2,
        "the first expand grows the rows with spare room, and the second fits in it"
    );
}

#[test]
fn sub_agents_reply_and_output_render_in_place_as_in_whole_render() {
    let reply = numbered_lines("finding", 8);
    let entries = codex_session_with_sub_agent(
        &[],
        &[
            codex_shell_command(3, "call_1", "cargo test"),
            codex_reply(4, &reply),
        ],
    );
    let links = links_of(&entries);
    let lanes = Lanes::of(top_level_tool_blocks(&entries), &links);
    let conversation = ParsedConversation {
        entries,
        origins: Vec::new(),
        subagent_identities: Vec::new(),
        links,
        lanes,
        activity_span: None,
    };

    let toggled = assert_outputs_render_in_place_as_whole_render("sub-agent", &conversation);
    assert!(toggled > 0, "the sub-agent's reply toggles");
}

#[test]
fn task_report_renders_in_place_as_in_whole_render() {
    use crate::history::format::claude::test_support::IDE_DIAGNOSTICS;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(&path, IDE_DIAGNOSTICS).unwrap();

    let toggled =
        assert_outputs_render_in_place_as_whole_render("task report", &read_bare_file(&path));
    assert!(toggled > 0, "the report toggles");
}

#[test]
fn every_agents_outputs_render_in_place_as_in_whole_render() {
    use crate::history::provider::contract_tests::every_record_sessions;
    let toggled: Vec<(String, usize)> = every_record_sessions()
        .iter()
        .map(|session| {
            let row = &session.row;
            let conversation = read_as_viewer(row.source, &row.path, &row.subagents);
            let name = format!("{:?}", row.source);
            let toggled = assert_outputs_render_in_place_as_whole_render(&name, &conversation);
            (name, toggled)
        })
        .collect();

    // Every agent's session has a tool run that a click toggles in summary
    // mode.
    let expected = [
        ("Claude", 56),
        ("Codex", 108),
        ("OpenCode", 44),
        ("Kimi", 64),
        ("Grok", 40),
        ("Pi", 40),
        ("Omp", 48),
    ]
    .map(|(name, count)| (name.to_owned(), count));
    assert_eq!(
        toggled, expected,
        "the outputs of each agent's session that render in place, over every mode"
    );
}

#[test]
fn outputs_toggle_alone_in_modes_that_truncate_them() {
    let call = make_tool_output_id(0, None, 0, ToolOutputKind::ToolCall, Some("toolu_1"));
    let result = make_tool_output_id(1, None, 0, ToolOutputKind::ToolResult, Some("toolu_1"));
    let report = make_tool_output_id(
        2,
        None,
        0,
        ToolOutputKind::ToolResult,
        Some(TASK_REPORT_RAW_ID),
    );
    let reply = make_reply_output_id(3, Some("toolu_agent"), 0);
    let run = make_tool_summary_output_id(4, None);
    let toggles_in = |tool_display| {
        let options = test_render_options(tool_display);
        [&call, &result, &report, &reply, &run].map(|id| id.toggles_alone_in(&options))
    };

    assert_eq!(
        toggles_in(ToolDisplayMode::Hidden),
        [false, false, true, true, false],
        "summary mode: a report and a reply"
    );
    assert_eq!(
        toggles_in(ToolDisplayMode::Truncated),
        [true, true, true, true, false],
        "truncated mode: every output but a run"
    );
    assert_eq!(
        toggles_in(ToolDisplayMode::Full),
        [false; 5],
        "`tools·all`: nothing"
    );
}

/// `prefix 1` to `prefix count`, one a line.
fn lines_of(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|line| format!("{prefix} {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A Claude record of `role` with `content`, stamped `timestamp`, or
/// unstamped for `None`.
fn record(role: &str, timestamp: Option<&str>, content: serde_json::Value) -> serde_json::Value {
    let mut record = serde_json::json!({
        "type": role,
        "message": {"role": role, "content": content}
    });
    if let Some(timestamp) = timestamp {
        record["timestamp"] = timestamp.into();
    }
    record
}

pub(super) fn text(role: &str, timestamp: Option<&str>, text: &str) -> serde_json::Value {
    record(
        role,
        timestamp,
        serde_json::json!([{"type": "text", "text": text}]),
    )
}

/// A call with an input long enough to be truncated.
pub(super) fn long_call(timestamp: Option<&str>, id: &str) -> serde_json::Value {
    record(
        "assistant",
        timestamp,
        serde_json::json!([{
            "type": "tool_use", "id": id, "name": "Bash",
            "input": {"command": lines_of(&format!("echo {id}"), 6)}
        }]),
    )
}

/// The result of the call `id`, long enough to be truncated.
pub(super) fn long_result(timestamp: Option<&str>, id: &str) -> serde_json::Value {
    record(
        "user",
        timestamp,
        serde_json::json!([{
            "type": "tool_result", "tool_use_id": id,
            "content": lines_of(&format!("{id} output"), 8)
        }]),
    )
}

/// An entry that doesn't render any rows while thinking is hidden.
fn thinking() -> serde_json::Value {
    record(
        "assistant",
        None,
        serde_json::json!([{"type": "thinking", "thinking": "next step", "signature": ""}]),
    )
}

/// A usage entry, which doesn't render any rows.
fn usage() -> serde_json::Value {
    serde_json::json!({
        "type": "pi-metadata", "label": "Usage", "text": "", "searchable": false,
        "usage": {"input_tokens": 12, "output_tokens": 3}
    })
}

/// `record` written by the sub-agent the call `parent` started.
fn of_sub_agent(mut record: serde_json::Value, parent: &str) -> serde_json::Value {
    record["parent_tool_use_id"] = parent.into();
    record
}

pub(super) fn conversation_of_records(records: &[serde_json::Value]) -> ParsedConversation {
    conversation_of(
        records
            .iter()
            .enumerate()
            .map(|(entry_index, record)| RenderableEntry {
                entry_index,
                entry: claude_entry(&record.to_string()),
            })
            .collect(),
    )
}

/// Two runs of calls between messages.
pub(super) fn tool_runs_between_messages() -> ParsedConversation {
    let day = day_one();
    let at = Some(day.as_str());
    conversation_of_records(&[
        text("user", at, "Run the tests."),
        long_call(at, "toolu_a"),
        long_result(at, "toolu_a"),
        long_call(at, "toolu_b"),
        long_result(at, "toolu_b"),
        text("assistant", at, "Both passed."),
        long_call(at, "toolu_c"),
        long_result(at, "toolu_c"),
        text("user", at, "Thanks."),
    ])
}

#[test]
fn tool_runs_and_outputs_inside_them_render_in_place_as_in_whole_render() {
    let conversation = tool_runs_between_messages();

    let toggled = assert_outputs_render_in_place_as_whole_render("runs", &conversation);
    // In each of summary mode's four option sets: two runs, then two runs and
    // their six outputs. In each of truncated mode's four: the six outputs.
    assert_eq!(
        toggled,
        4 * (2 + 2 + 6) + 4 * 6,
        "the runs and their outputs toggle in summary mode, and the outputs in truncated mode"
    );
}

/// A top-level call, then two sub-agents' runs. With thinking shown, each
/// sub-agent's call opens a run of its own, which ends the run before it;
/// that run's result arrives later.
pub(super) fn run_with_results_in_later_entries() -> ParsedConversation {
    conversation_of_records(&[
        text("user", None, "Run both agents."),
        long_call(None, "toolu_top"),
        of_sub_agent(long_call(None, "toolu_a"), "toolu_agent_a"),
        of_sub_agent(long_call(None, "toolu_b"), "toolu_agent_b"),
        of_sub_agent(long_result(None, "toolu_a"), "toolu_agent_a"),
        long_result(None, "toolu_top"),
        of_sub_agent(long_result(None, "toolu_b"), "toolu_agent_b"),
        text("assistant", None, "Both agents finished."),
    ])
}

#[test]
fn run_with_results_in_later_entries_renders_in_place_as_in_whole_render() {
    let conversation = run_with_results_in_later_entries();
    let mut options = Modes {
        tool_display: ToolDisplayMode::Hidden,
        show_timing: false,
        show_thinking: true,
    }
    .options(BTreeSet::new());
    let collapsed = render_parsed_conversation(&conversation, &options);
    options.expanded_tool_outputs = clickable_ids(&collapsed.lines).into_iter().collect();
    let expanded = render_parsed_conversation(&conversation, &options);
    let late_result =
        make_tool_output_id(5, None, 0, ToolOutputKind::ToolResult, Some("toolu_top"));
    assert_eq!(
        message_showing(&expanded.lines, &expanded.messages, &late_result),
        Some(1),
        "the top-level run shows its result, which arrives after the sub-agents' runs"
    );

    let toggled = assert_outputs_render_in_place_as_whole_render("late results", &conversation);
    // Summary mode, in each of two option sets with thinking hidden: the
    // top-level run, then the run and its call and result; with thinking
    // shown: three runs, then three runs and their six outputs. Truncated
    // mode: the two top-level outputs, or with thinking shown all six.
    assert_eq!(
        toggled,
        2 * (1 + 3) + 2 * (3 + 9) + 2 * 2 + 2 * 6,
        "the runs and their outputs toggle in summary mode, and the outputs in truncated mode"
    );
}

/// Runs of calls whose entries fall on two days: crossing midnight, starting
/// on a new day, and opened by an unstamped entry. The last starts on the
/// day of the message before it, and ends at the call on a later day.
///
/// Each comes with the count of outputs
/// [`assert_outputs_render_in_place_as_whole_render`] toggles. Summary mode,
/// in each of two option sets with timing hidden and two with it shown,
/// toggles the runs, then the runs and their outputs; the run of two calls
/// splits at the day label while timing is shown. Truncated mode, in each of
/// four, toggles the outputs.
pub(super) fn runs_around_midnight() -> [(&'static str, ParsedConversation, usize); 3] {
    let (day_one, day_three) = (day_one(), day_three());
    let (one, three) = (Some(day_one.as_str()), Some(day_three.as_str()));
    let two_calls_split_by_day = 2 * (1 + 5) + 2 * (2 + 6) + 4 * 4;
    [
        (
            "crossing midnight",
            conversation_of_records(&[
                text("user", one, "Start."),
                long_call(one, "toolu_a"),
                long_result(three, "toolu_a"),
                long_call(three, "toolu_b"),
                long_result(three, "toolu_b"),
                text("assistant", three, "Done."),
            ]),
            two_calls_split_by_day,
        ),
        (
            "starting on a new day",
            conversation_of_records(&[
                text("user", one, "Start."),
                long_call(three, "toolu_a"),
                long_result(three, "toolu_a"),
                text("assistant", three, "Done."),
            ]),
            4 * (1 + 3) + 4 * 2,
        ),
        (
            "opened by an unstamped entry",
            conversation_of_records(&[
                text("user", one, "Start."),
                long_call(None, "toolu_a"),
                long_result(None, "toolu_a"),
                long_call(three, "toolu_b"),
                long_result(three, "toolu_b"),
                text("assistant", three, "Done."),
            ]),
            two_calls_split_by_day,
        ),
    ]
}

#[test]
fn runs_around_midnight_render_in_place_as_in_whole_render() {
    for (name, conversation, expected) in runs_around_midnight() {
        let toggled = assert_outputs_render_in_place_as_whole_render(name, &conversation);
        assert_eq!(
            toggled, expected,
            "{name}: the runs and their outputs toggle in summary mode, and the outputs in truncated mode"
        );
    }
}

/// Runs ended or followed by entries that don't render any rows while
/// thinking is hidden.
pub(super) fn runs_followed_by_entries_rendering_nothing() -> ParsedConversation {
    conversation_of_records(&[
        text("user", None, "Start."),
        long_call(None, "toolu_a"),
        long_result(None, "toolu_a"),
        thinking(),
        usage(),
        long_call(None, "toolu_b"),
        long_result(None, "toolu_b"),
        thinking(),
        usage(),
        text("assistant", None, "Done."),
        long_call(None, "toolu_c"),
        long_result(None, "toolu_c"),
        usage(),
        thinking(),
    ])
}

#[test]
fn runs_followed_by_entries_rendering_nothing_render_in_place_as_in_whole_render() {
    let conversation = runs_followed_by_entries_rendering_nothing();

    let toggled =
        assert_outputs_render_in_place_as_whole_render("entries rendering nothing", &conversation);
    // Summary mode, in each of two option sets with thinking hidden: two
    // runs, then two runs and their six outputs; with thinking shown, each
    // thinking entry ends a run: three runs, then three runs and their six
    // outputs. Truncated mode, in each of four: the six outputs.
    assert_eq!(
        toggled,
        2 * (2 + 8) + 2 * (3 + 9) + 4 * 6,
        "the runs and their outputs toggle in summary mode, and the outputs in truncated mode"
    );
}
