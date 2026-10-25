//! A file the user opened directly (`rearview <file>`), read on the TUI's
//! background worker: a session inside a provider's session roots is read
//! as the refresh owner and cached; any other file is parsed and not cached.

use super::SessionStorage;
use super::load::cache_sessions;
use super::outcome::SessionOutcome;
use crate::error::{AppError, Result};
use crate::history::load_meter::AgentMeter;
use crate::history::parser::{RowDating, conversation_from_projection};
use crate::history::{
    Conversation, DisplayEntries, Environment, OperationScope, Source, TranscriptEntries,
    format_short_name_from_path,
};
use std::path::Path;

/// What reading a file opened directly found.
pub enum DirectFileRead {
    /// A session inside `source`'s session roots, read as the refresh owner
    /// and cached.
    Session {
        source: Source,
        outcome: SessionOutcome,
    },
    /// A file outside every provider's session roots, read by the format of
    /// `source`, which recognises it, and not cached: its row, `None` when
    /// it holds no conversation, and its entries without sub-agents.
    Bare {
        source: Source,
        row: Option<Box<Conversation>>,
        entries: DisplayEntries,
    },
}

/// The file at `path` read under `operation`, with the preview `show_last`
/// selects. A session inside more than one provider's roots is read by the
/// first provider that lists it, else the first that finds it. Fails with
/// [`AppError::UnrecognizedTranscript`] for a file outside every root that
/// no format recognises.
pub fn open_direct_file(
    environment: &Environment,
    path: &Path,
    show_last: bool,
    operation: &OperationScope,
) -> Result<DirectFileRead> {
    let mut found = None;
    for storage in storages_holding(environment, path) {
        let source = storage.source();
        let Some(outcome) = cache_sessions(
            environment,
            source,
            &[path],
            show_last,
            operation,
            AgentMeter::UNMETERED,
        )?
        .pop() else {
            continue;
        };
        if outcome.is_listed() {
            return Ok(DirectFileRead::Session { source, outcome });
        }
        if !outcome.is_missing() && found.is_none() {
            found = Some(DirectFileRead::Session { source, outcome });
        }
    }
    match found {
        Some(read) => Ok(read),
        None => read_outside_every_root(environment, path),
    }
}

/// The storages with a session root holding the file at `path`.
fn storages_holding<'a>(
    environment: &'a Environment,
    path: &Path,
) -> impl Iterator<Item = &'a dyn SessionStorage> {
    let file = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    environment
        .storages()
        .iter()
        .copied()
        .filter(move |storage| {
            storage.roots().is_ok_and(|roots| {
                roots.iter().any(|root| {
                    root.path
                        .canonicalize()
                        .is_ok_and(|root| file.starts_with(root))
                })
            })
        })
}

/// A direct file every provider's session roots left out, which its read
/// parses: only [`open_direct_file`] makes one, after every root missed.
pub(crate) struct OutsideEveryRoot(());

/// The file at `path`, outside every provider's session roots, parsed by the
/// first registered format that recognises it.
fn read_outside_every_root(environment: &Environment, path: &Path) -> Result<DirectFileRead> {
    let outside = OutsideEveryRoot(());
    let permit = crate::history::ParsePermit::outside_every_root(&outside);
    let projection = crate::history::format::parse_transcript(permit, path)?
        .ok_or_else(|| AppError::UnrecognizedTranscript(path.display().to_string()))?;
    let modified = std::fs::metadata(path)
        .and_then(|file| file.modified())
        .ok();
    let row = conversation_from_projection(
        path.to_path_buf(),
        &projection,
        RowDating::new(modified, environment.clock().as_ref()),
        None,
    )
    .map(|mut row| {
        let project_path = row.cwd.clone().unwrap_or_else(|| path.to_path_buf());
        row.project_name = Some(format_short_name_from_path(&project_path));
        Box::new(row)
    });
    Ok(DirectFileRead::Bare {
        source: projection.source,
        row,
        entries: TranscriptEntries::of_projection(projection).into(),
    })
}
