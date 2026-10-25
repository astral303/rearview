//! The records a reader drops, counted for the hidden dropped-records report
//! a maintainer runs while reviewing an agent's new releases.

use std::collections::BTreeMap;

/// Why a reader dropped a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropOutcome {
    /// A kind the reader names and drops on purpose.
    Skipped,
    /// A kind the reader does not name.
    Unrecognised,
    /// A record that did not parse.
    Failed,
}

impl DropOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Unrecognised => "unrecognised",
            Self::Failed => "failed",
        }
    }
}

/// The records one transcript's reader dropped, by the agent version each
/// record falls under, kind and outcome.
///
/// Counting is off unless the report asks for it: a reader handed the default
/// records nothing and allocates nothing, so the ordinary load pays one branch
/// per dropped record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DroppedRecords {
    counting: bool,
    /// The version later counts fall under: the last one the transcript stated.
    version: Option<String>,
    /// The first version the transcript stated.
    first_version: Option<String>,
    counts: BTreeMap<(Option<String>, String, DropOutcome), usize>,
}

impl DroppedRecords {
    pub fn counting() -> Self {
        Self {
            counting: true,
            ..Self::default()
        }
    }

    pub(crate) fn is_counting(&self) -> bool {
        self.counting
    }

    /// Count one record of a kind the reader names and drops: `field` holding
    /// `value`, or `(missing)` when the record has no such field.
    pub(crate) fn skipped(&mut self, field: &str, value: Option<&str>) {
        self.add(field, value, DropOutcome::Skipped);
    }

    /// Count one record of a kind the reader does not name: `field` holding
    /// `value`, or `(missing)` when the record has no such field.
    pub(crate) fn unrecognised(&mut self, field: &str, value: Option<&str>) {
        self.add(field, value, DropOutcome::Unrecognised);
    }

    /// Count one record of `kind` that did not parse.
    pub(crate) fn failed(&mut self, kind: &str) {
        if self.counting {
            self.count(kind.to_owned(), DropOutcome::Failed);
        }
    }

    fn add(&mut self, field: &str, value: Option<&str>, outcome: DropOutcome) {
        if self.counting {
            self.count(format!("{field}={}", value.unwrap_or("(missing)")), outcome);
        }
    }

    fn count(&mut self, kind: String, outcome: DropOutcome) {
        *self
            .counts
            .entry((self.version.clone(), kind, outcome))
            .or_default() += 1;
    }

    /// Count later records under the version `version` reads, while counting.
    /// A record that states none leaves the version stated before it.
    pub(crate) fn record_version(&mut self, version: impl FnOnce() -> Option<String>) {
        if !self.counting {
            return;
        }
        if let Some(version) = version() {
            self.first_version.get_or_insert_with(|| version.clone());
            self.version = Some(version);
        }
    }

    pub fn first_version(&self) -> Option<&str> {
        self.first_version.as_deref()
    }

    /// Every count with its version, kind and outcome, in version and kind
    /// order.
    pub fn counts(&self) -> impl Iterator<Item = (Option<&str>, &str, DropOutcome, usize)> {
        self.counts.iter().map(|((version, kind, outcome), count)| {
            (version.as_deref(), kind.as_str(), *outcome, *count)
        })
    }
}

/// The values of one field the reader drops on purpose; any other value counts
/// as unrecognised. When a release review decides against a kind, add it to
/// its field's list.
pub(crate) struct SkippedKinds {
    pub(crate) field: &'static str,
    pub(crate) values: &'static [&'static str],
}

impl SkippedKinds {
    pub(crate) fn count(&self, value: Option<&str>, dropped: &mut DroppedRecords) {
        if value.is_some_and(|value| self.values.contains(&value)) {
            dropped.skipped(self.field, value);
        } else {
            dropped.unrecognised(self.field, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_handed_default_count_nothing() {
        let mut dropped = DroppedRecords::default();

        dropped.record_version(|| Some("1.0.0".to_owned()));
        dropped.skipped("type", Some("plan"));
        dropped.unrecognised("type", None);
        dropped.failed("line");

        assert_eq!(dropped, DroppedRecords::default());
    }

    #[test]
    fn counting_records_add_up_by_version_kind_and_outcome() {
        let mut dropped = DroppedRecords::counting();

        dropped.unrecognised("type", None);
        dropped.record_version(|| Some("1.0.0".to_owned()));
        dropped.skipped("type", Some("plan"));
        dropped.skipped("type", Some("plan"));
        dropped.record_version(|| None);
        dropped.failed("line");
        dropped.record_version(|| Some("2.0.0".to_owned()));
        dropped.skipped("type", Some("plan"));

        assert_eq!(
            dropped.counts().collect::<Vec<_>>(),
            [
                (None, "type=(missing)", DropOutcome::Unrecognised, 1),
                (Some("1.0.0"), "line", DropOutcome::Failed, 1),
                (Some("1.0.0"), "type=plan", DropOutcome::Skipped, 2),
                (Some("2.0.0"), "type=plan", DropOutcome::Skipped, 1),
            ]
        );
        assert_eq!(dropped.first_version(), Some("1.0.0"));
    }
}
