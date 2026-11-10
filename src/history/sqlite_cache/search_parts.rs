//! Bounded entry batches projected onto the existing search surfaces.

use super::{CachedSession, entries, files};
use crate::error::Result;
use crate::history::Conversation;
use crate::history::parser::{
    normalize_full_text, normalize_whitespace, search_parts_of_entry, semantic_text_of_entry,
};
use crate::log_entry::LogEntry;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchField {
    Full,
    Agent,
    Thinking,
    Leading,
    Supplement,
    Semantic,
}

pub(crate) struct SearchPart {
    pub(crate) ordinal: Option<usize>,
    pub(crate) field: SearchField,
    pub(crate) text: String,
}

impl CachedSession {
    pub(crate) fn search_parts(
        &self,
        row: &Conversation,
        visit: impl FnMut(SearchPart),
    ) -> Result<Option<()>> {
        self.search_parts_within(row, &crate::time_filter::TimeFilter::default(), visit)
    }

    pub(crate) fn search_parts_within(
        &self,
        row: &Conversation,
        window: &crate::time_filter::TimeFilter,
        mut visit: impl FnMut(SearchPart),
    ) -> Result<Option<()>> {
        self.read(|connection, session| {
            let subagents: HashSet<_> = row.subagents.iter().map(|path| files::key(path)).collect();
            let mut spliced_transcripts = HashMap::new();
            let mut after = -1;
            let mut summary = None;
            let mut title = None;
            loop {
                let batch = entries::read_search_batch(connection, session, after, window)?;
                let Some(last) = batch.last_sequence else { break };
                for ((record, entry), number) in batch.transcript.entries.into_iter().zip(batch.numbers) {
                    let ordinal = number.map(|number| number.ordinal);
                    match &entry {
                        LogEntry::Summary { summary: text } if summary.is_none() => summary = Some(text.clone()),
                        LogEntry::AiTitle { ai_title } if !ai_title.trim().is_empty() => summary = Some(ai_title.trim().to_owned()),
                        LogEntry::CustomTitle { custom_title } => title = (!custom_title.trim().is_empty()).then(|| custom_title.trim().to_owned()),
                        _ => {}
                    }
                    let spliced = record.transcript.as_ref().is_some_and(|path| {
                        *spliced_transcripts.entry(path.clone()).or_insert_with(|| subagents.contains(&files::key(path)))
                    });
                    for (field, texts) in [SearchField::Full, SearchField::Agent, SearchField::Thinking].into_iter().zip(search_parts_of_entry(&entry, spliced)) {
                        for text in texts {
                            let text = if field == SearchField::Full { normalize_full_text(&text) } else { normalize_whitespace(&text) };
                            if !text.is_empty() { visit(SearchPart { ordinal, field, text }); }
                        }
                    }
                    if ordinal.is_some() && let Some(text) = semantic_text_of_entry(&entry) {
                        visit(SearchPart { ordinal, field: SearchField::Semantic, text });
                    }
                }
                after = last;
            }
            if window.is_active() { return Ok(()); }
            for text in [title, summary].into_iter().flatten() {
                visit(SearchPart { ordinal: None, field: SearchField::Leading, text: normalize_full_text(&text) });
            }
            let mut statement = connection.prepare("SELECT sanitise_text, text FROM subagent_search_parts WHERE session = ?1 ORDER BY position")?;
            let mut parts = statement.query([session])?;
            while let Some(part) = parts.next()? {
                let full: bool = part.get(0)?;
                let text: String = part.get(1)?;
                let text = if full { normalize_full_text(&text) } else { normalize_whitespace(&text) };
                if !text.is_empty() { visit(SearchPart { ordinal: None, field: SearchField::Supplement, text }); }
            }
            Ok(())
        })
    }
}
