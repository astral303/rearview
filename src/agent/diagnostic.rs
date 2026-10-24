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
    /// the detail holds the record's `refs=` and `cached-until=` atoms and
    /// the sentence for its cause.
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

    /// The sessions at `refs`, read from the cache as it was for `cause`.
    /// `cached_until` is the earliest end of their cached messages; `None`
    /// when the cache held none of them.
    pub fn stale_cache(
        refs: &[String],
        cached_until: Option<chrono::DateTime<crate::clock::Zone>>,
        cause: crate::history::StaleCause,
    ) -> Self {
        let mut detail = format!("refs={}", refs.join(","));
        if let Some(cached_until) = cached_until {
            detail.push_str(" cached-until=");
            detail.push_str(&cached_until.to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
        }
        detail.push_str(" : ");
        detail.push_str(match cause {
            crate::history::StaleCause::AnotherRearview => STALE_CACHE_SENTENCE,
            crate::history::StaleCause::WritesStopped => WRITES_STOPPED_SENTENCE,
        });
        Self {
            kind: AgentWarningKind::StaleCache,
            reference: None,
            detail,
        }
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

pub fn format_warning_records(warnings: &[AgentWarning]) -> (usize, Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    let mut grouped = std::collections::BTreeMap::<AgentWarningKind, Vec<&AgentWarning>>::new();
    for warning in warnings {
        if seen.insert((warning.kind, warning.reference.as_deref())) {
            grouped.entry(warning.kind).or_default().push(warning);
        }
    }

    let total = grouped.values().map(Vec::len).sum();
    let records = grouped
        .into_iter()
        .map(|(kind, warnings)| {
            if warnings.len() == 1 {
                format_warning(warnings[0])
            } else {
                format!(
                    "protocol agent-warning kind={} count={}\n",
                    kind.as_str(),
                    warnings.len()
                )
            }
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
        output.push_str(&escape_atom(&sanitize_agent_text(detail)));
    }
    output.push('\n');
    output
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
}
