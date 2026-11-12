//! Renames and deletes the user confirmed, made on the background worker as
//! the refresh owner of the session cache: the providers read the
//! transcripts they check before they rename or delete them.

use crate::error::{AppError, Result};
use crate::history::provider::{Deleted, Operation};
use crate::history::{OperationScope, Source};
use chrono::{DateTime, Utc};
use std::path::PathBuf;

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
    },
}

/// The result of a [`ListChange`] on the session at `path`.
pub(crate) enum ListChangeOutcome {
    Renamed {
        path: PathBuf,
        title: String,
        result: Result<()>,
    },
    Deleted {
        path: PathBuf,
        result: Result<Deleted>,
    },
}

impl ListChange {
    /// Make the change as `operation`'s refresh owner. Fails when acquiring
    /// the ownership fails.
    pub(crate) fn make(self, operation: &OperationScope) -> ListChangeOutcome {
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
            Self::Delete { source, path } => {
                let result = operation
                    .wait_to_parse_as_owner(|permit| {
                        crate::history::provider::delete_session(permit, source, &path)
                    })
                    .and_then(|deleted| deleted);
                ListChangeOutcome::Deleted { path, result }
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
            },
        }
    }
}
