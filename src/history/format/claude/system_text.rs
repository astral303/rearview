//! Session recaps (`away_summary`) and notices (`model_refusal_fallback`,
//! `informational`): `system` records. Claude Code shows each as text in the
//! conversation.

use serde_json::Value;

use crate::log_entry::{LogEntry, NOTICE_LABEL, RECAP_LABEL};

const RECAP_SUBTYPE: &str = "away_summary";
const NOTICE_SUBTYPES: [&str; 2] = ["model_refusal_fallback", "informational"];

/// Claude Code ends some recaps with this hint.
const RECAP_HINT: &str = "(disable recaps in /config)";

/// A recap or a notice in `entry` as a labelled row. `None` for any other
/// record, and for a record with empty `content`.
pub(super) fn system_text_row(entry: &LogEntry) -> Option<LogEntry> {
    let LogEntry::System { subtype, extra, .. } = entry else {
        return None;
    };
    let content = extra.get("content").and_then(Value::as_str)?.trim_end();
    let (label, text) = if subtype == RECAP_SUBTYPE {
        (RECAP_LABEL, without_recap_hint(content))
    } else if NOTICE_SUBTYPES.contains(&subtype.as_str()) {
        (NOTICE_LABEL, content)
    } else {
        return None;
    };
    (!text.is_empty()).then(|| LogEntry::PiMetadata {
        label: label.to_owned(),
        text: text.to_owned(),
        timestamp: extra
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned),
        searchable: true,
        usage: None,
    })
}

fn without_recap_hint(recap: &str) -> &str {
    recap.strip_suffix(RECAP_HINT).map_or(recap, str::trim_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: &str) -> Option<(String, String)> {
        let entry = serde_json::from_str::<LogEntry>(line).unwrap();
        match system_text_row(&entry)? {
            LogEntry::PiMetadata { label, text, .. } => Some((label, text)),
            other => panic!("a metadata row: {other:?}"),
        }
    }

    #[test]
    fn recap_reads_as_recap_row_without_its_hint() {
        assert_eq!(
            row(
                r#"{"type":"system","subtype":"away_summary","content":"Goal: fix the parser. Next: run the tests. (disable recaps in /config)"}"#
            ),
            Some((
                RECAP_LABEL.to_owned(),
                "Goal: fix the parser. Next: run the tests.".to_owned()
            ))
        );
        assert_eq!(
            row(r#"{"type":"system","subtype":"away_summary","content":"Goal: fix the parser."}"#),
            Some((RECAP_LABEL.to_owned(), "Goal: fix the parser.".to_owned()))
        );
    }

    #[test]
    fn refusal_fallback_and_informational_record_read_as_notice_rows() {
        assert_eq!(
            row(
                r#"{"type":"system","subtype":"model_refusal_fallback","content":"The model's safeguards flagged this message. Switched to another model.","level":"warning"}"#
            ),
            Some((
                NOTICE_LABEL.to_owned(),
                "The model's safeguards flagged this message. Switched to another model."
                    .to_owned()
            ))
        );
        assert_eq!(
            row(
                r#"{"type":"system","subtype":"informational","content":"Backgrounding after the current tool finishes…","level":"warning"}"#
            ),
            Some((
                NOTICE_LABEL.to_owned(),
                "Backgrounding after the current tool finishes…".to_owned()
            ))
        );
    }

    #[test]
    fn refusal_with_no_fallback_doesnt_read_as_row() {
        assert_eq!(
            row(r#"{"type":"system","subtype":"model_refusal_no_fallback","content":""}"#),
            None
        );
    }
}
