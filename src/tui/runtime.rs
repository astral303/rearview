use super::app::{Action, App, AppMode, ListScope, TuiSearchOptions};
use super::backend::ShowAfterMove;
use super::ui;
use crate::clock::Clock;
use crate::config::KeyBindings;
use crate::debug_log;
use crate::error::{AppError, Result};
use crate::history::load_meter::LoadMeter;
use crate::history::{
    Conversation, Environment, LoaderMessage, OperationScope, Patience, RefreshOptions,
    RefreshOutcome, WaitStatus, refresh_in_background, refresh_sessions,
};
use crate::tui::viewer::ToolDisplayMode;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use std::io::{self, BufWriter, Stderr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Frames are assembled in a buffer and reach the terminal in one write.
/// Written straight to unbuffered stderr, a frame is one write per changed
/// cell. The cursor is shown only once it has been moved to the prompt; see
/// [`ShowAfterMove`].
type TerminalBackend = ShowAfterMove<CrosstermBackend<BufWriter<Stderr>>>;

/// Holds a whole redraw of a large terminal, so even a full repaint is one
/// write.
const FRAME_BUFFER_BYTES: usize = 256 * 1024;

struct TerminalGuard {
    terminal: Terminal<TerminalBackend>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().map_err(|e| AppError::Io(io::Error::other(e)))?;

        let mut stderr = io::stderr();
        if let Err(e) = crossterm::execute!(stderr, EnterAlternateScreen, EnableMouseCapture) {
            let _ = terminal::disable_raw_mode();
            return Err(AppError::Io(io::Error::other(e)));
        }

        let backend = ShowAfterMove::new(CrosstermBackend::new(BufWriter::with_capacity(
            FRAME_BUFFER_BYTES,
            stderr,
        )));
        let terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => {
                let _ = terminal::disable_raw_mode();
                let _ =
                    crossterm::execute!(io::stderr(), DisableMouseCapture, LeaveAlternateScreen);
                return Err(AppError::Io(io::Error::other(e)));
            }
        };

        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

const MAX_EVENT_BATCH: usize = 256;

struct FrameState {
    frame_area: Rect,
    viewport_height: usize,
}

impl FrameState {
    fn frame_width(&self) -> usize {
        self.frame_area.width as usize
    }
}

enum EventLoopResult<T> {
    Continue,
    Break,
    Return(T),
}

fn drain_events(wait: Duration) -> Result<Vec<Event>> {
    if !event::poll(wait).map_err(|e| AppError::Io(io::Error::other(e)))? {
        return Ok(Vec::new());
    }

    let mut events = vec![event::read().map_err(|e| AppError::Io(io::Error::other(e)))?];

    while events.len() < MAX_EVENT_BATCH
        && event::poll(Duration::ZERO).map_err(|e| AppError::Io(io::Error::other(e)))?
    {
        events.push(event::read().map_err(|e| AppError::Io(io::Error::other(e)))?);
    }

    Ok(events)
}

fn prepare_frame<B: Backend>(app: &mut App, terminal: &mut Terminal<B>) -> FrameState {
    let frame_area = terminal.get_frame().area();
    let viewport_height = frame_area.height.saturating_sub(3) as usize;

    app.check_view_resize(frame_area.width as usize, viewport_height);
    let viewport_height = match app.app_mode() {
        AppMode::View(state) => {
            ui::view_layout_rects(frame_area, app, state).content.height as usize
        }
        AppMode::List => viewport_height,
    };

    FrameState {
        frame_area,
        viewport_height,
    }
}

fn draw_frame<B: Backend>(app: &App, terminal: &mut Terminal<B>) -> Result<()> {
    terminal
        .draw(|frame| ui::render(frame, app))
        .map_err(|error| AppError::Io(io::Error::other(error.to_string())))?;
    Ok(())
}

fn handle_events<F>(
    app: &mut App,
    frame_state: &FrameState,
    events: Vec<Event>,
    allow_list_click_enter: bool,
    mut on_action: F,
) -> EventLoopResult<Option<Action>>
where
    F: FnMut(&mut App, Action) -> EventLoopResult<Option<Action>>,
{
    for ev in events {
        let key = match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            Event::Mouse(m) => {
                match m.kind {
                    MouseEventKind::ScrollDown => {
                        app.scroll_mouse(3, frame_state.viewport_height);
                    }
                    MouseEventKind::ScrollUp => {
                        app.scroll_mouse(-3, frame_state.viewport_height);
                    }
                    MouseEventKind::Down(MouseButton::Left) if app.is_actions_menu_open() => {
                        if let Some(action) =
                            app.handle_actions_menu_click(m.column, m.row, frame_state.frame_area)
                        {
                            match on_action(app, action) {
                                EventLoopResult::Continue => {}
                                EventLoopResult::Break => return EventLoopResult::Break,
                                EventLoopResult::Return(action) => {
                                    return EventLoopResult::Return(action);
                                }
                            }
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if app.handle_view_click(
                            m.row,
                            frame_state.frame_area,
                            frame_state.viewport_height,
                        ) {
                            return EventLoopResult::Break;
                        }
                        if allow_list_click_enter
                            && app.handle_list_click(m.row, frame_state.frame_area)
                            && app.request_open()
                        {
                            return EventLoopResult::Break;
                        }
                    }
                    MouseEventKind::Moved => {
                        app.handle_view_mouse_move(m.row, frame_state.frame_area);
                    }
                    _ => {}
                }
                continue;
            }
            _ => continue,
        };

        if let Some(action) = app.handle_key(key.code, key.modifiers, frame_state.viewport_height) {
            match on_action(app, action) {
                EventLoopResult::Continue => {}
                EventLoopResult::Break => return EventLoopResult::Break,
                EventLoopResult::Return(action) => return EventLoopResult::Return(action),
            }
        }
        // The frame drawn next shows the list as opening; the load runs after it.
        if app.is_opening() {
            return EventLoopResult::Break;
        }
    }
    EventLoopResult::Continue
}

/// The end of one pass of the event loop.
pub(crate) enum Tick {
    Continue,
    /// Leave the TUI with this action.
    Exit(Action),
}

/// The TUI's event loop body: the load's and the refresh's messages, the
/// timed checks, one frame, the pending open or viewer refresh, then the
/// terminal events of one poll. [`run_with_loader`] and [`run_single_file`]
/// run it against the terminal; a test runs it against a test backend with
/// events of its own.
pub(crate) struct Runtime {
    app: App,
    environment: Environment,
    /// The streaming load's messages; `None` for a file opened directly.
    loader: Option<Receiver<LoaderMessage>>,
    load_meter: Arc<LoadMeter>,
    refresh_options: RefreshOptions,
    /// The background refresh thread's outcome, while one runs.
    refresh: Option<Receiver<Result<RefreshOutcome>>>,
    /// The wait the background refresh thread's operation publishes.
    refresh_wait: Arc<WaitStatus>,
}

impl Runtime {
    /// The list, filled from `loader`, the streaming load whose reads
    /// `load_meter` records, in `environment`. `Ctrl+R` refreshes it with
    /// `refresh_options`.
    pub(crate) fn listing(
        app: App,
        environment: Environment,
        loader: Receiver<LoaderMessage>,
        load_meter: Arc<LoadMeter>,
        refresh_options: RefreshOptions,
    ) -> Self {
        Self {
            app,
            environment,
            loader: Some(loader),
            load_meter,
            refresh_options,
            refresh: None,
            refresh_wait: Arc::default(),
        }
    }

    /// A file opened directly, in `environment`.
    pub(crate) fn single_file(app: App, environment: Environment) -> Self {
        Self {
            app,
            environment,
            loader: None,
            load_meter: Arc::default(),
            refresh_options: RefreshOptions::default(),
            refresh: None,
            refresh_wait: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn app(&self) -> &App {
        &self.app
    }

    /// One pass of the event loop, drawing on `terminal`. `poll` returns the
    /// terminal events that arrive within the time it is given.
    pub(crate) fn tick<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        poll: &mut dyn FnMut(Duration) -> Result<Vec<Event>>,
    ) -> Result<Tick> {
        self.receive_load()?;
        self.receive_refresh()?;
        self.app.apply_finished_refresh();
        self.app.set_cache_wait(if self.app.is_loading() {
            self.load_meter.waiting_for()
        } else if self.refresh.is_some() {
            self.refresh_wait.current()
        } else {
            None
        });
        self.app.check_awaited_refresh();
        if self.refresh.is_none()
            && let Some(rows) = self.app.begin_caching_refresh()
        {
            let environment = self.environment.clone();
            let wait = Arc::clone(&self.refresh_wait);
            self.refresh = Some(refresh_in_background(move || {
                run_operation(&environment, wait, |operation| rows.cache(operation))
            }));
        }

        let frame_state = prepare_frame(&mut self.app, terminal);
        self.app.receive_search_results();
        self.app.receive_fork_read();
        draw_frame(&self.app, terminal)?;
        if self.app.complete_pending_open(frame_state.frame_width())
            || self
                .app
                .complete_pending_view_refresh(frame_state.viewport_height)
        {
            return Ok(Tick::Continue);
        }

        let events = poll(self.poll_timeout())?;
        Ok(match self.handle_events(&frame_state, events) {
            EventLoopResult::Return(Some(action)) => Tick::Exit(action),
            EventLoopResult::Continue | EventLoopResult::Break | EventLoopResult::Return(None) => {
                Tick::Continue
            }
        })
    }

    /// The rows the list holds, for the action the TUI exits with.
    pub(crate) fn into_conversations(self) -> Vec<Conversation> {
        self.app.into_conversations()
    }

    /// Apply every message the load has sent. Fails with the load's fatal
    /// error, or when the finished load found no session.
    fn receive_load(&mut self) -> Result<()> {
        let Some(loader) = &self.loader else {
            return Ok(());
        };
        loop {
            match loader.try_recv() {
                Ok(LoaderMessage::Fatal(error)) => return Err(error),
                Ok(LoaderMessage::ProviderError) => {}
                Ok(LoaderMessage::Batch(conversations)) => {
                    self.app.append_conversations(conversations);
                }
                Ok(LoaderMessage::Progress(progress)) => self.app.report_load_progress(progress),
                Ok(LoaderMessage::Ignored(term)) => self.app.add_active_filter(term),
                Ok(LoaderMessage::SkippedSessions(sessions)) => {
                    self.app.add_skipped_sessions(sessions);
                }
                Ok(LoaderMessage::Changes(changes)) => {
                    self.app
                        .finish_refresh(Ok(RefreshOutcome::Changes(changes)));
                }
                Ok(LoaderMessage::Done) => finish_loading(&mut self.app)?,
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if self.app.is_loading() {
                        finish_loading(&mut self.app)?;
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Hand a finished refresh to the app. A refresh thread that stopped
    /// without an outcome reports as a failed refresh. Fails when the
    /// refresh found the cache's drive short of space: rearview stops.
    fn receive_refresh(&mut self) -> Result<()> {
        let Some(outcome) = self.refresh.as_ref() else {
            return Ok(());
        };
        let outcome = match outcome.try_recv() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(AppError::RefreshStopped),
        };
        self.refresh = None;
        if let Err(error @ AppError::SessionCacheOutOfSpace { .. }) = outcome {
            return Err(error);
        }
        self.app.finish_refresh(outcome);
        Ok(())
    }

    /// The longest the next poll waits for input: briefly while work runs in
    /// the background, until the next timed redraw otherwise.
    fn poll_timeout(&self) -> Duration {
        if self.app.is_loading() || self.refresh.is_some() || self.app.is_reading_fork() {
            Duration::from_millis(50)
        } else if self.app.has_search_work_in_flight() {
            Duration::from_millis(8)
        } else {
            self.app.idle_wait()
        }
    }

    fn handle_events(
        &mut self,
        frame_state: &FrameState,
        events: Vec<Event>,
    ) -> EventLoopResult<Option<Action>> {
        if self.app.is_single_file_mode() {
            return handle_events(
                &mut self.app,
                frame_state,
                events,
                false,
                |_, action| match action {
                    Action::Quit => EventLoopResult::Return(Some(Action::Quit)),
                    _ => EventLoopResult::Continue,
                },
            );
        }
        let environment = &self.environment;
        let refresh_options = self.refresh_options;
        let refresh = &mut self.refresh;
        let refresh_wait = &self.refresh_wait;
        handle_events(
            &mut self.app,
            frame_state,
            events,
            true,
            |app, action| match action {
                Action::Refresh => {
                    if let Some(known) = app.begin_refresh() {
                        let environment = environment.clone();
                        let wait = Arc::clone(refresh_wait);
                        *refresh = Some(refresh_in_background(move || {
                            run_operation(&environment, wait, |operation| {
                                refresh_sessions(&environment, &known, refresh_options, operation)
                                    .map(RefreshOutcome::Changes)
                            })
                        }));
                    }
                    EventLoopResult::Continue
                }
                Action::Delete(ref path) => {
                    delete_selected_session(app, path, environment.clock().as_ref());
                    EventLoopResult::Continue
                }
                _ => EventLoopResult::Return(Some(action)),
            },
        )
    }
}

/// `work`, one operation on `environment`'s session cache that waits for
/// refresh ownership as long as another rearview keeps it, publishing its
/// wait on `wait`. Fails as [`OperationScope::finish`] does too.
fn run_operation<T>(
    environment: &Environment,
    wait: Arc<WaitStatus>,
    work: impl FnOnce(&OperationScope) -> Result<T>,
) -> Result<T> {
    let operation = OperationScope::new(environment.cache_base(), Patience::UntilAcquired, wait);
    let outcome = work(&operation);
    operation.finish().and(outcome)
}

/// Show `app`'s loaded list; fails when it holds no session.
fn finish_loading(app: &mut App) -> Result<()> {
    app.finish_loading();
    if app.conversations().is_empty() {
        return Err(AppError::NoHistoryFound("selected scope".to_string()));
    }
    Ok(())
}

/// Delete the listed session at `path` and leave the viewer, or report why
/// it could not be deleted; log under `clock`'s time.
fn delete_selected_session(app: &mut App, path: &std::path::Path, clock: &dyn Clock) {
    let Some(source) = app.get_selected_source() else {
        let _ = debug_log::log_debug(
            &format!("No listed session to delete at {}", path.display()),
            clock,
        );
        return;
    };
    match crate::history::provider::delete_session(source, path) {
        Ok(_) => {
            app.remove_selected_from_list();
            app.exit_view_mode();
        }
        Err(error) => {
            let _ = debug_log::log_debug(
                &format!("Failed to delete session {}: {}", path.display(), error),
                clock,
            );
            app.report_delete_failure(&error);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_loader(
    rx: Receiver<LoaderMessage>,
    load_meter: Arc<LoadMeter>,
    refresh_options: RefreshOptions,
    tool_display: ToolDisplayMode,
    show_thinking: bool,
    keys: KeyBindings,
    scope: ListScope,
    search_options: TuiSearchOptions,
    active_filters: Vec<crate::history::FilterTerm>,
    environment: Environment,
) -> Result<(Action, Vec<Conversation>)> {
    let mut guard = TerminalGuard::new()?;
    let mut app = App::new_loading_with_options(
        tool_display,
        show_thinking,
        keys,
        scope,
        search_options,
        environment.clone(),
    );
    app.set_active_filters(active_filters);
    app.set_show_last(refresh_options.show_last);
    let mut runtime = Runtime::listing(app, environment, rx, load_meter, refresh_options);
    loop {
        if let Tick::Exit(action) = runtime.tick(&mut guard.terminal, &mut drain_events)? {
            return Ok((action, runtime.into_conversations()));
        }
    }
}

#[cfg(test)]
mod tests;

pub fn run_single_file(
    path: PathBuf,
    source: crate::history::Source,
    tool_display: ToolDisplayMode,
    show_thinking: bool,
    keys: KeyBindings,
    environment: Environment,
) -> Result<()> {
    let mut guard = TerminalGuard::new()?;
    let app = App::new_single_file(
        path,
        source,
        tool_display,
        show_thinking,
        keys,
        environment.clone(),
    );
    let mut runtime = Runtime::single_file(app, environment);
    loop {
        if let Tick::Exit(_) = runtime.tick(&mut guard.terminal, &mut drain_events)? {
            return Ok(());
        }
    }
}
