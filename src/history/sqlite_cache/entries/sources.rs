use crate::error::{AppError, Result};
use crate::history::cache::CachedFingerprint;
use crate::history::format::{RecordLine, SessionFormat};
use crate::log_entry::{ContentBlock, LogEntry, UserContent};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

type Results = HashMap<(usize, usize, String), Option<Value>>;

/// Restores omitted result fields for rendering and existing message anchors.
/// The cache retains their source positions instead of opaque payload bytes.
pub(super) struct Originals {
    path: PathBuf,
    format: &'static dyn SessionFormat,
    loaded: HashMap<PathBuf, Results>,
    fingerprints: HashMap<PathBuf, CachedFingerprint>,
}

impl Originals {
    pub(super) fn new(connection: &Connection, session: i64) -> rusqlite::Result<Self> {
        let (storage, path): (String, String) = connection.query_row(
            "SELECT storage, locator FROM sessions WHERE id = ?1",
            params![session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let provider = crate::history::provider::providers()
            .iter()
            .find(|provider| provider.storage().cache().directory == storage)
            .ok_or(rusqlite::Error::InvalidQuery)?;
        let fingerprints = connection
            .prepare(
                "SELECT path, file_size, mtime_secs, mtime_nsecs FROM session_files \
             WHERE session = ?1 AND file_size IS NOT NULL",
            )?
            .query_map([session], |row| {
                Ok((
                    super::super::files::key(Path::new(&row.get::<_, String>(0)?)),
                    CachedFingerprint {
                        file_size: row.get::<_, i64>(1)? as u64,
                        mtime_secs: row.get::<_, i64>(2)? as u64,
                        mtime_nsecs: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self {
            path: PathBuf::from(path),
            format: provider.format(),
            loaded: HashMap::new(),
            fingerprints,
        })
    }

    pub(super) fn restore(
        &mut self,
        record: &RecordLine,
        part: usize,
        block: &mut ContentBlock,
    ) -> Result<()> {
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        else {
            return Err(rusqlite::Error::InvalidQuery.into());
        };
        let path = record
            .transcript
            .as_deref()
            .unwrap_or(&self.path)
            .to_path_buf();
        if !self.loaded.contains_key(&path) {
            let before = super::super::files::fingerprint(&path)?;
            let expected = self.fingerprints.get(&super::super::files::key(&path));
            if expected.is_none() || expected.copied() != before {
                return Err(AppError::SessionCacheSourceChanged(path));
            }
            let results = read_results(self.format, &path)?;
            if super::super::files::fingerprint(&path)? != before {
                return Err(AppError::SessionCacheSourceChanged(path));
            }
            self.loaded.insert(path.clone(), results);
        }
        *content = self.loaded[&path]
            .get(&(record.line, part, tool_use_id.clone()))
            .cloned()
            .ok_or_else(|| AppError::SessionCacheSourceChanged(path))?;
        Ok(())
    }
}

fn read_results(format: &dyn SessionFormat, path: &Path) -> Result<Results> {
    let projection = format
        .parse_transcript(path)?
        .ok_or_else(|| AppError::SessionCacheSourceChanged(path.to_path_buf()))?;
    let mut results = Results::new();
    for (record, entry) in projection.entries {
        if record
            .transcript
            .as_deref()
            .is_some_and(|original| original != path)
        {
            continue;
        }
        let blocks = match entry {
            LogEntry::User { message, .. } => match message.content {
                UserContent::Blocks(blocks) => blocks,
                UserContent::String(_) => continue,
            },
            LogEntry::Assistant { message, .. } => message.content,
            LogEntry::Progress { data, .. } => super::progress_blocks(&data).unwrap_or_default(),
            _ => continue,
        };
        for (part, block) in blocks.into_iter().enumerate() {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            {
                results.insert((record.line, part, tool_use_id), content);
            }
        }
    }
    Ok(results)
}
