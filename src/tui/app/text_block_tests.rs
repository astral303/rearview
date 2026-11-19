//! Message navigation inside a message's text: `→` focuses its blocks and a
//! list's items, `J`/`K` step through them, `←` steps out, and `y` copies
//! what the focus marks.

use super::interaction_tests::{
    app_with_tool_conversation, copied_by, press, record_copied_text,
    viewer_showing_subagent_turns, write_subagent_reply,
};
use super::test_waits::finish_view_render;
use super::*;

/// A reply with a heading, a paragraph, a code block, a quote holding a code
/// block, a list whose second item holds a code block, and a table.
const REPLY: &str = "## Findings\n\nMost of it is one chunk.\n\n```rust\nfn chunks() {}\n```\n\n> A session of short texts\n> fits in one chunk:\n>\n> ```\n> 885193 texts\n> ```\n\n1. Split by bytes.\n2. Cap the texts:\n\n   ```sh\n   cargo test\n   ```\n3. Sort the sessions.\n\n| Fix | Speedup |\n|---|---|\n| bytes | 1.2x |\n";

const REPLY_MESSAGE: usize = 1;

/// The viewer on `intro`, `reply` and `outro`, the reply focused.
fn app_on_message(dir: &tempfile::TempDir, reply: &str) -> App {
    let path = dir.path().join("reply.jsonl");
    let lines = [
        serde_json::json!({"type": "user", "message": {"role": "user", "content": "intro"}}),
        serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": reply}]}}),
        serde_json::json!({"type": "user", "message": {"role": "user", "content": "outro"}}),
    ];
    std::fs::write(&path, lines.map(|line| line.to_string() + "\n").concat()).unwrap();
    let mut app = app_with_tool_conversation(path, ToolDisplayMode::Hidden);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE)));
    app
}

/// The viewer on `intro`, [`REPLY`] and `outro`, the reply focused.
fn app_on_reply(dir: &tempfile::TempDir) -> App {
    app_on_message(dir, REPLY)
}

/// The status line's text.
fn status(app: &App) -> String {
    app.status_message
        .as_ref()
        .map(|(text, _)| text.clone())
        .unwrap_or_default()
}

fn view(app: &App) -> &ViewState {
    match app.app_mode() {
        AppMode::View(state) => state,
        AppMode::List => panic!("the viewer is closed"),
    }
}

fn focus(app: &App) -> Option<Focus> {
    view(app).focus
}

fn text_focus(app: &App) -> Option<TextBlockFocus> {
    view(app).focused_text_block()
}

fn on_block(block: usize, item: Option<usize>) -> Option<TextBlockFocus> {
    Some(TextBlockFocus { block, item })
}

