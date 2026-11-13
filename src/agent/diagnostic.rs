use crate::agent::protocol::escape_atom;
use crate::agent::sanitize::sanitize_agent_text;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentErrorKind {
    InvalidRef,
    AmbiguousRef,
    NotFound,
    OutOfRange,
    BudgetTooSmall,
    MalformedTranscript,
    Io,
    SemanticUnavailable,
}

impl AgentErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRef => "invalid-ref",
            Self::AmbiguousRef => "ambiguous-ref",
            Self::NotFound => "not-found",
            Self::OutOfRange => "out-of-range",
            Self::BudgetTooSmall => "budget-too-small",
            Self::MalformedTranscript => "malformed-transcript",
            Self::Io => "io",
            Self::SemanticUnavailable => "semantic-unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentError {
    pub kind: AgentErrorKind,
    pub reference: Option<String>,
    pub detail: String,
}

impl AgentError {
    pub fn new(kind: AgentErrorKind, reference: Option<&str>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            reference: reference.map(str::to_string),
            detail: detail.into(),
        }
    }

    pub fn invalid_ref(reference: &str, detail: impl Into<String>) -> Self {
        Self::new(AgentErrorKind::InvalidRef, Some(reference), detail)
    }

    pub fn out_of_range(reference: Option<&str>, detail: impl Into<String>) -> Self {
        Self::new(AgentErrorKind::OutOfRange, reference, detail)
    }

    pub fn io(reference: Option<&str>, detail: impl Into<String>) -> Self {
        Self::new(AgentErrorKind::Io, reference, detail)
    }

    pub fn malformed_transcript(reference: Option<&str>, detail: impl Into<String>) -> Self {
        Self::new(AgentErrorKind::MalformedTranscript, reference, detail)
    }

    pub fn semantic_unavailable(detail: impl Into<String>) -> Self {
        Self::new(AgentErrorKind::SemanticUnavailable, None, detail)
    }

    /// Converts `warning`, a session read's warning, into the error for
    /// `reference` when it reports a failed read; `None` for a skipped
    /// session's warning.
    pub(crate) fn of_read_warning(warning: &AgentWarning, reference: &str) -> Option<Self> {
        let kind = match warning.kind {
            AgentWarningKind::MalformedTranscript => AgentErrorKind::MalformedTranscript,
            AgentWarningKind::Io => AgentErrorKind::Io,
            _ => return None,
        };
        Some(Self::new(kind, Some(reference), warning.detail.clone()))
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for AgentError {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AgentWarningKind {
    Skipped,
    MalformedTranscript,
    Io,
    SemanticUnavailable,
    /// Sessions this tool ignores because of how the agent stored them, or
    /// did not load because the agent's session list could not be read; the
    /// detail names the agent and the reason.
    Ignored,
    /// Passages the semantic index has no embedding for, so they could not be
    /// ranked; the detail gives the counts and the remedy.
    PartialIndex,
    /// Sessions whose cached copy is out of date or missing, because another
    /// rearview was refreshing the cache or this rearview's writes stopped;
    /// the detail holds the record's `refs=`, `cached-until=` and
    /// `omitted-sessions=` atoms and the sentence for its cause.
    StaleCache,
}

impl AgentWarningKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::MalformedTranscript => "malformed-transcript",
            Self::Io => "io",
            Self::SemanticUnavailable => "semantic-unavailable",
            Self::Ignored => "ignored",
            Self::PartialIndex => "partial-index",
            Self::StaleCache => "stale-cache",
        }
    }

    /// The kind `atom` names, as [`Self::as_str`] writes it.
    fn of_atom(atom: &str) -> Option<Self> {
        [
            Self::Skipped,
            Self::MalformedTranscript,
            Self::Io,
            Self::SemanticUnavailable,
            Self::Ignored,
            Self::PartialIndex,
            Self::StaleCache,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == atom)
    }
}

/// The sentence a stale-cache record ends with while another rearview
/// refreshes the cache.
const STALE_CACHE_SENTENCE: &str = "another rearview is refreshing these sessions; repeat the command after it finishes to include their newest messages";

/// The sentence a stale-cache record ends with once the command's own cache
/// writes stopped.
const WRITES_STOPPED_SENTENCE: &str = "rearview could not update these sessions in the session cache; repeat the command to include their newest messages";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentWarning {
    pub kind: AgentWarningKind,
    pub reference: Option<String>,
    pub detail: String,
}

