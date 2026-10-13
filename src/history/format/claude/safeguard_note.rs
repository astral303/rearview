//! Claude Code's note to the model after a safeguard refusal warning: a
//! `user` record marked `isMeta`, with the stopped reply's
//! `refusedMessageId`. The warning before it shows the stop.

use serde_json::Value;

const REFUSED_MESSAGE_ID: &str = "refusedMessageId";

/// Requires both `isMeta` and a string `refusedMessageId`: a `user` record
/// without `isMeta` is the user's own prompt.
pub(super) fn is_note_to_the_model(line: &str) -> bool {
    // Parse the line a second time only when it can hold the note.
    if !line.contains(REFUSED_MESSAGE_ID) {
        return false;
    }
    serde_json::from_str::<Value>(line).is_ok_and(|record| {
        record.get("isMeta").and_then(Value::as_bool) == Some(true)
            && record.get(REFUSED_MESSAGE_ID).is_some_and(Value::is_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meta_user_record_naming_the_stopped_reply_is_a_note_to_the_model() {
        assert!(is_note_to_the_model(
            r#"{"type":"user","isMeta":true,"refusedMessageId":"msg_1","message":{"role":"user","content":"Your response above was stopped."}}"#
        ));
    }

    #[test]
    fn a_record_without_is_meta_or_a_stopped_reply_isnt_a_note_to_the_model() {
        assert!(!is_note_to_the_model(
            r#"{"type":"user","refusedMessageId":"msg_1","message":{"role":"user","content":"refusedMessageId"}}"#
        ));
        assert!(!is_note_to_the_model(
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"refusedMessageId"}}"#
        ));
    }
}