/// The content of each row the `▌` marks: the focused block's or item's,
/// or the focused message's.
fn marked_rows(app: &App) -> Vec<String> {
    let state = view(app);
    let rows = match state.focused_text() {
        Some(text) => text.rows,
        None => state.message_ranges[state.focused_message().unwrap()].rows(),
    };
    state.rendered_lines[rows]
        .iter()
        .map(|line| {
            let text = line.text();
            text.split_once(" │ ")
                .map_or(text.as_str(), |(_, content)| content)
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn right_then_j_steps_through_blocks_and_list_items_then_leaves_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);

    press(&mut app, KeyCode::Right);
    let mut seen = vec![(text_focus(&app), marked_rows(&app))];
    for code in [
        KeyCode::Char('J'),
        KeyCode::Char('J'),
        KeyCode::Char('J'),
        KeyCode::Char('J'),
        KeyCode::Right,
        KeyCode::Char('J'),
        KeyCode::Char(']'),
        KeyCode::Char('J'),
    ] {
        press(&mut app, code);
        seen.push((text_focus(&app), marked_rows(&app)));
    }

    let firsts: Vec<(Option<TextBlockFocus>, &str)> = seen
        .iter()
        .map(|(focus, rows)| (*focus, rows[0].as_str()))
        .collect();
    assert_eq!(
        firsts,
        [
            (on_block(0, None), "Findings"),
            (on_block(1, None), "Most of it is one chunk."),
            (on_block(2, None), "fn chunks() {}"),
            (on_block(3, None), "> A session of short texts"),
            (on_block(4, None), "1. Split by bytes."),
            (on_block(4, Some(0)), "1. Split by bytes."),
            (on_block(4, Some(1)), "2. Cap the texts:"),
            (on_block(4, Some(2)), "3. Sort the sessions."),
            (on_block(5, None), "┌───────┬─────────┐"),
        ],
        "each block, then each item of the list, is marked in turn"
    );
    let rows_of = |step: usize| seen[step].1.join("\n");
    assert_eq!(rows_of(0), "Findings", "a block's rows alone are marked");
    assert!(rows_of(3).contains("885193 texts"), "{}", rows_of(3));
    assert!(
        rows_of(4).ends_with("3. Sort the sessions."),
        "{}",
        rows_of(4)
    );
    assert!(rows_of(6).contains("cargo test"), "{}", rows_of(6));

    press(&mut app, KeyCode::Char('J'));
    assert_eq!(
        focus(&app),
        Some(Focus::message(REPLY_MESSAGE + 1)),
        "`J` on the last block focuses the next message"
    );
    press(&mut app, KeyCode::Char('K'));
    assert_eq!(
        focus(&app),
        Some(Focus::message(REPLY_MESSAGE)),
        "coming back focuses the whole message"
    );
}

#[test]
fn k_steps_back_through_items_and_blocks_then_leaves_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('J'));
    }
    press(&mut app, KeyCode::Right);

    let mut seen = Vec::new();
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('K'));
        seen.push(text_focus(&app));
    }

    assert_eq!(
        seen,
        [
            on_block(3, None),
            on_block(2, None),
            on_block(1, None),
            on_block(0, None),
            None,
            None,
        ],
        "`K` on the first item focuses the block before the list"
    );
    assert_eq!(
        focus(&app),
        Some(Focus::message(REPLY_MESSAGE - 1)),
        "`K` on the first block focuses the previous message"
    );
}

#[test]
fn left_steps_out_one_level() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('J'));
    }
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));

    let mut seen = Vec::new();
    for _ in 0..3 {
        press(&mut app, KeyCode::Left);
        seen.push(focus(&app));
    }

    let list = Focus {
        text_block: on_block(4, None),
        ..Focus::message(REPLY_MESSAGE)
    };
    assert_eq!(
        seen,
        [
            Some(list),
            Some(Focus::message(REPLY_MESSAGE)),
            Some(Focus::message(REPLY_MESSAGE)),
        ],
        "item to list, list to message, and the message stays"
    );
}

#[test]
fn right_on_block_other_than_list_doesnt_move_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);

    press(&mut app, KeyCode::Right);

    assert_eq!(text_focus(&app), on_block(0, None));
}

#[test]
fn right_on_message_of_one_paragraph_doesnt_move_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Char('K'));

    press(&mut app, KeyCode::Right);

    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE - 1)));
}