impl AgentWarning {
    pub fn from_app_error(error: &crate::error::AppError, reference: Option<&str>) -> Self {
        if let crate::error::AppError::Agent(agent_error) = error {
            let mut warning = Self::from_error(agent_error);
            if reference.is_some() {
                warning.reference = reference.map(str::to_string);
            }
            return warning;
        }
        Self {
            kind: AgentWarningKind::Io,
            reference: reference.map(str::to_string),
            detail: error.to_string(),
        }
    }

    pub fn from_error(error: &AgentError) -> Self {
        Self {
            kind: match error.kind {
                AgentErrorKind::MalformedTranscript => AgentWarningKind::MalformedTranscript,
                AgentErrorKind::SemanticUnavailable => AgentWarningKind::SemanticUnavailable,
                _ => AgentWarningKind::Io,
            },
            reference: error.reference.clone(),
            detail: error.detail.clone(),
        }
    }

    /// The warning as the session cache records it: its kind's atom, a
    /// space, then its detail. The reference is left out.
    pub(crate) fn to_recorded(&self) -> String {
        format!("{} {}", self.kind.as_str(), self.detail)
    }

    /// The warning `recorded` holds, as [`Self::to_recorded`] writes it,
    /// without a reference; `None` when it does not start with a kind.
    pub(crate) fn from_recorded(recorded: &str) -> Option<Self> {
        let (atom, detail) = recorded.split_once(' ')?;
        Some(Self {
            kind: AgentWarningKind::of_atom(atom)?,
            reference: None,
            detail: detail.to_owned(),
        })
    }

    pub fn skipped(reference: Option<&str>, detail: impl Into<String>) -> Self {
        Self {
            kind: AgentWarningKind::Skipped,
            reference: reference.map(str::to_string),
            detail: detail.into(),
        }
    }

    pub fn ignored(detail: impl Into<String>) -> Self {
        Self {
            kind: AgentWarningKind::Ignored,
            reference: None,
            detail: detail.into(),
        }
    }

    pub fn partial_index(detail: impl Into<String>) -> Self {
        Self {
            kind: AgentWarningKind::PartialIndex,
            reference: None,
            detail: detail.into(),
        }
    }

    /// The record of stale sessions, read from the cache as it was for
    /// `cause`: it names the sessions at `refs` and counts
    /// `omitted_sessions` more. `cached_until` is the earliest end of the
    /// cached messages of the sessions at `refs`; `None` when none of them
    /// is in the session cache. The record is `None` without a ref or an
    /// omitted session.
    pub fn stale_cache(
        refs: &[String],
        cached_until: Option<chrono::DateTime<crate::clock::Zone>>,
        omitted_sessions: usize,
        cause: crate::history::StaleCause,
    ) -> Option<Self> {
        if refs.is_empty() && omitted_sessions == 0 {
            return None;
        }
        let mut atoms = Vec::new();
        if !refs.is_empty() {
            atoms.push(format!("refs={}", refs.join(",")));
        }
        if let Some(cached_until) = cached_until {
            atoms.push(format!(
                "cached-until={}",
                cached_until.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
            ));
        }
        if omitted_sessions > 0 {
            atoms.push(format!("omitted-sessions={omitted_sessions}"));
        }
        let mut detail = atoms.join(" ");
        detail.push_str(" : ");
        detail.push_str(match cause {
            crate::history::StaleCause::AnotherRearview => STALE_CACHE_SENTENCE,
            crate::history::StaleCause::WritesStopped => WRITES_STOPPED_SENTENCE,
        });
        Some(Self {
            kind: AgentWarningKind::StaleCache,
            reference: None,
            detail,
        })
    }
}

pub fn format_error(error: &AgentError) -> String {
    format_record(
        "agent-error",
        error.kind.as_str(),
        error.reference.as_deref(),
        &error.detail,
    )
}

pub fn format_warning(warning: &AgentWarning) -> String {
    if warning.kind == AgentWarningKind::StaleCache {
        return format!(
            "protocol agent-warning {} {}\n",
            warning.kind.as_str(),
            warning.detail
        );
    }
    format_record(
        "agent-warning",
        warning.kind.as_str(),
        warning.reference.as_deref(),
        &warning.detail,
    )
}

