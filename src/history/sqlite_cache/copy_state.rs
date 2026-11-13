//! A session row's state for its readers, decided here: [`RowState::of`]
//! classifies a row, [`RowState::at`] reads the row at one session's
//! address, and [`LISTED_COPY`] and [`SETTLED_ROW`] are the same
//! classification as SQL conditions for queries over many rows. A read that
//! doesn't hold any data of a copy reports one [`CopyMiss`].

use super::files::RootKey;
use super::{SessionAddress, SessionStatus, catalogue, is_storage_current};
use crate::error::{AppError, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// The SQL condition on a `sessions` row that holds a listed copy with its
/// entries: [`RowState::Listed`].
pub(super) const LISTED_COPY: &str = "(status = 'listed' AND entries_error IS NULL)";

/// The SQL condition on a `sessions` row whose write ended with its entries
/// in place: a listed copy, or a session the load did not list (empty or
/// unreadable). A row being written and a listed row whose entries were kept
/// out are left out.
pub(super) const SETTLED_ROW: &str =
    "(status != 'writing' AND (status != 'listed' OR entries_error IS NULL))";

/// A stored session row: its id, and the generation its last write gave it.
pub(super) struct StoredSession {
    pub(super) id: i64,
    pub(super) generation: i64,
}

/// The state of a session's row, as its readers take it.
pub(super) enum RowState {
    /// A listed copy with its entries.
    Listed(StoredSession),
    /// Another write is replacing the row: its status is
    /// [`catalogue::WRITING`] until the write ends.
    BeingWritten,
    /// A listed copy whose entries the load kept out: a file changed after
    /// its stamp. The next load writes the session again.
    EntriesKeptOut,
    /// No row, or the row of a session the load did not list.
    NotThere,
}

impl RowState {
    /// The state of the row `stored`, of `status`, with `entries_error`.
    fn of(status: &str, entries_error: Option<String>, stored: StoredSession) -> Self {
        if status == catalogue::WRITING {
            return Self::BeingWritten;
        }
        if status != SessionStatus::Listed.as_str() {
            return Self::NotThere;
        }
        match entries_error {
            Some(_) => Self::EntriesKeptOut,
            None => Self::Listed(stored),
        }
    }

    /// The state of the row of the session at `address`, in `connection`'s
    /// snapshot. A storage written under another schema version doesn't hold
    /// a row.
    pub(super) fn at(connection: &Connection, address: &SessionAddress) -> Result<Self> {
        if !is_storage_current(connection, address.storage)? {
            return Ok(Self::NotThere);
        }
        Self::of_session(
            connection,
            address.storage.directory,
            address.roots.key(),
            &address.cache_key,
        )
    }

    /// The state of the row `storage` found at `cache_key` under the root of
    /// `root_key`.
    pub(super) fn of_session(
        connection: &Connection,
        storage: &str,
        root_key: Option<&RootKey>,
        cache_key: &str,
    ) -> Result<Self> {
        let row: Option<(i64, i64, String, Option<String>)> = connection
            .query_row(
                "SELECT id, generation, status, entries_error FROM sessions \
                 WHERE storage = ?1 AND cache_key = ?3 \
                 AND root = (SELECT id FROM roots WHERE key = ?2)",
                params![storage, root_key.map(RootKey::as_str), cache_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        Ok(match row {
            Some((id, generation, status, entries_error)) => {
                Self::of(&status, entries_error, StoredSession { id, generation })
            }
            None => Self::NotThere,
        })
    }

    /// The listed copy, or the miss of a reader of it.
    pub(super) fn listed(self) -> std::result::Result<StoredSession, CopyMiss> {
        match self {
            Self::Listed(stored) => Ok(stored),
            Self::BeingWritten | Self::EntriesKeptOut => Err(CopyMiss::NotReady),
            Self::NotThere => Err(CopyMiss::NotThere),
        }
    }
}

/// The listed copy `storage` holds at `cache_key` under the root of
/// `root_key`; `None` when none is there. Fails naming the session when its
/// copy is not ready.
#[cfg(test)]
pub(super) fn listed_session_of(
    connection: &Connection,
    storage: &str,
    root_key: Option<&RootKey>,
    cache_key: &str,
) -> Result<Option<StoredSession>> {
    match RowState::of_session(connection, storage, root_key, cache_key)? {
        RowState::Listed(stored) => Ok(Some(stored)),
        RowState::BeingWritten | RowState::EntriesKeptOut => Err(AppError::SessionCacheNotReady {
            session: cache_key.to_owned(),
            reason: "its cached copy is not ready yet".to_owned(),
        }),
        RowState::NotThere => Ok(None),
    }
}

/// A read of one copy of a session: its data, or the reason it doesn't hold
/// any.
pub(crate) type CopyRead<T> = std::result::Result<T, CopyMiss>;

/// The reason a read of a session's copy doesn't hold any data of it.
#[derive(Debug)]
pub(crate) enum CopyMiss {
    /// A listed copy of the session is not in the cache: none was written,
    /// another write removed it, or the session is no longer listed.
    NotThere,
    /// The session's copy is being written, or its entries were kept out; a
    /// later write lists it.
    NotReady,
    /// The read failed.
    ReadError(AppError),
}

impl From<AppError> for CopyMiss {
    fn from(error: AppError) -> Self {
        Self::ReadError(error)
    }
}

/// `read` as the viewer, the printout and the agent commands take it: `None`
/// for a copy that is not there or not ready, and a failed read as its
/// error. An agent command reports a `None` copy in its `stale-cache`
/// record.
pub(crate) fn found<T>(read: CopyRead<T>) -> Result<Option<T>> {
    match read {
        Ok(data) => Ok(Some(data)),
        Err(CopyMiss::NotThere | CopyMiss::NotReady) => Ok(None),
        Err(CopyMiss::ReadError(error)) => Err(error),
    }
}

/// `reads`, one per copy a command selected, unless every one failed: then
/// the first failure, as the command's error. A failure of every copy is the
/// cache's, not one session's.
pub(crate) fn unless_every_read_failed<T>(reads: Vec<CopyRead<T>>) -> Result<Vec<CopyRead<T>>> {
    let every_read_failed = !reads.is_empty()
        && reads
            .iter()
            .all(|read| matches!(read, Err(CopyMiss::ReadError(_))));
    if !every_read_failed {
        return Ok(reads);
    }
    match reads.into_iter().next() {
        Some(Err(CopyMiss::ReadError(error))) => Err(error),
        _ => unreachable!("every read failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each status a row takes, with and without an entries error.
    fn every_row() -> Vec<(&'static str, Option<&'static str>)> {
        ["listed", catalogue::WRITING, "empty", "unreadable"]
            .into_iter()
            .flat_map(|status| [(status, None), (status, Some("a file changed"))])
            .collect()
    }

    /// The cache keys of the rows `condition` selects, in key order.
    fn selected(connection: &Connection, condition: &str) -> Vec<String> {
        connection
            .prepare(&format!(
                "SELECT cache_key FROM sessions WHERE {condition} ORDER BY cache_key"
            ))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    #[test]
    fn sql_conditions_select_the_rows_the_classification_names() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(super::super::schema::CREATE)
            .unwrap();
        connection
            .execute("INSERT INTO roots (id, key) VALUES (1, 'root')", [])
            .unwrap();
        let mut listed = Vec::new();
        let mut settled = Vec::new();
        for (position, (status, entries_error)) in every_row().into_iter().enumerate() {
            let cache_key = format!("{position:02} {status} {entries_error:?}");
            connection
                .execute(
                    "INSERT INTO sessions (storage, source, root, cache_key, status, file_size, \
                     mtime_secs, mtime_nsecs, entries_error, generation) \
                     VALUES ('storage', 'claude', 1, ?1, ?2, 0, 0, 0, ?3, ?4)",
                    params![cache_key, status, entries_error, position as i64],
                )
                .unwrap();
            let state = RowState::of(
                status,
                entries_error.map(str::to_owned),
                StoredSession {
                    id: 0,
                    generation: 0,
                },
            );
            if matches!(state, RowState::Listed(_)) {
                listed.push(cache_key.clone());
            }
            if !matches!(state, RowState::BeingWritten | RowState::EntriesKeptOut) {
                settled.push(cache_key);
            }
        }

        assert_eq!(
            selected(&connection, LISTED_COPY),
            listed,
            "`LISTED_COPY` selects the rows classified as listed"
        );
        assert_eq!(
            selected(&connection, SETTLED_ROW),
            settled,
            "`SETTLED_ROW` selects every row but those being written or with entries kept out"
        );
    }

    #[test]
    fn every_read_failing_is_the_commands_failure_and_one_read_failing_is_its_copys() {
        let failed = || CopyMiss::ReadError(AppError::SessionCache(rusqlite::Error::InvalidQuery));

        let one_failed =
            unless_every_read_failed(vec![Ok(1), Err(failed()), Err(CopyMiss::NotReady)]).unwrap();
        let every_failed = unless_every_read_failed::<i32>(vec![Err(failed()), Err(failed())]);

        assert!(
            matches!(
                one_failed[..],
                [Ok(1), Err(CopyMiss::ReadError(_)), Err(CopyMiss::NotReady)]
            ),
            "a failure of one copy stays that copy's: {one_failed:?}"
        );
        assert!(
            matches!(every_failed, Err(AppError::SessionCache(_))),
            "a failure of every copy is the command's"
        );
    }
}
