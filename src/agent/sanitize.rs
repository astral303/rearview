//! Terminal codes: escape sequences, and the control characters other than
//! line breaks and tabs. Search and `agent read` show text without them; the
//! viewer shows them. The session cache stores a text without its codes and
//! the codes beside it, so either form reads back without parsing again.

use std::borrow::Cow;
use std::ops::Range;

pub fn sanitize_agent_text(text: &str) -> String {
    without_terminal_codes(text).into_owned()
}

/// [`sanitize_agent_text`]'s output, borrowing `text` when it doesn't hold
/// any terminal codes.
pub(crate) fn without_terminal_codes(text: &str) -> Cow<'_, str> {
    let mut runs = code_runs(text).peekable();
    if runs.peek().is_none() {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    let mut shown_from = 0;
    for codes in runs {
        output.push_str(&text[shown_from..codes.start]);
        shown_from = codes.end;
    }
    output.push_str(&text[shown_from..]);
    Cow::Owned(output)
}

/// A text split into the text without its terminal codes and the codes.
pub(crate) struct SplitText<'a> {
    /// [`sanitize_agent_text`]'s output for the text.
    pub(crate) text: Cow<'a, str>,
    /// The codes, encoded as [`join_terminal_codes`] reads them; `None` when
    /// the text doesn't hold any.
    pub(crate) codes: Option<Vec<u8>>,
}

/// Splits `raw` into the text without its terminal codes and the codes, each
/// run of adjacent codes with its byte offset in that text.
///
/// The codes are encoded as one entry per run: the run's offset after the
/// previous run's, as a LEB128 number, its length in bytes, as a LEB128
/// number, then its bytes.
pub(crate) fn split_terminal_codes(raw: &str) -> SplitText<'_> {
    let runs = code_runs(raw).collect::<Vec<_>>();
    if runs.is_empty() {
        return SplitText {
            text: Cow::Borrowed(raw),
            codes: None,
        };
    }
    let mut text = String::with_capacity(raw.len());
    let mut codes = Vec::new();
    let mut shown_from = 0;
    let mut previous_offset = 0;
    for run in runs {
        text.push_str(&raw[shown_from..run.start]);
        push_number(&mut codes, text.len() - previous_offset);
        push_number(&mut codes, run.len());
        codes.extend_from_slice(&raw.as_bytes()[run.clone()]);
        previous_offset = text.len();
        shown_from = run.end;
    }
    text.push_str(&raw[shown_from..]);
    SplitText {
        text: Cow::Owned(text),
        codes: Some(codes),
    }
}

/// `text` with `codes`, as [`split_terminal_codes`] encoded them, back in
/// place: the text that was split.
pub(crate) fn join_terminal_codes(
    text: &str,
    mut codes: &[u8],
) -> Result<String, MalformedTerminalCodes> {
    let mut joined = Vec::with_capacity(text.len() + codes.len());
    let mut offset = 0;
    let mut shown_from = 0;
    while !codes.is_empty() {
        offset += take_number(&mut codes)?;
        let length = take_number(&mut codes)?;
        let shown = text.get(shown_from..offset).ok_or(MalformedTerminalCodes)?;
        let run = codes.get(..length).ok_or(MalformedTerminalCodes)?;
        joined.extend_from_slice(shown.as_bytes());
        joined.extend_from_slice(run);
        codes = &codes[length..];
        shown_from = offset;
    }
    joined.extend_from_slice(
        text.get(shown_from..)
            .ok_or(MalformedTerminalCodes)?
            .as_bytes(),
    );
    String::from_utf8(joined).map_err(|_| MalformedTerminalCodes)
}

/// Stored terminal codes that do not fit the text stored beside them.
#[derive(Debug)]
pub(crate) struct MalformedTerminalCodes;

impl std::fmt::Display for MalformedTerminalCodes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stored terminal codes do not fit their text")
    }
}

impl std::error::Error for MalformedTerminalCodes {}

fn push_number(bytes: &mut Vec<u8>, mut number: usize) {
    while number >= 0x80 {
        bytes.push((number & 0x7F) as u8 | 0x80);
        number >>= 7;
    }
    bytes.push(number as u8);
}

fn take_number(bytes: &mut &[u8]) -> Result<usize, MalformedTerminalCodes> {
    let mut number = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or(MalformedTerminalCodes)?;
        *bytes = rest;
        number |= usize::from(byte & 0x7F) << shift;
        if byte < 0x80 {
            return Ok(number);
        }
    }
    Err(MalformedTerminalCodes)
}

/// The byte range of each run of adjacent terminal codes in `text`, in
/// order.
fn code_runs(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut chars = text
        .as_bytes()
        .iter()
        .any(|&byte| may_start_code(byte))
        .then(|| text.char_indices())
        .into_iter()
        .flatten()
        .peekable();
    let mut state = Sanitiser::Text;
    std::iter::from_fn(move || {
        let start = loop {
            let (offset, ch) = chars.next()?;
            if !state.keeps(ch) {
                break offset;
            }
        };
        let mut end = text.len();
        while let Some(&(offset, ch)) = chars.peek() {
            if state.keeps(ch) {
                end = offset;
                break;
            }
            chars.next();
        }
        // The kept character ends the run; the next run starts after it.
        chars.next();
        Some(start..end)
    })
}

/// True for the first byte of a terminal code, and for 0xC2, which starts
/// both C1 controls and other characters.
fn may_start_code(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t' && byte != b'\n') || byte == 0x7F || byte == 0xC2
}

/// The terminal code recogniser, one character at a time.
#[derive(Clone, Copy)]
enum Sanitiser {
    Text,
    /// After an escape character.
    Escape,
    /// Inside a control sequence, until its final character.
    ControlSequence,
    /// Inside an operating system command or string, until a bell or a
    /// string terminator.
    CommandString {
        after_escape: bool,
    },
}

impl Sanitiser {
    /// True when `ch` is text, false when it belongs to a terminal code.
    fn keeps(&mut self, ch: char) -> bool {
        match *self {
            Self::Text => {
                if ch == '\u{1b}' {
                    *self = Self::Escape;
                    false
                } else {
                    ch == '\n' || ch == '\t' || (!ch.is_control() && ch != '\u{7f}')
                }
            }
            Self::Escape => {
                *self = match ch {
                    '[' => Self::ControlSequence,
                    ']' | 'P' | '^' | '_' => Self::CommandString {
                        after_escape: false,
                    },
                    _ => Self::Text,
                };
                false
            }
            Self::ControlSequence => {
                if ('@'..='~').contains(&ch) {
                    *self = Self::Text;
                }
                false
            }
            Self::CommandString { after_escape } => {
                *self = if ch == '\u{7}' || (after_escape && ch == '\\') {
                    Self::Text
                } else {
                    Self::CommandString {
                        after_escape: ch == '\u{1b}',
                    }
                };
                false
            }
        }
    }
}

#[cfg(test)]
mod tests;