/// A header's ` warnings=N warning-records=M`, or empty without warning
/// records.
pub fn warning_atoms(warning_count: usize, warning_records: &[String]) -> String {
    if warning_records.is_empty() {
        String::new()
    } else {
        format!(
            " warnings={warning_count} warning-records={}",
            warning_records.len()
        )
    }
}

/// The part of `budget` an output's other records share: warning records
/// print whole, whatever the budget.
pub fn budget_beside_warnings(budget: Option<usize>, warning_records: &[String]) -> Option<usize> {
    let warning_chars = warning_records
        .iter()
        .map(|record| record.chars().count())
        .sum();
    budget.map(|budget| budget.saturating_sub(warning_chars))
}

/// The records of `warnings`, by kind, and how many warnings they report.
/// Within a kind, each warning without a reference prints its own record,
/// then those with one print as one record, a `count=` record for two
/// references or more. A warning repeated for one reference, or repeated
/// whole without one, counts once.
pub fn format_warning_records(warnings: &[AgentWarning]) -> (usize, Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    let mut by_kind = std::collections::BTreeMap::<
        AgentWarningKind,
        (Vec<&AgentWarning>, Vec<&AgentWarning>),
    >::new();
    for warning in warnings {
        let detail_without_reference = warning
            .reference
            .is_none()
            .then_some(warning.detail.as_str());
        if !seen.insert((
            warning.kind,
            warning.reference.as_deref(),
            detail_without_reference,
        )) {
            continue;
        }
        let (without_reference, with_reference) = by_kind.entry(warning.kind).or_default();
        match warning.reference {
            None => without_reference.push(warning),
            Some(_) => with_reference.push(warning),
        }
    }

    let total = by_kind
        .values()
        .map(|(without_reference, with_reference)| without_reference.len() + with_reference.len())
        .sum();
    let records = by_kind
        .into_iter()
        .flat_map(|(kind, (without_reference, with_reference))| {
            let referenced_record = match with_reference.as_slice() {
                [] => None,
                [warning] => Some(format_warning(warning)),
                several => Some(format!(
                    "protocol agent-warning kind={} count={}\n",
                    kind.as_str(),
                    several.len()
                )),
            };
            without_reference
                .into_iter()
                .map(format_warning)
                .chain(referenced_record)
        })
        .collect();
    (total, records)
}

fn format_record(protocol: &str, kind: &str, reference: Option<&str>, detail: &str) -> String {
    let mut output = format!("protocol {protocol} kind={kind}");
    if let Some(reference) = reference {
        output.push_str(" ref=");
        output.push_str(&escape_atom(&sanitize_agent_text(reference)));
    }
    if !detail.is_empty() {
        output.push_str(" detail=");
        output.push_str(&encode_detail(detail));
    }
    output.push('\n');
    output
}

