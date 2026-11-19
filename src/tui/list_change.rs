//! Renames and deletes the user confirmed, made on the background worker as
//! the refresh owner of the session cache: the providers read the
//! transcripts they check before they rename or delete them.

use crate::clock::{Clock, Moment};
use crate::error::{AppError, Result};
use crate::history::provider::{Deleted, Operation, RenamedTitle};
use crate::history::{OperationScope, Source};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A rename or delete waiting for the background worker.
pub(crate) enum ListChange {
    Rename {
        source: Source,
        path: PathBuf,
        title: String,
        renamed_at: DateTime<Utc>,
    },
    Delete {
        source: Source,
        path: PathBuf,
        /// The title the delete prompt showed.
        title: String,
        /// When the user confirmed the delete with `y`.
        confirmed_at: Moment,
    },
}

/// The result of a [`ListChange`] on the session at `path`.
pub(crate) enum ListChangeOutcome {
    Renamed {
        path: PathBuf,
        title: String,
        result: Result<RenamedTitle>,
    },
    Deleted {
        path: PathBuf,
        /// The title the delete prompt showed.
        title: String,
        result: Result<Deleted>,
        stages: DeleteStages,
    },
}

/// The report of a rename that failed with `error`, in the bar and on stderr.
pub(crate) fn rename_failure(error: &AppError) -> String {
    format!("Failed to rename: {error}")
}

/// A rename or a delete, without its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Rename,
    Delete,
}

impl ListChangeOutcome {
    pub(crate) fn kind(&self) -> ChangeKind {
        match self {
            Self::Renamed { .. } => ChangeKind::Rename,
            Self::Deleted { .. } => ChangeKind::Delete,
        }
    }

    pub(crate) fn succeeded(&self) -> bool {
        match self {
            Self::Renamed { result, .. } => result.is_ok(),
            Self::Deleted { result, .. } => result.is_ok(),
        }
    }

    /// The line rearview prints on stderr after leaving the TUI for a rename
    /// or delete that failed; `None` when it succeeded.
    pub(crate) fn failure_report(&self) -> Option<String> {
        match self {
            Self::Deleted {
                title,
                result: Err(error),
                ..
            } => Some(format!("Failed to delete \"{title}\": {error}")),
            Self::Renamed {
                result: Err(error), ..
            } => Some(rename_failure(error)),
            Self::Deleted { result: Ok(_), .. } | Self::Renamed { result: Ok(_), .. } => None,
        }
    }
}

/// The time a delete spent in the queue, then in each stage on the
/// background worker.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeleteStages {
    /// From the user's `y` until the worker began the delete, behind the
    /// worker's running job and the changes confirmed before it.
    pub(crate) queue: Duration,
    /// Acquiring refresh ownership of the session cache, waiting for any
    /// other holder.
    pub(crate) ownership: Duration,
    /// Opening the write batch, taking SQLite's writer lock and committing:
    /// the batch's time outside the provider.
    pub(crate) write_batch: Duration,
    /// The provider's checks, reads and removals.
    pub(crate) provider: Duration,
}

impl ListChange {
    pub(crate) fn kind(&self) -> ChangeKind {
        match self {
            Self::Rename { .. } => ChangeKind::Rename,
            Self::Delete { .. } => ChangeKind::Delete,
        }
    }

    /// Make the change as `operation`'s refresh owner. Fails when acquiring
    /// the ownership fails. A delete times its stages on `clock`.
    pub(crate) fn make(self, operation: &OperationScope, clock: &dyn Clock) -> ListChangeOutcome {
        match self {
            Self::Rename {
                source,
                path,
                title,
                renamed_at,
            } => {
                let provider = source.provider();
                let result = operation
                    .wait_to_parse_as_owner(|permit| {
                        provider
                            .rename()
                            .or_refuse(provider.labels().display, Operation::Rename)?
                            .rename_session(permit, &path, &title, renamed_at)
                    })
                    .and_then(|renamed| renamed);
                ListChangeOutcome::Renamed {
                    path,
                    title,
                    result,
                }
            }
            Self::Delete {
                source,
                path,
                title,
                confirmed_at,
            } => {
                let (result, stages) =
                    delete_with_stages(operation, clock, source, &path, confirmed_at);
                ListChangeOutcome::Deleted {
                    path,
                    title,
                    result,
                    stages,
                }
            }
        }
    }

    /// The outcome of this change when its job stops without one.
    pub(crate) fn stopped(&self) -> ListChangeOutcome {
        match self {
            Self::Rename { path, title, .. } => ListChangeOutcome::Renamed {
                path: path.clone(),
                title: title.clone(),
                result: Err(AppError::RefreshStopped),
            },
            Self::Delete { path, title, .. } => ListChangeOutcome::Deleted {
                path: path.clone(),
                title: title.clone(),
                result: Err(AppError::RefreshStopped),
                stages: DeleteStages::default(),
            },
        }
    }
}

/// Delete `source`'s session at `path`, confirmed at `confirmed_at`, as
/// `operation`'s refresh owner, and time each stage on `clock`. Fails when
/// acquiring the ownership fails.
fn delete_with_stages(
    operation: &OperationScope,
    clock: &dyn Clock,
    source: Source,
    path: &Path,
    confirmed_at: Moment,
) -> (Result<Deleted>, DeleteStages) {
    let started_at = clock.now();
    let acquired = operation.acquire_access().map(drop);
    let owned_at = clock.now();
    let mut provider_time = Duration::ZERO;
    let result = acquired
        .and_then(|()| {
            operation.wait_to_parse_as_owner(|permit| {
                let provider_started_at = clock.now();
                let deleted = crate::history::provider::delete_session(permit, source, path);
                provider_time = clock.elapsed_since(provider_started_at);
                deleted
            })
        })
        .and_then(|deleted| deleted);
    let stages = DeleteStages {
        queue: started_at.saturating_duration_since(confirmed_at),
        ownership: owned_at.saturating_duration_since(started_at),
        write_batch: clock.elapsed_since(owned_at).saturating_sub(provider_time),
        provider: provider_time,
    };
    (result, stages)
}
