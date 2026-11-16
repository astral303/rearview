//! The one-time import of the old file, `embeddings-v1.bin`: the whole-file
//! embedding cache earlier releases kept beside the store.

use super::{
    OpenPassageStore, PassageKey, PassageStore, SkipReason, UPSERT_STAMP, encoded_embedding,
};
use crate::semantic::embed::is_valid_embedding;
use crate::semantic::types::{ChunkConfig, MODEL_NAME};
use bincode::Options;
use rusqlite::OptionalExtension;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

const OLD_FILE_NAME: &str = "embeddings-v1.bin";
/// The last version of the old file's layout.
const OLD_FILE_SCHEMA_VERSION: u32 = 7;
/// An entry of the old file is a 64-character key, an embedding and nine
/// more bytes. A length over this limit comes from a damaged file, and is
/// not allocated.
const OLD_ENTRY_SIZE_LIMIT: u64 = 1 << 20;
const ROWS_PER_TRANSACTION: usize = 1_000;
/// Longer than the store's own: a second rearview importing the same file
/// waits for the first one's transactions of [`ROWS_PER_TRANSACTION`] rows.
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// A passage the store already holds keeps its row and its stamp.
const INSERT_IMPORTED_PASSAGE: &str = "INSERT INTO passages (key, embedding) VALUES (?1, ?2)
    ON CONFLICT (key) DO NOTHING RETURNING id";

impl PassageStore {
    fn old_file_path(&self) -> Option<PathBuf> {
        Some(self.path.as_deref()?.parent()?.join(OLD_FILE_NAME))
    }
}