#[test]
fn y_copies_block_or_item_and_names_it_without_moving_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    app.set_clipboard_writer_for_test(record_copied_text);
    let copy = |app: &mut App| {
        let before = focus(app);
        let copied = copied_by(app, KeyCode::Char('y'));
        assert_eq!(focus(app), before, "the focus stays where it is");
        let status = app.status_message.as_ref().map(|(text, _)| text.clone());
        (status.unwrap_or_default(), copied.join(""))
    };

    let (status, _) = copy(&mut app);
    assert_eq!(
        status, "Message copied to clipboard",
        "the message copies as today"
    );

    press(&mut app, KeyCode::Right);
    let mut copies = vec![copy(&mut app)];
    for _ in 0..5 {
        press(&mut app, KeyCode::Char('J'));
        copies.push(copy(&mut app));
    }
    press(&mut app, KeyCode::Char('K'));
    press(&mut app, KeyCode::Right);
    for _ in 0..3 {
        copies.push(copy(&mut app));
        press(&mut app, KeyCode::Char('J'));
    }

    let expected = [
        ("Heading", "## Findings"),
        ("Paragraph", "Most of it is one chunk."),
        ("Code block", "fn chunks() {}"),
        (
            "Quote",
            "A session of short texts\nfits in one chunk:\n\n```\n885193 texts\n```",
        ),
        (
            "List",
            "1. Split by bytes.\n2. Cap the texts:\n\n   ```sh\n   cargo test\n   ```\n3. Sort the sessions.",
        ),
        ("Table", "| Fix | Speedup |\n|---|---|\n| bytes | 1.2x |"),
        ("List item", "Split by bytes."),
        ("List item", "Cap the texts:\n\n```sh\ncargo test\n```"),
        ("List item", "Sort the sessions."),
    ]
    .map(|(name, text)| (format!("{name} copied to clipboard"), text.to_owned()));
    assert_eq!(copies, expected);
}

#[test]
fn esc_and_click_return_focus_to_message_level() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));

    press(&mut app, KeyCode::Esc);

    assert!(!view(&app).message_nav_active);
    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE)));

    press(&mut app, KeyCode::Char('K'));
    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));
    let frame = Rect::new(0, 0, 80, 20);
    let layout = crate::tui::ui::view_layout_rects(frame, &app, view(&app));
    let marked_row = view(&app).focused_text().unwrap().rows.start;
    let screen_row = layout.content.y + (marked_row - view(&app).scroll_offset) as u16;

    assert!(app.handle_view_click(screen_row, frame));

    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE)));
}

#[test]
fn enter_on_block_expands_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);

    press(&mut app, KeyCode::Enter);

    assert!(view(&app).expanded_tool_outputs.is_empty());
    assert_eq!(text_focus(&app), on_block(0, None));
}

#[test]
fn resize_keeps_focused_item_on_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('J'));
    }
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));
    let rows_before = view(&app).rendered_lines.len();

    app.check_view_resize(30);
    finish_view_render(&mut app, 17);

    assert_ne!(
        view(&app).rendered_lines.len(),
        rows_before,
        "the narrower frame wraps rows"
    );
    assert_eq!(text_focus(&app), on_block(4, Some(1)));
    let marked = marked_rows(&app);
    assert!(marked[0].starts_with("2. Cap"), "{marked:?}");
}

/// The viewer on `intro`, a reply that is one list, and `outro`, the reply
/// focused.
fn app_on_one_list(dir: &tempfile::TempDir) -> App {
    app_on_message(dir, "- alpha\n- beta\n- gamma")
}

/// `→` focuses the first item of a list that is the message's only block,
/// because the list's rows are the message's.
#[test]
fn right_on_message_of_one_list_focuses_its_first_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_one_list(&dir);
    let mut seen = vec![(focus(&app), marked_rows(&app))];
    for code in [
        KeyCode::Right,
        KeyCode::Char('J'),
        KeyCode::Char('J'),
        KeyCode::Char('J'),
        KeyCode::Char('K'),
        KeyCode::Right,
        KeyCode::Char('K'),
    ] {
        press(&mut app, code);
        seen.push((focus(&app), marked_rows(&app)));
    }

    let item = |item: usize| {
        Some(Focus {
            text_block: on_block(0, Some(item)),
            ..Focus::message(REPLY_MESSAGE)
        })
    };
    let rows = |rows: &[&str]| rows.iter().map(|row| row.to_string()).collect::<Vec<_>>();
    assert_eq!(
        seen,
        [
            (
                Some(Focus::message(REPLY_MESSAGE)),
                rows(&["- alpha", "- beta", "- gamma"])
            ),
            (item(0), rows(&["- alpha"])),
            (item(1), rows(&["- beta"])),
            (item(2), rows(&["- gamma"])),
            (Some(Focus::message(REPLY_MESSAGE + 1)), rows(&["outro"])),
            (
                Some(Focus::message(REPLY_MESSAGE)),
                rows(&["- alpha", "- beta", "- gamma"])
            ),
            (item(0), rows(&["- alpha"])),
            (Some(Focus::message(REPLY_MESSAGE - 1)), rows(&["intro"])),
        ],
        "`J` past the last item and `K` before the first step to the neighbouring message"
    );
}

