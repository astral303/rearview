//! `--report-dropped-records`: the record kinds each agent's reader drops
//! across the user's sessions, by agent and the version each record falls
//! under, for a maintainer reviewing an agent's new releases.
//!
//! Counting re-reads every transcript from disk. The session cache stores no
//! counts, and counting neither reads nor writes it.

use crate::history::format::dropped::{DropOutcome, DroppedRecords};
use crate::history::{Conversation, Source};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

/// The version column for a record that falls under no stated version.
const UNKNOWN_VERSION: &str = "unknown";

/// One row of the report: agent, version, outcome and kind.
type RowKey = (&'static str, String, DropOutcome, String);
type Counts = BTreeMap<RowKey, usize>;

/// Print the report for `conversations` and every sub-agent transcript they
/// hold.
pub fn print_report(conversations: &[Conversation]) {
    print!("{}", render(&count(conversations)));
}

fn count(conversations: &[Conversation]) -> Counts {
    conversations
        .par_iter()
        .flat_map_iter(|conversation| {
            std::iter::once(&conversation.path)
                .chain(&conversation.subagents)
                .map(move |path| (conversation.source, path))
        })
        .map(|(source, path)| transcript_counts(source, path))
        .reduce(Counts::new, merge)
}

/// The dropped records of the transcript at `path`, read as `source`'s
/// format, each under the version it falls under. A line that does not parse
/// states no version and counts under the transcript's first; a transcript
/// that cannot be read counts once as a failed `transcript`.
fn transcript_counts(source: Source, path: &Path) -> Counts {
    let agent = source.display_label();
    let version_label = |version: Option<&str>| version.unwrap_or(UNKNOWN_VERSION).to_owned();
    let mut counts = Counts::new();
    match source
        .provider()
        .format()
        .parse_transcript_counting(path, DroppedRecords::counting())
    {
        Ok(Some(projection)) => {
            for (version, kind, outcome, count) in projection.dropped.counts() {
                *counts
                    .entry((agent, version_label(version), outcome, kind.to_owned()))
                    .or_default() += count;
            }
            if !projection.malformed_lines.is_empty() {
                let version = version_label(projection.dropped.first_version());
                *counts
                    .entry((agent, version, DropOutcome::Failed, "line".to_owned()))
                    .or_default() += projection.malformed_lines.len();
            }
        }
        Ok(None) => {}
        Err(_) => {
            counts.insert(
                (
                    agent,
                    UNKNOWN_VERSION.to_owned(),
                    DropOutcome::Failed,
                    "transcript".to_owned(),
                ),
                1,
            );
        }
    }
    counts
}

fn merge(mut into: Counts, from: Counts) -> Counts {
    for (key, count) in from {
        *into.entry(key).or_default() += count;
    }
    into
}

const HEADERS: [&str; 5] = ["agent", "version", "outcome", "count", "kind"];

/// The report as a plain table, a column per field, in agent, version,
/// outcome and kind order.
fn render(counts: &Counts) -> String {
    if counts.is_empty() {
        return "No record was dropped.\n".to_owned();
    }
    let rows = counts
        .iter()
        .map(|((agent, version, outcome, kind), count)| {
            [
                (*agent).to_owned(),
                version.clone(),
                outcome.label().to_owned(),
                count.to_string(),
                kind.clone(),
            ]
        })
        .collect::<Vec<_>>();
    let widths: [usize; 5] = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| row[column].len())
            .chain([HEADERS[column].len()])
            .max()
            .unwrap_or(0)
    });
    let mut table = String::new();
    for row in std::iter::once(HEADERS.map(str::to_owned)).chain(rows) {
        let line = format!(
            "{:<agent$}  {:<version$}  {:<outcome$}  {:>count$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            agent = widths[0],
            version = widths[1],
            outcome = widths[2],
            count = widths[3],
        );
        table.push_str(line.trim_end());
        table.push('\n');
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::format::SessionProjection;
    use crate::history::provider::contract_tests::every_record_sessions;

    /// Counting adds the counts and changes nothing else, and a parse that
    /// does not count records none.
    #[test]
    fn counting_leaves_every_agents_projection_as_ordinary_parse_reads_it() {
        for session in every_record_sessions() {
            let row = &session.row;
            let format = row.source.provider().format();
            for path in std::iter::once(&row.path).chain(&row.subagents) {
                let ordinary = format.parse_transcript(path).unwrap().unwrap();
                let counted = format
                    .parse_transcript_counting(path, DroppedRecords::counting())
                    .unwrap()
                    .unwrap();

                assert_eq!(ordinary.dropped, DroppedRecords::default(), "{path:?}");
                assert_eq!(
                    format!(
                        "{:?}",
                        SessionProjection {
                            dropped: DroppedRecords::default(),
                            ..counted
                        }
                    ),
                    format!("{ordinary:?}"),
                    "{path:?}"
                );
            }
        }
    }

    fn row(agent: &'static str, version: &str, outcome: DropOutcome, kind: &str) -> RowKey {
        (agent, version.to_owned(), outcome, kind.to_owned())
    }

    #[test]
    fn report_is_table_in_agent_version_outcome_and_kind_order() {
        let counts = Counts::from([
            (
                row("Codex", "0.160.0", DropOutcome::Skipped, "type=world_state"),
                4,
            ),
            (
                row(
                    "Claude",
                    "2.1.300",
                    DropOutcome::Unrecognised,
                    "type=teleport",
                ),
                12,
            ),
            (row("Grok", "unknown", DropOutcome::Failed, "line"), 1),
        ]);

        assert_eq!(
            render(&counts),
            concat!(
                "agent   version  outcome       count  kind\n",
                "Claude  2.1.300  unrecognised     12  type=teleport\n",
                "Codex   0.160.0  skipped           4  type=world_state\n",
                "Grok    unknown  failed            1  line\n",
            )
        );
    }

    #[test]
    fn empty_report_prints_that_nothing_was_dropped() {
        assert_eq!(render(&Counts::new()), "No record was dropped.\n");
    }

    #[test]
    fn unreadable_transcript_counts_as_failed_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let unreadable = directory.path().join("unreadable.jsonl");
        std::fs::create_dir(&unreadable).unwrap();

        assert_eq!(
            transcript_counts(Source::Claude, &unreadable),
            Counts::from([(
                row("Claude", "unknown", DropOutcome::Failed, "transcript"),
                1
            )])
        );
    }

    #[test]
    fn malformed_lines_count_as_failed_lines_under_transcripts_first_version() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","version":"2.1.300","message":{"role":"user","content":"hi"}}"#,
                "\n{not json\n",
                r#"{"type":"user","version":"2.1.301","message":{"role":"user","content":"hi"}}"#,
                "\n",
            ),
        )
        .unwrap();

        assert_eq!(
            transcript_counts(Source::Claude, &transcript),
            Counts::from([(row("Claude", "2.1.300", DropOutcome::Failed, "line"), 1)])
        );
    }
}