impl OpenPassageStore<'_> {
    /// Import the old file beside the store, when there is one, and delete
    /// it once its last entry is committed. Delete a file of another
    /// version, model or chunking without an import. An import stopped by
    /// low space, a busy store or an error keeps the file, and the next open
    /// imports it again.
    pub(super) fn import_old_file(&mut self) {
        if self.connection.is_none() {
            return;
        }
        let Some(old_file) = self.store.old_file_path() else {
            return;
        };
        // Most opens end here: the old file is missing.
        let Ok(entries) = OldFileEntries::open(&old_file) else {
            return;
        };
        let imported = match entries {
            Some(entries) => {
                self.set_busy_timeout(self.store.import_busy_timeout);
                let imported = self.import(entries);
                self.set_busy_timeout(self.store.busy_timeout);
                imported
            }
            None => Ok(()),
        };
        if imported.is_ok() {
            // Another rearview importing the same file may have deleted it.
            let _ = std::fs::remove_file(&old_file);
        }
    }

    fn set_busy_timeout(&self, timeout: Duration) {
        if let Some(connection) = &self.connection {
            let _ = connection.busy_timeout(timeout);
        }
    }

    /// Store the valid `entries`, stamped today, skipping an entry whose
    /// embedding is not the store's dimension count of finite values.
    fn import(&mut self, entries: OldFileEntries<impl Read>) -> Result<(), SkipReason> {
        let (dimensions, today) = (self.store.dimensions, self.today);
        let mut valid_entries = entries
            .filter(|(_, embedding)| is_valid_embedding(embedding, dimensions))
            .peekable();
        while valid_entries.peek().is_some() {
            self.in_write_transaction(|transaction| {
                let mut insert_passage = transaction.prepare(INSERT_IMPORTED_PASSAGE)?;
                let mut upsert_stamp = transaction.prepare(UPSERT_STAMP)?;
                for (key, embedding) in valid_entries.by_ref().take(ROWS_PER_TRANSACTION) {
                    let inserted = insert_passage
                        .query_row((key.0.as_slice(), encoded_embedding(&embedding)), |row| {
                            row.get::<_, i64>(0)
                        })
                        .optional()?;
                    if let Some(id) = inserted {
                        upsert_stamp.execute((id, today))?;
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// The entries of an old file, decoded one at a time: the import holds one
/// entry, not the file. The entries end at the first one that does not
/// decode, as in a file cut short.
struct OldFileEntries<R> {
    reader: R,
    remaining: u64,
}

impl OldFileEntries<BufReader<File>> {
    /// The entries of the old file at `path`; `Ok(None)` when the file is of
    /// another version, model or chunking, or is shorter than its header.
    fn open(path: &Path) -> std::io::Result<Option<Self>> {
        Ok(Self::after_header(BufReader::new(File::open(path)?)))
    }
}

impl<R: Read> OldFileEntries<R> {
    fn after_header(mut reader: R) -> Option<Self> {
        let expected = old_file_header();
        let mut header = vec![0; expected.len()];
        reader.read_exact(&mut header).ok()?;
        if header != expected {
            return None;
        }
        let (_access_counter, remaining) = decoded::<(u64, u64)>(&mut reader)?;
        Some(Self { reader, remaining })
    }
}

impl<R: Read> Iterator for OldFileEntries<R> {
    type Item = (PassageKey, Vec<f32>);

    /// The next entry whose key is a passage's hash.
    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            self.remaining -= 1;
            let Some((key, embedding, _last_used, _protected)) =
                decoded::<(String, Vec<f32>, u64, bool)>(&mut self.reader)
            else {
                self.remaining = 0;
                return None;
            };
            if let Ok(hash) = blake3::Hash::from_hex(&key) {
                return Some((PassageKey(*hash.as_bytes()), embedding));
            }
        }
        None
    }
}

/// The bytes the old file opens with: its schema version, model and
/// chunking, as bincode wrote the leading fields of the cache.
fn old_file_header() -> Vec<u8> {
    let chunking = ChunkConfig::default();
    bincode::serialize(&(
        OLD_FILE_SCHEMA_VERSION,
        MODEL_NAME,
        chunking.target_chars,
        chunking.overlap_chars,
        chunking.context_turns,
    ))
    .expect("integers and a string serialise")
}

/// The next value of `reader`, in the encoding `bincode::serialize` wrote
/// the old file with.
fn decoded<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> Option<T> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(OLD_ENTRY_SIZE_LIMIT)
        .deserialize_from(reader)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::free_space::{FREE_SPACE_FLOOR, FixedFreeSpace};
    use crate::semantic::store::test_support::{
        store_passages, stored_embedding, stored_passage_count, temporary_store, temporary_store_at,
    };
    use rusqlite::Connection;
    use serde::Serialize;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// The old file's cache, with the fields and their order as the release
    /// that wrote the file serialised them.
    #[derive(Serialize)]
    struct OldCache {
        schema_version: u32,
        model: String,
        chunk_target_chars: usize,
        chunk_overlap_chars: usize,
        chunk_context_turns: usize,
        access_counter: u64,
        entries: HashMap<String, OldEntry>,
    }

    #[derive(Serialize)]
    struct OldEntry {
        embedding: Vec<f32>,
        last_used: u64,
        protected: bool,
    }

    /// The old cache's key of `text`: its hash in hexadecimal.
    fn old_key(text: &str) -> String {
        blake3::hash(text.as_bytes()).to_hex().to_string()
    }

    /// An old cache of this version, model and chunking with one entry per
    /// key.
    fn old_cache<K: Into<String>>(entries: impl IntoIterator<Item = (K, Vec<f32>)>) -> OldCache {
        let chunking = ChunkConfig::default();
        OldCache {
            schema_version: OLD_FILE_SCHEMA_VERSION,
            model: MODEL_NAME.to_string(),
            chunk_target_chars: chunking.target_chars,
            chunk_overlap_chars: chunking.overlap_chars,
            chunk_context_turns: chunking.context_turns,
            access_counter: 9,
            entries: entries
                .into_iter()
                .enumerate()
                .map(|(index, (key, embedding))| {
                    let entry = OldEntry {
                        embedding,
                        last_used: index as u64,
                        protected: index % 2 == 0,
                    };
                    (key.into(), entry)
                })
                .collect(),
        }
    }

    /// An old cache with `embedding` for each of `texts`.
    fn old_cache_of(texts: &[&str], embedding: [f32; 2]) -> OldCache {
        old_cache(texts.iter().map(|text| (old_key(text), embedding.to_vec())))
    }

    /// Write `bytes` as the old file beside `store`.
    fn write_old_file_bytes(store: &PassageStore, bytes: &[u8]) -> PathBuf {
        let path = store.old_file_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn write_old_file(store: &PassageStore, cache: &OldCache) -> PathBuf {
        write_old_file_bytes(store, &bincode::serialize(cache).unwrap())
    }

    fn empty_store() -> PassageStore {
        temporary_store(Arc::new(TestClock::default()))
    }

    fn raw_connection(store: &PassageStore) -> Connection {
        Connection::open(store.path().unwrap()).unwrap()
    }

    #[test]
    fn open_imports_the_old_files_embeddings_and_deletes_the_file() {
        let store = empty_store();
        let old_file = write_old_file(&store, &old_cache_of(&["alpha", "beta"], [0.5, 0.5]));

        let imported = (
            stored_embedding(&store, "alpha"),
            stored_embedding(&store, "beta"),
        );

        assert_eq!(
            imported,
            (Some(Arc::from([0.5, 0.5])), Some(Arc::from([0.5, 0.5]))),
            "each entry of the old file is read from the store"
        );
        assert!(!old_file.exists(), "the imported old file is deleted");
        let stamped: i64 = raw_connection(&store)
            .query_row("SELECT COUNT(*) FROM passage_uses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamped, 2, "each imported passage has a stamp");
    }

    #[test]
    fn import_of_more_entries_than_one_transaction_holds_stores_them_all() {
        const ENTRIES: usize = 2 * ROWS_PER_TRANSACTION + 1;
        let store = empty_store();
        let cache = old_cache(
            (0..ENTRIES).map(|index| (old_key(&format!("passage {index}")), vec![1.0, 0.0])),
        );
        let old_file = write_old_file(&store, &cache);

        assert_eq!(stored_passage_count(&store), ENTRIES);
        assert!(!old_file.exists(), "the imported old file is deleted");
    }

    /// The state an interrupted import leaves: some of the old file's
    /// passages stored, and the file in place.
    #[test]
    fn import_keeps_a_passage_already_stored_and_adds_the_others() {
        let store = empty_store();
        store_passages(&store, &["stored"], [1.0, 0.0]);
        let old_file = write_old_file(&store, &old_cache_of(&["stored", "added"], [0.5, 0.5]));

        let after = (
            stored_embedding(&store, "stored"),
            stored_embedding(&store, "added"),
        );

        assert_eq!(
            after,
            (Some(Arc::from([1.0, 0.0])), Some(Arc::from([0.5, 0.5]))),
            "the stored passage keeps its embedding and the other is imported"
        );
        assert!(!old_file.exists(), "the imported old file is deleted");
    }

    #[test]
    fn old_file_with_another_header_is_deleted_without_import() {
        let matching = || old_cache_of(&["alpha"], [0.5, 0.5]);
        let another_version = OldCache {
            schema_version: OLD_FILE_SCHEMA_VERSION - 1,
            ..matching()
        };
        let another_model = OldCache {
            model: "another".to_string(),
            ..matching()
        };
        let another_chunking = OldCache {
            chunk_overlap_chars: matching().chunk_overlap_chars + 1,
            ..matching()
        };
        let cases = [
            ("version", bincode::serialize(&another_version).unwrap()),
            ("model", bincode::serialize(&another_model).unwrap()),
            ("chunking", bincode::serialize(&another_chunking).unwrap()),
            ("content", b"not a cache".to_vec()),
        ];
        for (differing, bytes) in cases {
            let store = empty_store();
            let old_file = write_old_file_bytes(&store, &bytes);

            assert_eq!(
                stored_passage_count(&store),
                0,
                "an old file of another {differing} is not imported"
            );
            assert!(
                !old_file.exists(),
                "an old file of another {differing} is deleted"
            );
        }
    }

    #[test]
    fn invalid_entries_of_the_old_file_are_skipped() {
        let store = empty_store();
        let cache = old_cache([
            (old_key("valid"), vec![0.5, 0.5]),
            (old_key("another length"), vec![0.5, 0.5, 0.5]),
            (old_key("not a number"), vec![f32::NAN, 0.5]),
            ("not a hash".to_string(), vec![0.5, 0.5]),
        ]);
        let old_file = write_old_file(&store, &cache);

        assert_eq!(
            stored_passage_count(&store),
            1,
            "the valid entry alone is imported"
        );
        assert!(stored_embedding(&store, "valid").is_some());
        assert!(!old_file.exists(), "the imported old file is deleted");
    }

    #[test]
    fn old_file_cut_short_imports_the_entries_before_its_end() {
        let store = empty_store();
        let mut bytes =
            bincode::serialize(&old_cache_of(&["alpha", "beta", "gamma"], [0.5, 0.5])).unwrap();
        bytes.truncate(bytes.len() - 4);
        let old_file = write_old_file_bytes(&store, &bytes);

        assert_eq!(
            stored_passage_count(&store),
            2,
            "the two whole entries are imported"
        );
        assert!(!old_file.exists(), "the imported old file is deleted");
    }

    /// The bytes of an old file with `cache`'s entries, then the bytes
    /// `last_entry`, which the file's entry count includes as one entry.
    fn old_file_bytes_with_last_entry(cache: &OldCache, last_entry: &[u8]) -> Vec<u8> {
        let mut bytes = bincode::serialize(cache).unwrap();
        let access_counter_bytes = size_of::<u64>();
        let count_at = old_file_header().len() + access_counter_bytes;
        let count = (cache.entries.len() as u64 + 1).to_le_bytes();
        bytes[count_at..count_at + count.len()].copy_from_slice(&count);
        bytes.extend_from_slice(last_entry);
        bytes
    }

    #[test]
    fn entry_with_a_huge_length_prefix_ends_the_import_after_the_entries_before_it() {
        let huge_length = u64::MAX.to_le_bytes();
        let entry_with_huge_key = huge_length.to_vec();
        let mut entry_with_huge_embedding = bincode::serialize(&old_key("damaged")).unwrap();
        entry_with_huge_embedding.extend_from_slice(&huge_length);
        let cases = [
            ("key", entry_with_huge_key),
            ("embedding", entry_with_huge_embedding),
        ];
        for (prefixed, last_entry) in cases {
            let store = empty_store();
            let bytes = old_file_bytes_with_last_entry(
                &old_cache_of(&["alpha", "beta"], [0.5, 0.5]),
                &last_entry,
            );
            let old_file = write_old_file_bytes(&store, &bytes);

            assert_eq!(
                stored_passage_count(&store),
                2,
                "the entries before the huge {prefixed} length are imported"
            );
            assert!(!old_file.exists(), "the imported old file is deleted");
        }
    }

    #[test]
    fn low_space_skips_the_import_and_keeps_the_old_file() {
        let store = empty_store();
        store_passages(&store, &["stored"], [1.0, 0.0]);
        let old_file = write_old_file(&store, &old_cache_of(&["alpha"], [0.5, 0.5]));
        let low_on_space = PassageStore {
            disk: Arc::new(FixedFreeSpace(FREE_SPACE_FLOOR - 1)),
            ..temporary_store_at(
                store.path().unwrap().to_path_buf(),
                Arc::new(TestClock::default()),
            )
        };

        let passages_while_low = stored_passage_count(&low_on_space);

        assert_eq!(passages_while_low, 1, "the import is skipped");
        assert!(old_file.exists(), "the old file stays for a later import");
        assert_eq!(
            stored_passage_count(&store),
            2,
            "an open with space imports the old file"
        );
    }

    #[test]
    fn busy_store_skips_the_import_and_keeps_the_old_file() {
        let store = PassageStore {
            import_busy_timeout: Duration::ZERO,
            ..empty_store()
        };
        store_passages(&store, &["stored"], [1.0, 0.0]);
        let old_file = write_old_file(&store, &old_cache_of(&["alpha"], [0.5, 0.5]));
        let other_writer = raw_connection(&store);
        other_writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let passages_while_busy = stored_passage_count(&store);
        other_writer.execute_batch("COMMIT").unwrap();

        assert_eq!(passages_while_busy, 1, "the import is skipped");
        assert!(old_file.exists(), "the old file stays for a later import");
        assert_eq!(
            stored_passage_count(&store),
            2,
            "an open of the store no longer busy imports the old file"
        );
    }
}