#[test]
fn left_on_item_of_message_of_one_list_focuses_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_one_list(&dir);
    app.set_clipboard_writer_for_test(record_copied_text);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));

    press(&mut app, KeyCode::Left);

    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE)));
    assert_eq!(marked_rows(&app), ["- alpha", "- beta", "- gamma"]);
    copied_by(&mut app, KeyCode::Char('y'));
    assert_eq!(
        app.status_message.as_ref().map(|(text, _)| text.as_str()),
        Some("Message copied to clipboard"),
        "`y` on the message copies the message"
    );
}

/// Short enough that the reply's first blocks scroll off its top.
const SHORT_VIEWPORT: usize = 3;

#[test]
fn scrolling_block_off_screen_moves_focus_to_block_on_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);
    let paragraph = view(&app).text_blocks_of(REPLY_MESSAGE)[1].rows.start;

    while view(&app).scroll_offset < paragraph {
        app.handle_key(KeyCode::Down, KeyModifiers::empty(), SHORT_VIEWPORT);
    }

    assert_eq!(
        text_focus(&app),
        on_block(1, None),
        "the heading left the top; the paragraph, first on screen, is focused"
    );

    app.handle_key(KeyCode::Up, KeyModifiers::empty(), SHORT_VIEWPORT);

    assert_eq!(
        text_focus(&app),
        on_block(1, None),
        "a block still on screen stays focused"
    );
}

#[test]
fn scrolling_item_off_screen_moves_focus_to_item_on_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    let key = |app: &mut App, code: KeyCode| {
        app.handle_key(code, KeyModifiers::empty(), SHORT_VIEWPORT);
    };
    key(&mut app, KeyCode::Right);
    for _ in 0..4 {
        key(&mut app, KeyCode::Char('J'));
    }
    key(&mut app, KeyCode::Right);
    assert_eq!(text_focus(&app), on_block(4, Some(0)));
    let first_item = view(&app).focused_text().unwrap().rows.start;

    while view(&app).scroll_offset <= first_item {
        key(&mut app, KeyCode::Down);
    }

    assert_eq!(
        text_focus(&app),
        on_block(4, Some(1)),
        "the first item left the top; the second, first on screen, is focused"
    );
}

#[test]
fn k_steps_back_from_later_item_to_previous_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_reply(&dir);
    press(&mut app, KeyCode::Right);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('J'));
    }
    for code in [KeyCode::Right, KeyCode::Char('J'), KeyCode::Char('J')] {
        press(&mut app, code);
    }
    assert_eq!(text_focus(&app), on_block(4, Some(2)));

    let mut seen = Vec::new();
    for _ in 0..2 {
        press(&mut app, KeyCode::Char('K'));
        seen.push(text_focus(&app));
    }

    assert_eq!(seen, [on_block(4, Some(1)), on_block(4, Some(0))]);
}

