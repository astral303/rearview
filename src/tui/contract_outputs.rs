//! The TUI's list, search results and viewer as text, for the output
//! contract in `crate::output_contract`. Each is read the way the TUI reads
//! it: the list from the streaming load, the results from the search worker,
//! and each viewer from the session the list opens.

use super::app::{App, AppMode, ListScope, TuiSearchOptions};
use super::ui;
use super::viewer::ToolDisplayMode;
use crate::config::KeyBindings;
use crate::history::{self, Conversation, LoaderMessage};
use crate::time_filter::TimeFilter;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::{Duration, Instant};

const FRAME_WIDTH: u16 = 160;
const FRAME_HEIGHT: u16 = 48;
/// The rows the list leaves to sessions: the frame without its search line,
/// status line and border.
const VIEWPORT_HEIGHT: usize = FRAME_HEIGHT as usize - 3;
/// The longest a search may take before the capture gives up on it.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The text of each TUI surface the output contract compares.
pub(crate) struct TuiOutputs {
    /// The list as it first shows after the load.
    pub(crate) list: String,
    /// The list after each query's search, one frame per query.
    pub(crate) search: String,
    /// Every listed session's viewer, in the default tools summary mode
    /// without thinking, then with every tool shown in full and thinking.
    pub(crate) viewer: String,
}

/// The TUI's outputs over the sessions `environment`'s storages hold, loaded
/// through its session cache, with the options the binary starts with when
/// no configuration is set. The views read the cache the load wrote.
pub(crate) fn tui_outputs(queries: &[&str], environment: history::Environment) -> TuiOutputs {
    let mut app = App::new_loading_with_options(
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        ListScope::default(),
        TuiSearchOptions::default(),
        environment.clone(),
    );
    let time = TimeFilter::default();
    app.set_active_filters(history::active_load_filters(&[&time]));
    app.set_show_last(true);
    let (messages, received) = std::sync::mpsc::channel();
    let options = history::RefreshOptions {
        show_last: true,
        debug_level: None,
        time,
    };
    let loader_environment = environment.clone();
    std::thread::spawn(move || {
        history::stream_all_conversations(
            &messages,
            &loader_environment,
            options,
            &Default::default(),
            &history::Cancellation::default(),
        );
    });
    load(&mut app, received);

    let list = frame_text(&app);
    let search = queries
        .iter()
        .map(|query| format!("=== search {query:?}\n{}", searched_frame(&mut app, query)))
        .collect::<String>();
    let conversations = app.conversations().to_vec();
    let mut viewer = viewer_text(&mut app, "tools summary, thinking hidden");
    let mut full = App::new_with_options(
        conversations,
        ToolDisplayMode::Full,
        true,
        KeyBindings::default(),
        Vec::new(),
        TuiSearchOptions::default(),
        environment,
    );
    full.set_show_last(true);
    viewer.push_str(&viewer_text(&mut full, "tools in full, thinking shown"));
    TuiOutputs {
        list,
        search,
        viewer,
    }
}

/// Feeds `app` the loader's messages, as the TUI's event loop does, until
/// the load is done.
fn load(app: &mut App, messages: std::sync::mpsc::Receiver<LoaderMessage>) {
    for message in messages {
        match message {
            LoaderMessage::Fatal(error) => panic!("the load doesn't fail: {error}"),
            LoaderMessage::ProviderError => {}
            LoaderMessage::Batch(conversations) => app.append_conversations(conversations),
            LoaderMessage::Progress(progress) => app.report_load_progress(progress),
            LoaderMessage::Ignored(term) => app.add_active_filter(term),
            LoaderMessage::SkippedSessions(sessions) => app.add_skipped_sessions(sessions),
            LoaderMessage::Changes(changes) => app.add_launch_changes(changes),
            LoaderMessage::CacheWritesStopped(stale) => {
                app.refresh_what_the_launch_could_not_cache(stale)
            }
            LoaderMessage::Done => break,
        }
    }
    app.finish_loading();
    settle_search(app);
}

/// The frame after typing `query` into an empty search line and the search
/// landing. The search line is cleared after.
fn searched_frame(app: &mut App, query: &str) -> String {
    for character in query.chars() {
        press(app, KeyCode::Char(character));
    }
    settle_search(app);
    let frame = frame_text(app);
    press(app, KeyCode::Esc);
    settle_search(app);
    frame
}

/// Waits for the search worker to answer the latest query.
fn settle_search(app: &mut App) {
    let started = Instant::now();
    loop {
        app.receive_search_results();
        if !app.has_search_work_in_flight() {
            return;
        }
        assert!(
            started.elapsed() < SEARCH_TIMEOUT,
            "the search results arrive"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Each listed session's viewer under `heading`, opened from the list as
/// `Enter` opens it.
fn viewer_text(app: &mut App, heading: &str) -> String {
    let mut text = String::new();
    press(app, KeyCode::Home);
    for row in 0..app.filtered().len() {
        assert!(app.request_open(), "row {row} opens");
        app.complete_pending_open(FRAME_WIDTH as usize);
        let AppMode::View(state) = app.app_mode() else {
            panic!("row {row} opens a viewer");
        };
        text.push_str(&format!(
            "=== viewer, {heading}: {}\n",
            listed_name(&app.conversations()[app.filtered()[row]])
        ));
        for line in &state.rendered_lines {
            let line = line
                .spans
                .iter()
                .map(|(span, _)| span.as_str())
                .collect::<String>();
            text.push_str(line.trim_end());
            text.push('\n');
        }
        app.exit_view_mode();
        press(app, KeyCode::Down);
    }
    text
}

/// The session's agent and its transcript's file name, which names it
/// without the directory a run builds its home in.
fn listed_name(conversation: &Conversation) -> String {
    let file_name = conversation
        .path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    format!("{} {file_name}", conversation.source.label())
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(code, KeyModifiers::empty(), VIEWPORT_HEIGHT);
}

/// The whole frame as the TUI draws it, each row without trailing spaces.
fn frame_text(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(FRAME_WIDTH, FRAME_HEIGHT)).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            let row = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            format!("{}\n", row.trim_end())
        })
        .collect()
}
