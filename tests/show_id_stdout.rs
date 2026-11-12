//! `id=$(rearview --show-id)` captures the session ID and nothing else.
//!
//! The conversation list draws on stderr and prints the selection on stdout,
//! so a shell's command substitution sees one line. Terminal background
//! detection writes its query to stdout, and until the probe was limited to a
//! terminal stdout, the query's escape bytes went into `$id` ahead of the ID.
//! Reproducing that needs a pseudo-terminal on stdin and stderr with stdout
//! redirected, which `Command::output` cannot arrange. Pseudo-terminals are
//! Unix-only, and so is the probe.
#![cfg(unix)]

mod common;

use nix::pty::{OpenptyResult, Winsize, openpty};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SESSION_ID: &str = "12345678-1234-4234-9234-123456789abc";
/// Appears in the fixture conversation's list row.
const SENTINEL: &str = "showid sentinel";
/// Starts the search bar's status while the list loads.
const LOADING_STATUS: &str = "Loading";
const CTRL_O: &[u8] = b"\x0f";
const TERMINAL_ROWS: u16 = 24;
const TERMINAL_COLUMNS: u16 = 80;
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn show_id_prints_only_session_id_when_stdout_is_captured() {
    let config = tempfile::tempdir().expect("config");
    let home = tempfile::tempdir().expect("home");
    write_claude_session(config.path());

    let mut list = ConversationList::spawn(config.path(), home.path(), &["--show-id"]);
    list.wait_until_loaded(SENTINEL);
    list.press(CTRL_O);
    let status = list.wait_for_exit();

    assert!(
        status.success(),
        "exit status {status}; the terminal showed:\n{}",
        list.screen_text()
    );
    let stdout = list.stdout();
    assert_eq!(
        stdout,
        format!("{SESSION_ID}\n"),
        "stdout carried more than the session ID: {}",
        stdout.escape_debug()
    );
}

fn write_claude_session(config: &Path) {
    let project = config.join("projects").join("-tmp-show-id-test");
    std::fs::create_dir_all(&project).expect("create project");
    let user = serde_json::json!({
        "type": "user",
        "uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "timestamp": "2026-07-20T00:00:00Z",
        "cwd": "/tmp/show-id-test",
        "sessionId": SESSION_ID,
        "message": {"role": "user", "content": format!("{SENTINEL} question")}
    });
    let assistant = serde_json::json!({
        "type": "assistant",
        "uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "timestamp": "2026-07-20T00:00:01Z",
        "cwd": "/tmp/show-id-test",
        "sessionId": SESSION_ID,
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": format!("{SENTINEL} answer")}]
        }
    });
    std::fs::write(
        project.join(format!("{SESSION_ID}.jsonl")),
        format!("{user}\n{assistant}\n"),
    )
    .expect("write transcript");
}

/// `rearview` showing the conversation list on a pseudo-terminal, with stdout
/// captured the way a shell's command substitution captures it.
struct ConversationList {
    child: Child,
    /// The terminal side of the pseudo-terminal: keys go in, frames come out.
    terminal: File,
    /// Everything the list has written to the terminal so far.
    output: Arc<Mutex<Vec<u8>>>,
}