/// `detail`, free text, as one field's value of a protocol or `perf` record.
pub(crate) fn encode_detail(detail: &str) -> String {
    escape_atom(&sanitize_agent_text(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_sanitizes_control_sequences() {
        let error = AgentError::new(
            AgentErrorKind::AmbiguousRef,
            Some("ch_12345678\u{1b}[31m"),
            "two\nchoices\u{1b}[2J",
        );

        assert_eq!(
            format_error(&error),
            "protocol agent-error kind=ambiguous-ref ref=ch_12345678 detail=two%0Achoices\n"
        );
    }

    #[test]
    fn every_error_kind_has_stable_atom() {
        let cases = [
            (AgentErrorKind::InvalidRef, "invalid-ref"),
            (AgentErrorKind::AmbiguousRef, "ambiguous-ref"),
            (AgentErrorKind::NotFound, "not-found"),
            (AgentErrorKind::OutOfRange, "out-of-range"),
            (AgentErrorKind::BudgetTooSmall, "budget-too-small"),
            (AgentErrorKind::MalformedTranscript, "malformed-transcript"),
            (AgentErrorKind::Io, "io"),
            (AgentErrorKind::SemanticUnavailable, "semantic-unavailable"),
        ];
        for (kind, atom) in cases {
            assert_eq!(
                format_error(&AgentError::new(kind, None, "detail")),
                format!("protocol agent-error kind={atom} detail=detail\n")
            );
        }
    }

    #[test]
    fn warning_envelope_is_stable() {
        let warning = AgentWarning::skipped(Some("ch_12345678"), "empty transcript");
        assert_eq!(
            format_warning(&warning),
            "protocol agent-warning kind=skipped ref=ch_12345678 detail=empty%20transcript\n"
        );
    }

    #[test]
    fn ignored_warning_renders_its_detail_without_ref() {
        assert_eq!(
            format_warning(&AgentWarning::ignored(
                "Codex: 1283 ignored: compressed sessions unsupported"
            )),
            "protocol agent-warning kind=ignored detail=Codex:%201283%20ignored:%20compressed%20sessions%20unsupported\n"
        );
    }

    #[test]
    fn partial_index_warning_renders_its_counts_without_ref() {
        assert_eq!(
            format_warning(&AgentWarning::partial_index(
                "475 of 6529 passages missing from the semantic index; run rearview --generate-semantic-cache"
            )),
            "protocol agent-warning kind=partial-index detail=475%20of%206529%20passages%20missing%20from%20the%20semantic%20index%3B%20run%20rearview%20--generate-semantic-cache\n"
        );
    }

    #[test]
    fn repeated_warning_kinds_are_summarized() {
        let warnings = vec![
            AgentWarning {
                kind: AgentWarningKind::MalformedTranscript,
                reference: Some("ch_a".to_string()),
                detail: "bad line 1".to_string(),
            },
            AgentWarning {
                kind: AgentWarningKind::MalformedTranscript,
                reference: Some("ch_b".to_string()),
                detail: "bad line 2".to_string(),
            },
            AgentWarning::skipped(Some("ch_c"), "empty transcript"),
        ];

        let (total, records) = format_warning_records(&warnings);

        assert_eq!(total, 3);
        assert_eq!(
            records,
            vec![
                "protocol agent-warning kind=skipped ref=ch_c detail=empty%20transcript\n",
                "protocol agent-warning kind=malformed-transcript count=2\n",
            ]
        );
    }

    /// The header atoms and the records of `warnings`.
    fn header_atoms_and_records(warnings: &[AgentWarning]) -> (String, Vec<String>) {
        let (total, records) = format_warning_records(warnings);
        (warning_atoms(total, &records), records)
    }

    fn io_warning(reference: Option<&str>, detail: &str) -> AgentWarning {
        AgentWarning::from_error(&AgentError::io(reference, detail))
    }

    const UNLISTED_AGENT_RECORD: &str =
        "protocol agent-warning kind=io detail=failed%20to%20list%20Codex%20sessions\n";

    #[test]
    fn warning_without_ref_prints_before_warning_of_its_kind_with_ref() {
        let (atoms, records) = header_atoms_and_records(&[
            io_warning(Some("ch_a"), "failed to read transcript"),
            io_warning(None, "failed to list Codex sessions"),
        ]);

        assert_eq!(atoms, " warnings=2 warning-records=2");
        assert_eq!(
            records,
            [
                UNLISTED_AGENT_RECORD,
                "protocol agent-warning kind=io ref=ch_a detail=failed%20to%20read%20transcript\n",
            ]
        );
    }

    #[test]
    fn warning_without_ref_prints_before_count_of_its_kind_with_refs() {
        let (atoms, records) = header_atoms_and_records(&[
            io_warning(Some("ch_a"), "failed to read transcript"),
            io_warning(None, "failed to list Codex sessions"),
            io_warning(Some("ch_b"), "failed to read transcript"),
        ]);

        assert_eq!(atoms, " warnings=3 warning-records=2");
        assert_eq!(
            records,
            [
                UNLISTED_AGENT_RECORD,
                "protocol agent-warning kind=io count=2\n"
            ]
        );
    }

    #[test]
    fn warnings_of_one_kind_without_ref_each_print_their_detail() {
        let warnings = [
            AgentWarning::ignored("Codex: 3 ignored: compressed sessions unsupported"),
            AgentWarning::ignored("Kimi Code: 2 ignored: session list unreadable"),
            AgentWarning::ignored("Codex: 3 ignored: compressed sessions unsupported"),
        ];

        let (total, records) = format_warning_records(&warnings);

        assert_eq!(total, 2, "the repeated warning counts once");
        assert_eq!(
            records,
            vec![format_warning(&warnings[0]), format_warning(&warnings[1]),]
        );
    }
}
