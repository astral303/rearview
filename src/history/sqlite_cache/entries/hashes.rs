//! One hash per entry of a stored copy, over the values the viewer's read
//! returns for it. Two reads of a session compare entry by entry without
//! decoding either.

use super::digest::Digest;
use super::{path_text, threads};
use crate::error::Result;
use crate::history::sqlite_cache::roots::Places;
use rusqlite::Connection;
use std::collections::HashMap;

/// One hash per entry of a copy, in entry order. An entry's hash combines
/// its row's `digest`, its message's `digest`, the values the viewer's read
/// returns of its message's thread, and the path of the transcript its
/// record names. It leaves out the message's number and whether the entry
/// contributes to it: the viewer's read doesn't return either.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EntryHashes(Vec<i64>);

impl EntryHashes {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// The index of the first entry that differs in `newer`: the shorter
    /// list's length when one list extends the other, `None` when the lists
    /// are equal.
    pub(crate) fn first_change_in(&self, newer: &Self) -> Option<usize> {
        self.0
            .iter()
            .zip(&newer.0)
            .position(|(shown, new)| shown != new)
            .or_else(|| (self.0.len() != newer.0.len()).then(|| self.0.len().min(newer.0.len())))
    }
}

/// The hashes of `session`'s entries, their transcripts at their paths in
/// `places`.
pub(in super::super) fn read(
    connection: &Connection,
    session: i64,
    places: &Places,
) -> Result<EntryHashes> {
    let stored = threads::read(connection, session, places)?;
    let thread_hashes: HashMap<i64, i64> = stored
        .threads
        .iter()
        .map(|(id, thread)| (*id, thread.digest()))
        .collect();
    let hashes = connection
        .prepare_cached(
            "SELECT e.digest, m.digest, m.thread, e.record_transcript FROM entries e \
             LEFT JOIN messages m ON m.id = e.message WHERE e.session = ?1 ORDER BY e.sequence",
        )?
        .query_map([session], |row| {
            let mut digest = Digest::default();
            digest.number(Some(row.get(0)?));
            digest.number(row.get(1)?);
            digest.number(
                row.get::<_, Option<i64>>(2)?
                    .map(|thread| thread_hashes[&thread]),
            );
            let record_transcript = row.get::<_, Option<i64>>(3)?;
            digest.text(
                record_transcript
                    .map(|transcript| path_text(&stored.transcripts[&transcript]))
                    .as_deref(),
            );
            Ok(digest.finish())
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(EntryHashes(hashes))
}

#[cfg(test)]
mod tests {
    use super::EntryHashes;

    #[test]
    fn first_change_is_first_differing_entry_or_end_of_shorter_list() {
        let shown = EntryHashes(vec![1, 2, 3]);

        assert_eq!(
            shown.first_change_in(&EntryHashes(vec![1, 2, 3])),
            None,
            "equal lists don't have a change"
        );
        assert_eq!(
            shown.first_change_in(&EntryHashes(vec![1, 5, 3])),
            Some(1),
            "the change is at the first entry that differs"
        );
        assert_eq!(
            shown.first_change_in(&EntryHashes(vec![1, 2, 3, 4])),
            Some(3),
            "an appended entry is the change"
        );
        assert_eq!(
            shown.first_change_in(&EntryHashes(vec![1, 2])),
            Some(2),
            "a removed last entry is the change, at the shorter list's end"
        );
        assert_eq!(
            EntryHashes::default().first_change_in(&shown),
            Some(0),
            "every entry of a list after an empty one is the change"
        );
    }
}
