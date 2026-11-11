//! The `roots` table, and the paths stored below its roots. A stored file
//! names the root it sits under by the root's row and holds only its path
//! below it; the full path is this run's spelling of the root joined with
//! the stored part. No spelling of a root is stored.

use super::files::{FileKey, RootKey};
use crate::error::{AppError, Result};
use crate::history::path::{FileIdentity, PathBelowRoot};
use crate::history::provider::SessionRoot;
use crate::history::provider::walk::Canonicaliser;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// A session root and the directories beside it, each in this run's
/// spelling with the key the `roots` table stores it by. The root sessions
/// list under comes first; positions count the others after it, in the
/// order [`SessionRoot::directories_beside`] gives them. A position names a
/// root within one run's record only: nothing stored names a root by it.
#[derive(Clone, Debug)]
pub(crate) struct SessionRoots(Arc<[RootSpelling]>);

#[derive(Debug)]
struct RootSpelling {
    /// `None` for a directory that does not canonicalise, such as one the
    /// agent has not created yet: the cache does not store anything below
    /// it, and a copy naming it reads as missing until a later read lists
    /// the session with the directory's key.
    key: Option<RootKey>,
    path: PathBuf,
}

impl SessionRoots {
    /// `root` and the directories beside it, each canonicalised through
    /// `canonicaliser` once per root.
    pub(crate) fn of(root: &SessionRoot, canonicaliser: &dyn Canonicaliser) -> Self {
        Self(
            std::iter::once(root)
                .chain(root.directories_beside())
                .map(|root| RootSpelling {
                    key: root
                        .canonical_path(canonicaliser)
                        .map(RootKey::of_canonical),
                    path: root.path.clone(),
                })
                .collect(),
        )
    }

    /// `root` and the directories beside it, canonicalised by the file
    /// system, for a test that does not count canonicalisations.
    #[cfg(test)]
    pub(crate) fn of_system(root: &SessionRoot) -> Self {
        Self::of(
            root,
            crate::history::provider::walk::SystemCanonicaliser::shared().as_ref(),
        )
    }

    /// The identity of the file at `path`, below one of these roots as
    /// spelled: that root's key joined with the path below it, without a
    /// file-system call. `None` for a path outside every one of them, or
    /// below one that doesn't canonicalise.
    pub(crate) fn file_identity(&self, path: &Path) -> Option<FileIdentity> {
        self.0.iter().find_map(|spelling| {
            let below = PathBelowRoot::of(path, &spelling.path)?;
            let key = spelling.key.as_ref()?;
            Some(FileIdentity::of_canonical(
                &Path::new(key.as_str()).join(below.as_str()),
            ))
        })
    }

    /// The key of the root sessions list under; `None` while its directory
    /// doesn't canonicalise.
    pub(crate) fn key(&self) -> Option<&RootKey> {
        self.0[0].key.as_ref()
    }

    /// This run's spelling of the root sessions list under.
    pub(crate) fn path(&self) -> &Path {
        &self.0[0].path
    }

    /// The place of `path`, a file of one of these roots' sessions: the
    /// position of the root it lies below and the path below it. Fails for
    /// a file outside every one of them, and for one below a root that
    /// doesn't canonicalise.
    pub(super) fn place(&self, session: &Path, path: &Path) -> Result<FilePlace> {
        let (root, spelling, below) = self
            .0
            .iter()
            .enumerate()
            .find_map(|(root, spelling)| {
                let below = PathBelowRoot::of(path, &spelling.path)?;
                Some((root, spelling, below))
            })
            .ok_or_else(|| AppError::SessionFileNotPlaced {
                session: session.to_path_buf(),
                file: path.to_path_buf(),
                reason: "is outside every root of the session",
            })?;
        if spelling.key.is_none() {
            return Err(AppError::SessionRootNotCanonicalised {
                session: session.to_path_buf(),
                root: spelling.path.clone(),
            });
        }
        Ok(FilePlace {
            root,
            path: below.into_string(),
        })
    }
}