impl ConversationList {
    fn spawn(config: &Path, home: &Path, args: &[&str]) -> Self {
        let OpenptyResult { master, slave } = openpty(
            &Winsize {
                ws_row: TERMINAL_ROWS,
                ws_col: TERMINAL_COLUMNS,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            None,
        )
        .expect("open a pseudo-terminal");
        let stdin = slave.try_clone().expect("share the terminal with stdin");

        let mut command = common::isolated_rearview(config);
        command
            .args(args)
            // config.toml is read from $HOME/.config; the user's must stay out.
            .env("HOME", home)
            // Without a terminal name the probe gives up before writing anything.
            .env("TERM", "xterm-256color")
            .env_remove("COLORFGBG")
            .env("CLAUDE_CONFIG_DIR", config)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe system calls, and the
        // closure touches nothing else between fork and exec.
        unsafe {
            command.pre_exec(|| {
                // Make the pseudo-terminal the controlling terminal, as a shell
                // would: the list reads its size from /dev/tty, and raw mode
                // needs the process in the terminal's foreground group.
                nix::unistd::setsid()?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("run rearview");
        // Closes this side's copies of the terminal's process end, so reading
        // the terminal ends when the child exits.
        drop(command);

        let terminal = File::from(master);
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut frames = terminal.try_clone().expect("read from the terminal");
        let sink = Arc::clone(&output);
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(read) = frames.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
        });

        Self {
            child,
            terminal,
            output,
        }
    }

    /// Waits until the list shows `needle` and has finished loading. While
    /// it loads, the list shows its sessions but doesn't select one on
    /// Ctrl+O.
    fn wait_until_loaded(&self, needle: &str) {
        let started = Instant::now();
        loop {
            let screen = self.screen_text();
            if screen.contains(needle) && !screen.contains(LOADING_STATUS) {
                return;
            }
            assert!(
                started.elapsed() < LOAD_TIMEOUT,
                "the list did not show {needle:?} and finish loading within {LOAD_TIMEOUT:?}; the terminal showed:\n{screen}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn press(&mut self, keys: &[u8]) {
        self.terminal
            .write_all(keys)
            .expect("send keys to the terminal");
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("poll rearview") {
                return status;
            }
            assert!(
                started.elapsed() < EXIT_TIMEOUT,
                "rearview kept running {EXIT_TIMEOUT:?} after the key; the terminal showed:\n{}",
                self.screen_text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Everything written to stdout; call once the child has exited.
    fn stdout(&mut self) -> String {
        let mut bytes = Vec::new();
        self.child
            .stdout
            .take()
            .expect("stdout is piped")
            .read_to_end(&mut bytes)
            .expect("read stdout");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The terminal's rows as the list last drew them.
    fn screen_text(&self) -> String {
        drawn_rows(&self.output.lock().unwrap()).join("\n")
    }
}

impl Drop for ConversationList {
    fn drop(&mut self) {
        // A test that failed before the key leaves the list running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The terminal's rows after the output `raw`. Text lands where the cursor
/// moves put it and a screen clear empties the rows; other escape sequences
/// are dropped. Every character counts as one column wide.
fn drawn_rows(raw: &[u8]) -> Vec<String> {
    const ESC: char = '\u{1b}';
    const BEL: char = '\u{7}';
    let mut rows = vec![vec![' '; usize::from(TERMINAL_COLUMNS)]; usize::from(TERMINAL_ROWS)];
    let (mut row, mut column) = (0, 0);
    let text = String::from_utf8_lossy(raw);
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            ESC => match characters.next() {
                // CSI: parameter and intermediate bytes up to a final byte in @..~.
                Some('[') => {
                    let mut parameters = String::new();
                    for character in characters.by_ref() {
                        if ('@'..='~').contains(&character) {
                            match character {
                                'H' => (row, column) = cursor_position(&parameters),
                                'J' => rows.iter_mut().for_each(|cells| cells.fill(' ')),
                                _ => {}
                            }
                            break;
                        }
                        parameters.push(character);
                    }
                }
                // OSC: a string ended by BEL or ESC \.
                Some(']') => {
                    while let Some(character) = characters.next() {
                        if character == BEL {
                            break;
                        }
                        if character == ESC {
                            characters.next();
                            break;
                        }
                    }
                }
                // Two-byte sequences such as ESC 7 and ESC =.
                _ => {}
            },
            ' '.. => {
                if let Some(cell) = rows.get_mut(row).and_then(|cells| cells.get_mut(column)) {
                    *cell = character;
                }
                column += 1;
            }
            _ => {}
        }
    }
    rows.into_iter()
        .map(|cells| cells.into_iter().collect::<String>().trim_end().to_owned())
        .collect()
}

/// The zero-based row and column of a cursor move's `row;column` parameters,
/// which count from 1 and default to 1.
fn cursor_position(parameters: &str) -> (usize, usize) {
    let mut positions = parameters
        .split(';')
        .map(|position| position.parse::<usize>().unwrap_or(1).saturating_sub(1));
    (positions.next().unwrap_or(0), positions.next().unwrap_or(0))
}
