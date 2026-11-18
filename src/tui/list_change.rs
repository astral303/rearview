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
        result: Result<Deleted>,
        stages: DeleteStages,
    },
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
                confirmed_at,
            } => {
                let (result, stages) =
                    delete_with_stages(operation, clock, source, &path, confirmed_at);
                ListChangeOutcome::Deleted {
                    path,
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
            Self::Delete { path, .. } => ListChangeOutcome::Deleted {
                path: path.clone(),
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