#[test]
fn right_on_message_of_one_code_block_or_quote_focuses_it() {
    for (reply, name, copied) in [
        ("```sh\ncargo test\n```", "Code block", "cargo test"),
        ("> quoted\n> text", "Quote", "quoted\ntext"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_on_message(&dir, reply);
        app.set_clipboard_writer_for_test(record_copied_text);

        press(&mut app, KeyCode::Right);
        assert_eq!(text_focus(&app), on_block(0, None), "{reply:?}");
        let text = copied_by(&mut app, KeyCode::Char('y')).join("");
        assert_eq!(
            (status(&app), text),
            (format!("{name} copied to clipboard"), copied.to_owned()),
            "{reply:?} copies without its fences or `>` marks"
        );

        press(&mut app, KeyCode::Left);
        assert_eq!(
            focus(&app),
            Some(Focus::message(REPLY_MESSAGE)),
            "`←` on {reply:?} focuses the message"
        );
    }
}

#[test]
fn y_on_empty_list_item_copies_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_message(&dir, "- one\n-\n- three");
    app.set_clipboard_writer_for_test(record_copied_text);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(text_focus(&app), on_block(0, Some(1)));

    let copied = copied_by(&mut app, KeyCode::Char('y'));

    assert!(copied.is_empty(), "nothing is copied: {copied:?}");
    assert_eq!(
        status(&app),
        "No text content in this message",
        "the status line shows what a message without text shows"
    );
}

/// A paragraph that wraps onto a second row in a narrow frame, then a list:
/// six rows at 80 columns, of which a truncated reply shows four, the
/// second item last.
const SUBAGENT_REPLY: &str =
    "First paragraph, wrapped in two rows.\n\n- one\n- two\n- three\n- four";

/// The viewer on a sub-agent's reply of [`SUBAGENT_REPLY`], truncated, the
/// reply focused.
fn app_on_truncated_reply(dir: &tempfile::TempDir) -> App {
    let path = dir.path().join("subagent.jsonl");
    write_subagent_reply(&path, SUBAGENT_REPLY);
    let mut app = viewer_showing_subagent_turns(path);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE)));
    assert!(view(&app).expanded_tool_outputs.is_empty());
    app
}

#[test]
fn right_expands_message_then_focuses_its_first_block() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_truncated_reply(&dir);

    press(&mut app, KeyCode::Right);

    assert_eq!(
        view(&app).expanded_tool_outputs.len(),
        1,
        "the reply expands"
    );
    assert_eq!(focus(&app), Some(Focus::message(REPLY_MESSAGE)));

    press(&mut app, KeyCode::Right);

    assert_eq!(text_focus(&app), on_block(0, None));
}

#[test]
fn enter_on_block_of_subagent_reply_toggles_its_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_truncated_reply(&dir);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);

    press(&mut app, KeyCode::Enter);

    assert!(
        view(&app).expanded_tool_outputs.is_empty(),
        "the reply is truncated again"
    );
    assert_eq!(
        text_focus(&app),
        on_block(0, None),
        "the block stays focused"
    );

    press(&mut app, KeyCode::Enter);

    assert_eq!(
        view(&app).expanded_tool_outputs.len(),
        1,
        "the truncated reply expands"
    );
    assert_eq!(
        text_focus(&app),
        on_block(0, None),
        "the block stays focused"
    );
}

#[test]
fn resize_that_truncates_focused_item_away_focuses_its_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_on_truncated_reply(&dir);
    for code in [
        KeyCode::Right,
        KeyCode::Right,
        KeyCode::Char('J'),
        KeyCode::Right,
        KeyCode::Char('J'),
        KeyCode::Enter,
    ] {
        press(&mut app, code);
    }
    assert!(view(&app).expanded_tool_outputs.is_empty());
    assert_eq!(
        marked_rows(&app),
        ["- two"],
        "the truncated reply shows the second item"
    );

    app.check_view_resize(40);
    finish_view_render(&mut app, 17);

    let shown_items: Vec<usize> = view(&app).text_blocks_of(REPLY_MESSAGE)[1]
        .items(&view(&app).rendered_lines)
        .iter()
        .map(|shown| shown.item)
        .collect();
    assert_eq!(
        shown_items,
        [0],
        "the narrower frame truncates the second item away"
    );
    assert_eq!(text_focus(&app), on_block(1, None), "the list is focused");
}
