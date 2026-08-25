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

use nix::pty::{OpenptyResult, Winsize, openpty};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SESSION_ID: &str = "12345678-1234-4234-9234-123456789abc";
/// Appears in the fixture conversation's list row once the list has loaded.
const SENTINEL: &str = "showid sentinel";
const CTRL_O: &[u8] = b"\x0f";
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn show_id_prints_only_the_session_id_when_stdout_is_captured() {
    let config = tempfile::tempdir().expect("config");
    let home = tempfile::tempdir().expect("home");
    write_claude_session(config.path());

    let mut list = ConversationList::spawn(config.path(), home.path(), &["--show-id"]);
    list.wait_until_drawn(SENTINEL);
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
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            None,
        )
        .expect("open a pseudo-terminal");
        let stdin = slave.try_clone().expect("share the terminal with stdin");

        let mut command = Command::new(env!("CARGO_BIN_EXE_rearview"));
        command
            .args(args)
            // config.toml is read from $HOME/.config; the user's must stay out.
            .env("HOME", home)
            // Without a terminal name the probe gives up before writing anything.
            .env("TERM", "xterm-256color")
            .env_remove("COLORFGBG")
            .env("CLAUDE_CONFIG_DIR", config)
            .env("REARVIEW_CACHE_DIR", config.join("cache"))
            .env(
                "PI_CODING_AGENT_SESSION_DIR",
                config.join("empty-agent-sessions"),
            )
            .env("CODEX_HOME", config.join("empty-codex-home"))
            .env("KIMI_CODE_HOME", config.join("empty-kimi-home"))
            .env("OPENCODE_DB", config.join("empty-opencode.db"))
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

    /// Waits until the text the list has drawn contains `needle`.
    fn wait_until_drawn(&self, needle: &str) {
        let started = Instant::now();
        while !self.screen_text().contains(needle) {
            assert!(
                started.elapsed() < LOAD_TIMEOUT,
                "the list did not draw {needle:?} within {LOAD_TIMEOUT:?}; the terminal showed:\n{}",
                self.screen_text()
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

    /// The terminal output so far without its escape sequences: roughly what
    /// a person would see, minus layout.
    fn screen_text(&self) -> String {
        visible_text(&self.output.lock().unwrap())
    }
}

impl Drop for ConversationList {
    fn drop(&mut self) {
        // A test that failed before the key leaves the list running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drops ANSI escape sequences, keeping printable text and newlines.
fn visible_text(raw: &[u8]) -> String {
    const ESC: u8 = 0x1b;
    const BEL: u8 = 0x07;
    let mut text = Vec::with_capacity(raw.len());
    let mut bytes = raw.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            ESC => match bytes.next() {
                // CSI: parameter and intermediate bytes up to a final byte in @..~.
                Some(b'[') => {
                    for byte in bytes.by_ref() {
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                // OSC: a string ended by BEL or ESC \.
                Some(b']') => {
                    while let Some(byte) = bytes.next() {
                        if byte == BEL {
                            break;
                        }
                        if byte == ESC {
                            bytes.next();
                            break;
                        }
                    }
                }
                // Two-byte sequences such as ESC 7 and ESC =.
                _ => {}
            },
            b'\n' | 0x20.. => text.push(byte),
            _ => {}
        }
    }
    String::from_utf8_lossy(&text).into_owned()
}