impl PartialEq for SessionRoots {
    fn eq(&self, other: &Self) -> bool {
        match (self.key(), other.key()) {
            (Some(key), Some(other_key)) => key == other_key,
            (None, None) => self.path() == other.path(),
            _ => false,
        }
    }
}

impl Eq for SessionRoots {}

/// A file's place below its session's roots, as a record holds it: the
/// position of its root among [`SessionRoots`] and its path below that
/// root, with the platform's separator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FilePlace {
    pub(super) root: usize,
    pub(super) path: String,
}

/// A path as the tables store it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct StoredPath {
    /// The `roots` row of the root the file sits under.
    pub(super) root: i64,
    /// The path below `root`, with the platform's separator.
    pub(super) path: String,
}

impl StoredPath {
    /// The key a lookup compares the path below its root by.
    pub(super) fn key(&self) -> FileKey {
        FileKey::of_path_below_root(Path::new(&self.path))
    }
}

/// The `roots` rows of one session's roots, with this run's spelling of
/// each.
pub(super) struct Places {
    /// The row of the root sessions list under; `None` while the database
    /// does not hold it.
    listing: Option<i64>,
    /// This run's spelling of the root sessions list under.
    listing_path: PathBuf,
    /// The row and spelling of each root the database holds a row of.
    roots: Vec<(i64, PathBuf)>,
}

/// The [`Places`] of a session a write stores, with the stored path of each
/// of its files.
pub(super) struct WrittenPlaces {
    places: Places,
    /// The stored path of each file of the session.
    written: HashSet<StoredPath>,
    /// The session, for the failure of a file it does not hold.
    session: PathBuf,
}

impl std::ops::Deref for WrittenPlaces {
    type Target = Places;

    fn deref(&self) -> &Places {
        &self.places
    }
}

impl WrittenPlaces {
    /// The rows of the root sessions list under and of each root a file of
    /// `session` lies below, with the stored path of each of `files`, in
    /// order. Inserts each row the database does not hold. A directory
    /// beside the root gets a row only once a file is stored below it.
    pub(super) fn of(
        connection: &Connection,
        roots: &SessionRoots,
        session: &Path,
        files: &[&FilePlace],
    ) -> Result<(Self, Vec<StoredPath>)> {
        let mut insert = connection
            .prepare_cached("INSERT INTO roots (key) VALUES (?1) ON CONFLICT DO NOTHING")?;
        let mut select = connection.prepare_cached("SELECT id FROM roots WHERE key = ?1")?;
        let mut rows: Vec<Option<i64>> = vec![None; roots.0.len()];
        let used = std::iter::once(0).chain(files.iter().map(|place| place.root));
        for position in used {
            if rows[position].is_some() {
                continue;
            }
            let Some(key) = &roots.0[position].key else {
                return Err(AppError::SessionRootNotCanonicalised {
                    session: session.to_path_buf(),
                    root: roots.0[position].path.clone(),
                });
            };
            insert.execute([key.as_str()])?;
            rows[position] = Some(select.query_row([key.as_str()], |row| row.get(0))?);
        }
        let stored: Vec<StoredPath> = files
            .iter()
            .map(|place| StoredPath {
                root: rows[place.root].expect("each placed file's root has its row"),
                path: place.path.clone(),
            })
            .collect();
        let places = Self {
            places: Places {
                listing: rows[0],
                listing_path: roots.path().to_path_buf(),
                roots: rows
                    .iter()
                    .zip(roots.0.iter())
                    .filter_map(|(row, spelling)| Some(((*row)?, spelling.path.clone())))
                    .collect(),
            },
            written: stored.iter().cloned().collect(),
            session: session.to_path_buf(),
        };
        Ok((places, stored))
    }

    /// The row of the root sessions list under. [`Self::of`] inserted it
    /// when the database didn't hold it.
    pub(super) fn written_listing_root(&self) -> i64 {
        self.listing
            .expect("a written root has its row in the database")
    }

