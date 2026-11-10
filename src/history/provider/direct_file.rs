//! A file the user opened directly (`rearview <file>`), read on the TUI's
//! background worker: a session inside a provider's session roots is read
//! as the refresh owner and cached; any other file is parsed and not cached.

use super::SessionRoot;
use super::SessionStorage;
use super::load::cache_canonical_session;
use super::outcome::SessionOutcome;
use crate::error::{AppError, Result};
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
    /// it does not hold a conversation, and its entries without sub-agents.
    Bare {
        source: Source,
        row: Option<Box<Conversation>>,
        entries: DisplayEntries,
    },
}

/// The file at `path`, canonicalised by the caller, read under `operation`, with
/// the preview `show_last` selects. A session inside more than one
/// provider's roots is read by the first provider that lists it, else the
/// first that finds it. Fails with [`AppError::UnrecognizedTranscript`] for
/// a file outside every root that the formats do not recognise.
pub fn open_direct_file(
    environment: &Environment,
    path: &Path,
    show_last: bool,
    operation: &OperationScope,
) -> Result<DirectFileRead> {
    let mut found = None;
    for (storage, roots) in storages_holding(environment, path) {
        let source = storage.source();
        let Some(outcome) =
            cache_canonical_session(environment, source, roots, path, show_last, operation)?.pop()
        else {
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

/// The storages with a session root holding the file at `path`, which the
/// caller canonicalised (`DirectFile::canonical`), each with its roots: below
/// the root or a directory beside it as spelled, or below its canonical path.
/// The roots keep the canonical path that matched, so the read of the file
/// doesn't canonicalise them again.
fn storages_holding<'a>(
    environment: &'a Environment,
    path: &'a Path,
) -> impl Iterator<Item = (&'a dyn SessionStorage, Vec<SessionRoot>)> {
    let canonicaliser = environment.canonicaliser();
    let root_environment = environment.root_environment();
    environment
        .storages()
        .iter()
        .copied()
        .filter_map(move |storage| {
            let roots = storage.roots_in(&root_environment).ok()?;
            roots
                .iter()
                .any(|root| {
                    root.holds(path) || root.canonical_position_of(path, canonicaliser).is_some()
                })
                .then_some((storage, roots))
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
    })
    .ok();
    Ok(DirectFileRead::Bare {
        source: projection.source,
        row,
        entries: TranscriptEntries::of_projection_without_leading_titles(projection).into(),
    })
}