    /// The stored path of `path`, a file of the session a write stores, as
    /// its record placed that file. Fails for a path that is not one of the
    /// session's files.
    pub(super) fn place_written(&self, path: &Path) -> Result<StoredPath> {
        self.place(path)
            .filter(|place| self.written.contains(place))
            .ok_or_else(|| AppError::SessionFileNotPlaced {
                session: self.session.clone(),
                file: path.to_path_buf(),
                reason: "is not one of the session's files",
            })
    }
}

impl Places {
    /// The rows of `roots` the database holds. A root without a row, and one
    /// that doesn't canonicalise, aren't included.
    pub(super) fn stored(connection: &Connection, roots: &SessionRoots) -> Result<Self> {
        let mut select = connection.prepare_cached("SELECT id FROM roots WHERE key = ?1")?;
        let mut places = Self {
            listing: None,
            listing_path: roots.path().to_path_buf(),
            roots: Vec::with_capacity(roots.0.len()),
        };
        for (index, root) in roots.0.iter().enumerate() {
            let Some(key) = &root.key else {
                continue;
            };
            let Some(id) = select
                .query_row([key.as_str()], |row| row.get::<_, i64>(0))
                .optional()?
            else {
                continue;
            };
            if index == 0 {
                places.listing = Some(id);
            }
            places.roots.push((id, root.path.clone()));
        }
        Ok(places)
    }

    /// The row of the root sessions list under; `None` while the database
    /// does not hold that row or any session under the root.
    pub(super) fn listing_root(&self) -> Option<i64> {
        self.listing
    }

    /// `path`'s place below the first of the roots it sits under, as this
    /// run spells them; `None` outside all of them, or under a root without
    /// a row in the database.
    pub(super) fn place(&self, path: &Path) -> Option<StoredPath> {
        self.roots.iter().find_map(|(id, root)| {
            Some(StoredPath {
                root: *id,
                path: PathBelowRoot::of(path, root)?.into_string(),
            })
        })
    }

    /// The name the saved state of a resumed read gives the transcript at
    /// `path`: the `roots` row of the root it sits under, then its path
    /// below that root. [`Self::path_saved_as`] reads it back whichever way
    /// a later run spells the roots. `None` outside every root with a row.
    pub(super) fn saved_name(&self, path: &Path) -> Option<PathBuf> {
        let stored = self.place(path)?;
        Some(Path::new(&stored.root.to_string()).join(stored.path))
    }

    /// The full path, in this run's spelling, of the transcript
    /// [`Self::saved_name`] named `name`; `None` when none of this run's
    /// roots has the row the name starts with, such as a root whose
    /// canonical path changed since.
    pub(super) fn path_saved_as(&self, name: &Path) -> Option<PathBuf> {
        let mut components = name.components();
        let Some(Component::Normal(row)) = components.next() else {
            return None;
        };
        let root: i64 = row.to_str()?.parse().ok()?;
        let below = components.as_path();
        let is_below_the_root = below.components().next().is_some()
            && below
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !is_below_the_root {
            return None;
        }
        let (_, spelling) = self.roots.iter().find(|(id, _)| *id == root)?;
        Some(spelling.join(below))
    }

    /// The full path of `stored`: this run's spelling of its root joined
    /// with the part below it. Fails with [`AppError::StoredRootUnknown`]
    /// for a file below a root these places do not have, such as a
    /// directory beside the root that did not canonicalise when this run
    /// made its roots, or canonicalises to another path now.
    pub(super) fn path(&self, stored: StoredPath) -> Result<PathBuf> {
        match self.roots.iter().find(|(id, _)| *id == stored.root) {
            Some((_, spelling)) => Ok(spelling.join(stored.path)),
            None => Err(AppError::StoredRootUnknown {
                listing_root: self.listing_path.display().to_string(),
                root: stored.root,
                file: stored.path,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript's saved name reads back as the same file when a later
    /// run lists the directories beside the root in another order, spells
    /// the root through `..`, or no longer has the directory.
    #[test]
    fn saved_transcript_name_reads_back_whichever_order_or_spelling_the_roots_have() {
        let directory = tempfile::tempdir().unwrap();
        let [root, first, second] = ["root", "first", "second"].map(|name| {
            let path = directory.path().join(name);
            std::fs::create_dir(&path).unwrap();
            path
        });
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE roots (id INTEGER PRIMARY KEY, key TEXT UNIQUE)")
            .unwrap();
        let listed = SessionRoot::new(&root)
            .with_directory_beside(&first)
            .with_directory_beside(&second);
        let session = root.join("session.jsonl");
        let transcript = second.join("agent.jsonl");
        let saved = SessionRoots::of_system(&listed);
        let placed = [
            &saved.place(&session, &root.join("x")).unwrap(),
            &saved.place(&session, &first.join("x")).unwrap(),
            &saved.place(&session, &transcript).unwrap(),
        ];
        let (written, _) = WrittenPlaces::of(&connection, &saved, &session, &placed).unwrap();
        let name = written.saved_name(&transcript).unwrap();

        let read_back = |later: &SessionRoot| {
            Places::stored(&connection, &SessionRoots::of_system(later))
                .unwrap()
                .path_saved_as(&name)
        };

        assert_eq!(read_back(&listed), Some(transcript.clone()));
        let reordered = SessionRoot::new(&root)
            .with_directory_beside(&second)
            .with_directory_beside(&first);
        assert_eq!(read_back(&reordered), Some(transcript.clone()));
        let respelled = SessionRoot::new(first.join("..").join("root"))
            .with_directory_beside(&first)
            .with_directory_beside(&second);
        assert_eq!(read_back(&respelled), Some(transcript.clone()));
        let without_first = SessionRoot::new(&root).with_directory_beside(&second);
        assert_eq!(read_back(&without_first), Some(transcript));
        assert_eq!(
            read_back(&SessionRoot::new(&root).with_directory_beside(&first)),
            None,
            "the name of a file under a root the later run lacks names nothing"
        );
    }

    #[test]
    fn file_below_root_that_doesnt_canonicalise_fails_naming_that_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = SessionRoot::new(directory.path().join("root"));
        std::fs::create_dir(&root.path).unwrap();
        let missing = directory.path().join("missing");
        let session = root.path.join("session.jsonl");
        let below_missing = missing.join("agent.jsonl");
        let roots = SessionRoots::of_system(&root.clone().with_directory_beside(&missing));

        let failure = roots.place(&session, &below_missing).unwrap_err();

        assert!(
            matches!(&failure, AppError::SessionRootNotCanonicalised { root, .. } if *root == missing),
            "the failure names the root that doesn't canonicalise: {failure}"
        );
    }

    #[test]
    fn stored_file_below_root_this_run_lacks_fails_as_changed_source_naming_root_and_row() {
        let directory = tempfile::tempdir().unwrap();
        let root = SessionRoot::new(directory.path().join("root"));
        std::fs::create_dir(&root.path).unwrap();
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE roots (id INTEGER PRIMARY KEY, key TEXT UNIQUE)")
            .unwrap();
        let places = Places::stored(&connection, &SessionRoots::of_system(&root)).unwrap();

        let failure = places
            .path(StoredPath {
                root: 42,
                path: "agent.jsonl".to_owned(),
            })
            .unwrap_err();

        let message = failure.to_string();
        assert!(
            failure.is_changed_source()
                && message.contains(&root.path.display().to_string())
                && message.contains("`roots` row 42")
                && message.contains("agent.jsonl"),
            "the failure names the root, the `roots` row and the file: {message}"
        );
    }

    #[test]
    fn file_outside_every_root_fails_naming_its_session_and_itself() {
        let directory = tempfile::tempdir().unwrap();
        let root = SessionRoot::new(directory.path().join("root"));
        std::fs::create_dir(&root.path).unwrap();
        let session = root.path.join("session.jsonl");
        let outside = directory.path().join("outside").join("agent.jsonl");

        let failure = SessionRoots::of_system(&root)
            .place(&session, &outside)
            .unwrap_err()
            .to_string();

        assert!(
            failure.contains(&session.display().to_string())
                && failure.contains(&outside.display().to_string()),
            "the failure names the session and the file: {failure}"
        );
    }
}
