use crate::clock::Clock;
use crate::error::{AppError, Result};
use crate::history::sqlite_cache::{CopyMiss, CopyRead, unless_every_read_failed};
use crate::history::{Conversation, Workspace};
use crate::search::literal::{build_literal_corpus, exact_fallback};
use crate::search::query::ParsedQuery;
use crate::semantic::filter::filter_embedded_chunks_by_literals;
use crate::semantic::types::{ChunkHits, SemanticCancellationToken};
use crate::time_filter::TimeFilter;
use rayon::prelude::*;
use std::borrow::Cow;
use std::sync::Arc;

pub fn run(
    query: &str,
    conversations: &[Conversation],
    top: usize,
    workspace: Option<&Workspace>,
    clock: Arc<dyn Clock>,
) -> Result<()> {
    use crate::semantic::fastembed::FastembedEmbedder;
    use crate::semantic::index::{SemanticIndexRequest, SemanticIndexState};
    use crate::semantic::output::format_hit;
    use crate::semantic::types::MODEL_NAME;

    let parsed = ParsedQuery::parse(query);
    let content = searched_content(
        &conversations_in_workspace(conversations, workspace),
        &parsed,
    )?
    .reporting_skipped(&mut std::io::stderr());
    let selected = content.iter().map(|row| row.as_ref()).collect::<Vec<_>>();
    if selected.is_empty() {
        eprintln!("{}", no_conversations_message(workspace.is_some()));
        return Ok(());
    }

    if parsed.is_effectively_empty() {
        eprintln!("Query is empty.");
        return Ok(());
    }
    if parsed.is_quoted_only() {
        let results = exact_literal_indices(&selected, &parsed);
        for (rank, index) in results.iter().take(top).enumerate() {
            eprintln!("{}", format_exact_hit(rank + 1, selected[*index]));
        }
        if results.is_empty() {
            eprintln!("No semantic matches found.");
        }
        return Ok(());
    }

    let candidates = semantic_index_candidates(&selected);
    let request = SemanticIndexRequest {
        query: parsed.semantic_text(),
        literal_filters: parsed.literals(),
        full_corpus: &candidates,
        scope: &candidates,
        prewarm: false,
        chunk_hits: ChunkHits::None,
        window: TimeFilter::default(),
    };
    let mut state = SemanticIndexState::new(clock);
    let cancellation = SemanticCancellationToken::new();
    if !state.has_chunks(&request, &cancellation)? {
        state.clear_empty(&request, &cancellation)?;
        eprintln!("No visible dialogue text available for semantic search.");
        return Ok(());
    }

    let mut embedder = FastembedEmbedder::new()?;
    let (refresh, response) = refresh_and_rank_interactive(&request, &mut state, &mut embedder)?;

    eprintln!(
        "Semantic search: searching {} cached chunk(s) from {} recent conversation(s) with fastembed {MODEL_NAME}",
        refresh.indexed_chunk_count,
        selected.len()
    );

    if !response.query_embedding_returned {
        eprintln!("No query embedding returned.");
        return Ok(());
    }

    for (rank, hit) in response.hits.iter().take(top).enumerate() {
        eprintln!("{}", format_hit(rank + 1, hit, &selected));
    }

    if response.hits.is_empty() {
        eprintln!("No semantic matches found.");
    }

    Ok(())
}

pub fn clear_cache() -> Result<()> {
    let hugging_face_cache = crate::semantic::hugging_face::cache::HuggingFaceCache::locate();
    hugging_face_cache.move_old_download()?;
    match crate::semantic::cache::clear_semantic_cache_files(hugging_face_cache.path())? {
        Some(freed) => eprintln!("Semantic cache cleared: {} MB freed.", megabytes(freed)),
        None => eprintln!("Semantic cache is already empty."),
    }
    if let Some(model) = hugging_face_cache.model_on_disk()? {
        eprintln!(
            "Semantic model kept on disk: {} MB in {}",
            megabytes(crate::cache_file::disk_usage(&model)?),
            model.display()
        );
    }
    Ok(())
}

const BYTES_PER_MEGABYTE: u64 = 1_000_000;

/// `bytes` in decimal megabytes, rounded to a whole number.
fn megabytes(bytes: u64) -> u64 {
    (bytes + BYTES_PER_MEGABYTE / 2) / BYTES_PER_MEGABYTE
}

/// Embeds every passage of the conversations in `workspace` missing from the
/// passage store. Fails as [`embed_into_store`] does.
pub fn generate_cache(
    conversations: &[Conversation],
    workspace: Option<&Workspace>,
    clock: Arc<dyn Clock>,
) -> Result<()> {
    use crate::semantic::chunk::build_chunks_with_indices;
    use crate::semantic::fastembed::FastembedEmbedder;
    use crate::semantic::index::{SemanticIndexRequest, SemanticIndexState};
    use crate::semantic::types::ChunkConfig;

    let content = semantic_turns(&conversations_in_workspace(conversations, workspace))?
        .reporting_skipped(&mut std::io::stderr());
    let selected = content.iter().map(Cow::as_ref).collect::<Vec<_>>();
    if selected.is_empty() {
        eprintln!("{}", no_conversations_message(workspace.is_some()));
        return Ok(());
    }

    let candidates = semantic_index_candidates(&selected);
    let chunks = build_chunks_with_indices(
        candidates
            .iter()
            .map(|candidate| (candidate.index(), candidate.conversation().as_ref())),
        ChunkConfig::default(),
    );
    let chunk_count = chunks.len();
    if chunk_count == 0 {
        eprintln!("No semantic dialogue available for cache generation.");
        return Ok(());
    }

    let request = SemanticIndexRequest {
        query: "",
        literal_filters: &[],
        full_corpus: &candidates,
        scope: &candidates,
        prewarm: true,
        chunk_hits: ChunkHits::None,
        window: TimeFilter::default(),
    };
    let mut state = SemanticIndexState::new(clock);
    eprintln!(
        "Semantic cache: checking {chunk_count} chunk(s) from {} recent conversation(s).",
        selected.len()
    );

    let response = embed_into_store(&request, &mut state, FastembedEmbedder::new)?;

    eprintln!(
        "Semantic cache: cached {} chunk(s) from {} recent conversation(s).",
        response.indexed_chunk_count.min(chunk_count),
        selected.len()
    );

    Ok(())
}

/// Embeds the passages of `request`'s corpus missing from `state`'s passage
/// store with the embedder `load_embedder` returns, and reports each batch
/// on stderr.
/// - Fails before it loads the embedder when the store's drive is low on
///   space or the store does not open.
/// - Fails after embedding when the store skipped a write, with the count
///   of passages not stored and the reason.
fn embed_into_store<E: crate::semantic::embed::SemanticEmbedder>(
    request: &crate::semantic::index::SemanticIndexRequest<'_>,
    state: &mut crate::semantic::index::SemanticIndexState,
    load_embedder: impl FnOnce() -> Result<E>,
) -> Result<crate::semantic::index::SemanticIndexResponse> {
    use crate::semantic::index::SemanticIndexProgress;
    use crate::semantic::store::SkipReason;

    state.fail_unless_store_is_writable()?;
    let mut embedder = load_embedder()?;
    let response = state.refresh_or_prewarm(
        request,
        &mut embedder,
        &SemanticCancellationToken::new(),
        |progress| {
            if let SemanticIndexProgress::Embedding { completed, total } = progress
                && total > 0
                && completed > 0
            {
                eprintln!("Semantic cache: embedded {completed}/{total} changed chunk(s)");
            }
        },
    )?;
    match response.store_use {
        Some(store_use) if store_use.skipped > 0 => Err(AppError::PassagesNotStored {
            count: store_use.skipped,
            reason: store_use
                .skip_reason
                .unwrap_or(SkipReason::Error)
                .sentence(),
        }),
        _ => Ok(response),
    }
}

pub fn debug_search(
    query: &str,
    conversations: &[Conversation],
    workspace: Option<&Workspace>,
    clock: Arc<dyn Clock>,
) -> Result<()> {
    use crate::semantic::cache::look_up_chunks;
    use crate::semantic::chunk::build_chunks;
    use crate::semantic::embed::embed_valid_query;
    use crate::semantic::fastembed::FastembedEmbedder;
    use crate::semantic::output::{format_hit, truncate};
    use crate::semantic::rank::rank_chunks;
    use crate::semantic::store::PassageStore;
    use crate::semantic::types::{ChunkConfig, MODEL_NAME, SemanticCancellationToken};
    use std::collections::HashMap;

    let parsed = ParsedQuery::parse(query);
    let content = searched_content(
        &conversations_in_workspace(conversations, workspace),
        &parsed,
    )?
    .reporting_skipped(&mut std::io::stderr());
    let selected = content.iter().map(|row| row.as_ref()).collect::<Vec<_>>();
    eprint!("{}", format_parsed_query(&parsed));
    eprintln!();
    eprintln!(
        "Semantic debug: selected {} conversation(s).",
        selected.len()
    );
    eprintln!(
        "Semantic debug: model cache: {}",
        FastembedEmbedder::cache_dir().display()
    );
    let store = PassageStore::in_folder(
        crate::semantic::cache::semantic_cache_dir().as_deref(),
        clock,
    );
    match store.path() {
        Some(path) => eprintln!("Semantic debug: embedding cache: {}", path.display()),
        None => eprintln!("Semantic debug: embedding cache: unavailable"),
    }

    if selected.is_empty() {
        eprintln!("{}", no_conversations_message(workspace.is_some()));
        return Ok(());
    }

    if parsed.is_effectively_empty() {
        eprintln!("Semantic debug: query is empty.");
        return Ok(());
    }

    let turn_count = selected
        .iter()
        .map(|conversation| conversation.semantic_turns.len())
        .sum::<usize>();
    let chunk_config = ChunkConfig::default();
    let chunks = build_chunks(&selected, chunk_config);
    eprintln!(
        "Semantic debug: {} semantic turn(s), {} chunk(s), target={} overlap={} context_turns={}",
        turn_count,
        chunks.len(),
        chunk_config.target_chars,
        chunk_config.overlap_chars,
        chunk_config.context_turns
    );

    let mut chunk_counts = HashMap::<usize, usize>::new();
    for chunk in &chunks {
        *chunk_counts.entry(chunk.conversation_index).or_default() += 1;
    }
    let mut chunk_counts = chunk_counts.into_iter().collect::<Vec<_>>();
    chunk_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (rank, (index, count)) in chunk_counts.iter().take(10).enumerate() {
        let conversation = selected[*index];
        let title = conversation
            .custom_title
            .as_deref()
            .or(conversation.summary.as_deref())
            .unwrap_or(&conversation.preview);
        eprintln!(
            "Semantic debug: chunk-heavy #{:2}: {} chunk(s) | {} | {}",
            rank + 1,
            count,
            conversation.path.display(),
            title
        );
    }

    let suspicious = chunks
        .iter()
        .filter(|chunk| {
            chunk.text.contains("```")
                || chunk.text.contains("<system-reminder>")
                || chunk.text.contains("<local-command-stdout>")
                || chunk.text.contains("<local-command-stderr>")
                || chunk.text.contains("<command-message>")
        })
        .take(5)
        .collect::<Vec<_>>();
    if suspicious.is_empty() {
        eprintln!(
            "Semantic debug: no sampled chunks contain code fences or command/system markers."
        );
    } else {
        for chunk in suspicious {
            eprintln!(
                "Semantic debug: suspicious chunk {}:{}: {}",
                chunk.session,
                chunk.chunk_index,
                truncate(&chunk.text, 220)
            );
        }
    }

    if parsed.is_quoted_only() {
        let results = exact_literal_indices(&selected, &parsed);
        eprintln!("Semantic debug: exact literal matches={}.", results.len());
        for (rank, index) in results.iter().take(20).enumerate() {
            eprintln!("{}", format_exact_hit(rank + 1, selected[*index]));
        }
        if results.is_empty() {
            eprintln!("Semantic debug: no semantic matches found.");
        }
        return Ok(());
    }

    if chunks.is_empty() {
        eprintln!("Semantic debug: no visible dialogue text available.");
        return Ok(());
    }

    let chunk_count = chunks.len();
    let mut open_store = store.open();
    let lookup = look_up_chunks(chunks, &mut open_store, &SemanticCancellationToken::new())?;
    let missing = lookup.missing_passage_count();
    eprintln!(
        "Semantic debug: cache entries={}, hits={}, misses={}",
        open_store.passage_count(),
        chunk_count.saturating_sub(missing),
        missing
    );
    drop(open_store);
    for chunk in lookup.missing.iter().map(|chunks| &chunks[0]).take(5) {
        eprintln!(
            "Semantic debug: missing cache chunk {}:{}: {}",
            chunk.session,
            chunk.chunk_index,
            truncate(&chunk.text, 180)
        );
    }

    let embedded_chunks = lookup.embedded;
    if embedded_chunks.is_empty() {
        eprintln!("Semantic debug: no cached chunks available for ranking.");
        return Ok(());
    }

    let mut embedder = FastembedEmbedder::new()?;
    let Some(query_embedding) = embed_valid_query(&mut embedder, parsed.semantic_text())? else {
        eprintln!("Semantic debug: no query embedding returned.");
        return Ok(());
    };
    let embedded_chunks = filter_embedded_chunks_by_literals(embedded_chunks, parsed.literals());
    let hits = rank_chunks(
        parsed.semantic_text(),
        &query_embedding,
        &embedded_chunks,
        &SemanticCancellationToken::new(),
    )?;
    eprintln!(
        "Semantic debug: ranked {} cached chunk(s) with fastembed {MODEL_NAME}.",
        embedded_chunks.len()
    );

    for (rank, hit) in hits.iter().take(20).enumerate() {
        eprintln!("{}", format_hit(rank + 1, hit, &selected));
    }
    if hits.is_empty() {
        eprintln!("Semantic debug: no semantic matches found.");
    }

    Ok(())
}

fn format_exact_hit(rank: usize, conversation: &Conversation) -> String {
    let project = conversation.project_name.as_deref().unwrap_or("(none)");
    let title = conversation
        .custom_title
        .as_deref()
        .or(conversation.summary.as_deref())
        .unwrap_or(&conversation.preview);
    let session = &conversation.session_id;
    format!("#{rank:2} exact | {project} | {session}\n     {title}\n")
}

fn exact_literal_indices(conversations: &[&Conversation], parsed: &ParsedQuery) -> Vec<usize> {
    let plain_conversations = conversations
        .iter()
        .map(|conversation| (*conversation).clone())
        .collect::<Vec<_>>();
    let corpus = build_literal_corpus(&plain_conversations);
    exact_fallback(
        &corpus,
        parsed.literals(),
        |_| true,
        |index| plain_conversations[index].timestamp,
    )
}

fn format_parsed_query(parsed: &ParsedQuery) -> String {
    let mut output = format!("intent: {:?}\n", parsed.unquoted());
    if parsed.literals().is_empty() {
        output.push_str("literals: none\n");
    } else {
        output.push_str("literals:\n");
        for literal in parsed.literals() {
            output.push_str(&format!(
                "  {:?} ({:?})\n",
                literal.text(),
                literal.case_mode()
            ));
        }
    }
    output
}

fn refresh_and_rank_interactive(
    request: &crate::semantic::index::SemanticIndexRequest<'_>,
    state: &mut crate::semantic::index::SemanticIndexState,
    embedder: &mut dyn crate::semantic::embed::SemanticEmbedder,
) -> Result<(
    crate::semantic::index::SemanticIndexResponse,
    crate::semantic::index::SemanticIndexResponse,
)> {
    let cancellation = SemanticCancellationToken::new();
    let refresh = state.refresh_passages(request, embedder, &cancellation, |_| {})?;
    let response = state.rank_refreshed(request, embedder, &cancellation, |_| {})?;
    Ok((refresh, response))
}

/// The candidates `--generate-semantic-cache` embeds for `rows`, without the
/// rows whose read missed.
#[cfg(test)]
pub(crate) fn cache_generation_candidates(
    rows: &[&Conversation],
) -> Vec<crate::semantic::index::SemanticIndexCandidate> {
    let content = semantic_turns(rows).unwrap().held;
    semantic_index_candidates(&content.iter().map(Cow::as_ref).collect::<Vec<_>>())
}

fn semantic_index_candidates(
    selected: &[&Conversation],
) -> Vec<crate::semantic::index::SemanticIndexCandidate> {
    selected
        .iter()
        .enumerate()
        .map(|(index, conversation)| {
            crate::semantic::index::SemanticIndexCandidate::of_session(
                index,
                std::sync::Arc::new((*conversation).clone()),
            )
        })
        .collect()
}

/// Every conversation, or with a `workspace`, only those each agent's own
/// rule places in it.
fn conversations_in_workspace<'a>(
    conversations: &'a [Conversation],
    workspace: Option<&Workspace>,
) -> Vec<&'a Conversation> {
    conversations
        .iter()
        .filter(|conversation| workspace.is_none_or(|workspace| workspace.contains(conversation)))
        .collect()
}

/// The content a semantic search for `parsed` reads of each of `rows`: for a
/// query of quoted phrases alone, the search texts it matches exactly;
/// otherwise the semantic turns alone, as agent search reads them.
fn searched_content<'r>(
    rows: &[&'r Conversation],
    parsed: &ParsedQuery,
) -> Result<HeldReads<'r, Cow<'r, Conversation>>> {
    if parsed.is_quoted_only() {
        held_content(rows)
    } else {
        semantic_turns(rows)
    }
}

/// The content of each of `rows`, read from the copy the session cache holds
/// now, whichever generation it is.
fn held_content<'r>(rows: &[&'r Conversation]) -> Result<HeldReads<'r, Cow<'r, Conversation>>> {
    HeldReads::of(rows, |row| row.load_held_content().map(|held| held.row))
}

/// Each of `rows` with the semantic turns of the copy the session cache holds
/// now, read together as [`Conversation::load_held_semantic_turns_of`]
/// reads them; a row without a copy is its own content.
fn semantic_turns<'r>(rows: &[&'r Conversation]) -> Result<HeldReads<'r, Cow<'r, Conversation>>> {
    let mut reads = rows
        .iter()
        .map(|&row| Ok(Cow::Borrowed(row)))
        .collect::<Vec<_>>();
    let (positions, cached): (Vec<_>, Vec<_>) = rows
        .iter()
        .enumerate()
        .filter_map(|(position, &row)| Some((position, (row, row.cached_session.as_ref()?))))
        .unzip();
    let cached_reads = Conversation::load_held_semantic_turns_of(&cached)?;
    for (position, read) in positions.into_iter().zip(cached_reads) {
        reads[position] = read.map(|copy| Cow::Owned(copy.row));
    }
    HeldReads::of_reads(rows, reads)
}

/// A read of each row's copy the session cache holds now, and the rows of the
/// sessions skipped, each with the reason.
struct HeldReads<'r, T> {
    /// One read per row whose copy was read, in the rows' order.
    held: Vec<T>,
    skipped: Vec<(&'r Conversation, CopyMiss)>,
}

impl<'r, T: Send> HeldReads<'r, T> {
    /// `read` of each of `rows`, on the rayon pool, as [`Self::of_reads`]
    /// accepts them.
    fn of(
        rows: &[&'r Conversation],
        read: impl Fn(&'r Conversation) -> CopyRead<T> + Sync,
    ) -> Result<Self> {
        let results = rows.par_iter().map(|&row| read(row)).collect::<Vec<_>>();
        Self::of_reads(rows, results)
    }

    /// The reads `results`, one per row of `rows` in order. A row whose read
    /// missed is skipped; a failed read of every row fails the reads.
    fn of_reads(rows: &[&'r Conversation], results: Vec<CopyRead<T>>) -> Result<Self> {
        let mut reads = Self {
            held: Vec::with_capacity(rows.len()),
            skipped: Vec::new(),
        };
        for (&row, result) in rows.iter().zip(unless_every_read_failed(results)?) {
            match result {
                Ok(content) => reads.held.push(content),
                Err(miss) => reads.skipped.push((row, miss)),
            }
        }
        Ok(reads)
    }

    /// The reads, after writing one line to `stderr` for each session
    /// skipped, with the reason.
    fn reporting_skipped(self, stderr: &mut impl std::io::Write) -> Vec<T> {
        for (row, miss) in self.skipped {
            let _ = writeln!(
                stderr,
                "Skipped session {}: {}.",
                row.session_id,
                skipped_reason(&miss)
            );
        }
        self.held
    }
}

/// The reason the semantic commands skip a session whose read missed with
/// `miss`.
fn skipped_reason(miss: &CopyMiss) -> Cow<'static, str> {
    match miss {
        CopyMiss::NotThere => Cow::Borrowed("it is no longer in the session cache"),
        CopyMiss::NotReady => Cow::Borrowed("its cached copy is not ready yet"),
        CopyMiss::ReadError(error) => Cow::Owned(error.to_string()),
    }
}

fn no_conversations_message(local: bool) -> &'static str {
    if local {
        "No conversations available for semantic search in the current workspace."
    } else {
        "No conversations available for semantic search."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use crate::semantic::embed::SemanticEmbedder;
    use crate::semantic::store::PassageStore;
    use crate::semantic::store::test_support::{
        stored_passage_count, temporary_store_at, temporary_store_path,
    };
    use crate::semantic::test_fixtures::SemanticConversationFixture;
    use crate::semantic::types::ChunkConfig;
    use chrono::Local;

    /// The reads behind the semantic search flags and cache generation
    /// ([`held_content`] for quoted phrases alone, [`semantic_turns`]
    /// otherwise) read a session another rearview wrote again from the copy
    /// the cache holds, and write one line to stderr naming a session it
    /// removed.
    #[test]
    fn held_content_reads_copy_cache_holds_and_skips_removed_session() {
        use crate::search::test_fixtures::{cached_claude_sessions, october_messages as messages};
        let sessions = cached_claude_sessions(&[
            ("replaced", messages(&["first"])),
            ("removed", messages(&["second"])),
        ]);
        sessions.rewrite(0, &messages(&["inserted", "first"]));
        sessions.remove(1);
        let rows = sessions.rows.iter().collect::<Vec<_>>();

        let (mut stderr, mut turns_stderr) = (Vec::new(), Vec::new());
        let content = held_content(&rows).unwrap().reporting_skipped(&mut stderr);
        let read_turns = semantic_turns(&rows)
            .unwrap()
            .reporting_skipped(&mut turns_stderr);

        let turns = [vec!["inserted".to_owned(), "first".to_owned()]];
        for (read, rows) in [("held", &content), ("semantic turns", &read_turns)] {
            assert_eq!(
                rows.iter()
                    .map(|row| row.semantic_turns.clone())
                    .collect::<Vec<_>>(),
                turns,
                "the {read} read reads the replaced session as the cache holds it"
            );
        }
        let skipped = format!(
            "Skipped session {}: it is no longer in the session cache.\n",
            sessions.rows[1].session_id
        );
        for (read, stderr) in [("held", stderr), ("semantic turns", turns_stderr)] {
            assert_eq!(
                String::from_utf8(stderr).unwrap(),
                skipped,
                "the {read} content read names the removed session on stderr"
            );
        }
    }

    /// `--semantic-search` reads the other sessions and writes one line to
    /// stderr naming a session another rearview is writing.
    #[test]
    fn semantic_search_skips_session_being_written_and_reads_the_others() {
        use crate::search::test_fixtures::{cached_claude_sessions, october_messages as messages};
        let sessions = cached_claude_sessions(&[
            ("written", messages(&["first"])),
            ("kept", messages(&["second"])),
        ]);
        sessions.mark_writing(0);
        let rows = sessions.rows.iter().collect::<Vec<_>>();
        let mut stderr = Vec::new();

        let content = searched_content(&rows, &ParsedQuery::parse("question"))
            .expect("the search reads the other session")
            .reporting_skipped(&mut stderr);

        assert_eq!(
            content
                .iter()
                .map(|row| row.semantic_turns.clone())
                .collect::<Vec<_>>(),
            [vec!["second".to_owned()]],
            "the other session is searched"
        );
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!(
                "Skipped session {}: its cached copy is not ready yet.\n",
                sessions.rows[0].session_id
            ),
            "one line names the session being written"
        );
    }

    /// When the read of every session fails, `--semantic-search` fails with
    /// one error and doesn't skip any session, whether it reads semantic
    /// turns or, for quoted phrases alone, search texts: the failure is the
    /// cache's.
    #[test]
    fn semantic_search_whose_every_read_fails_fails_without_skipping_sessions() {
        use crate::search::test_fixtures::{cached_claude_sessions, october_messages as messages};
        let sessions = cached_claude_sessions(&[
            ("first", messages(&["one"])),
            ("second", messages(&["two"])),
        ]);
        crate::history::sqlite_cache::tests::hide_roots_table(sessions.cache_path());
        let rows = sessions.rows.iter().collect::<Vec<_>>();

        for query in ["question", "\"one\""] {
            let read = searched_content(&rows, &ParsedQuery::parse(query));

            if let Ok(reads) = read {
                let mut stderr = Vec::new();
                reads.reporting_skipped(&mut stderr);
                panic!(
                    "{query}: the search fails instead of skipping sessions:\n{}",
                    String::from_utf8_lossy(&stderr)
                );
            }
        }
    }

    #[test]
    fn semantic_search_flags_chunk_the_turns_agent_search_reads() {
        use crate::search::test_fixtures::{cached_claude_sessions, october_messages as messages};
        use crate::semantic::chunk::build_chunks;
        let sessions = cached_claude_sessions(&[
            ("replaced", messages(&["first question", "second answer"])),
            ("kept", messages(&["third question", "fourth answer"])),
        ]);
        sessions.rewrite(
            0,
            &messages(&["inserted note", "first question", "second answer"]),
        );
        let rows = sessions.rows.iter().collect::<Vec<_>>();
        let chunks = |rows: &[&Conversation]| {
            build_chunks(rows, ChunkConfig::default())
                .into_iter()
                .map(|chunk| (chunk.conversation_index, chunk.text))
                .collect::<Vec<_>>()
        };

        let flags = semantic_turns(&rows).unwrap().held;
        let agent = rows
            .iter()
            .map(|row| row.load_semantic_content().unwrap().row)
            .collect::<Vec<_>>();

        let agent_chunks = chunks(&agent.iter().map(Cow::as_ref).collect::<Vec<_>>());
        assert!(
            agent_chunks
                .iter()
                .any(|(_, text)| text.contains("inserted note")),
            "the agent read holds the replaced copy's turns: {agent_chunks:?}"
        );
        assert_eq!(
            chunks(&flags.iter().map(Cow::as_ref).collect::<Vec<_>>()),
            agent_chunks,
            "the semantic search flags chunk the turns agent search reads"
        );
    }

    struct FakeEmbedder {
        passage_calls: usize,
        query_calls: usize,
        query_embedding: Option<Vec<f32>>,
        query_error: bool,
    }

    impl FakeEmbedder {
        fn new(query_embedding: Option<Vec<f32>>) -> Self {
            Self {
                passage_calls: 0,
                query_calls: 0,
                query_embedding,
                query_error: false,
            }
        }
    }

    impl SemanticEmbedder for FakeEmbedder {
        fn dimensions(&self) -> usize {
            2
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
            self.passage_calls += 1;
            Ok(passages
                .iter()
                .map(|passage| match passage.as_str() {
                    "visible beta" => vec![0.0, 1.0],
                    _ => vec![1.0, 0.0],
                })
                .collect())
        }

        fn embed_query(&mut self, query: &str) -> Result<Option<Vec<f32>>> {
            self.query_calls += 1;
            if self.query_error {
                return Err(AppError::ConfigError("query failed".to_string()));
            }
            Ok(if query.contains("beta") {
                Some(vec![0.0, 1.0])
            } else {
                self.query_embedding.clone()
            })
        }
    }

    fn test_conversation(path: &str, title: &str, semantic_turns: Vec<String>) -> Conversation {
        SemanticConversationFixture::new(path, semantic_turns)
            .with_title(title)
            .with_project("project-a", "/projects/project-a")
            .with_cwd(None)
            .with_summary(None)
            .build()
    }

    fn interactive_refresh_context(
        query: &str,
        query_embedding: Option<Vec<f32>>,
        query_error: bool,
    ) -> (
        String,
        Vec<crate::semantic::index::SemanticIndexCandidate>,
        crate::semantic::index::SemanticIndexState,
        FakeEmbedder,
        PassageStore,
    ) {
        let conversations = [test_conversation(
            "/projects/project-a/session-1.jsonl",
            "one",
            vec!["visible alpha".to_string()],
        )];
        let selected = vec![&conversations[0]];
        let candidates = semantic_index_candidates(&selected);
        let clock = Arc::new(crate::clock::TestClock::default());
        let store_path = temporary_store_path();
        let state = crate::semantic::index::SemanticIndexState::with_store(
            ChunkConfig::default(),
            temporary_store_at(store_path.clone(), clock.clone()),
            clock.clone(),
        );
        let mut embedder = FakeEmbedder::new(query_embedding);
        embedder.query_error = query_error;

        (
            query.to_string(),
            candidates,
            state,
            embedder,
            temporary_store_at(store_path, clock),
        )
    }

    fn interactive_refresh_request<'a>(
        query: &'a String,
        candidates: &'a [crate::semantic::index::SemanticIndexCandidate],
    ) -> crate::semantic::index::SemanticIndexRequest<'a> {
        crate::semantic::index::SemanticIndexRequest {
            query,
            literal_filters: &[],
            full_corpus: candidates,
            scope: candidates,
            prewarm: false,
            chunk_hits: crate::semantic::types::ChunkHits::None,
            window: TimeFilter::default(),
        }
    }

    #[test]
    fn interactive_refresh_stores_passages_when_query_has_no_embedding() {
        let (query, candidates, mut state, mut embedder, store) =
            interactive_refresh_context("alpha", None, false);
        let request = interactive_refresh_request(&query, &candidates);

        let (refresh, response) = refresh_and_rank_interactive(&request, &mut state, &mut embedder)
            .expect("interactive refresh and rank succeeds");

        assert_eq!(refresh.indexed_chunk_count, 1);
        assert_eq!(stored_passage_count(&store), 1);
        assert!(!response.query_embedding_returned);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 1);
    }

    #[test]
    fn interactive_refresh_stores_passages_before_query_error() {
        let (query, candidates, mut state, mut embedder, store) =
            interactive_refresh_context("alpha", Some(vec![1.0, 0.0]), true);
        let request = interactive_refresh_request(&query, &candidates);

        let error = match refresh_and_rank_interactive(&request, &mut state, &mut embedder) {
            Ok(_) => panic!("query error should propagate"),
            Err(error) => error,
        };

        assert_eq!(stored_passage_count(&store), 1);
        assert_eq!(embedder.passage_calls, 1);
        assert_eq!(embedder.query_calls, 1);
        assert!(error.to_string().contains("query failed"));
    }

    fn one_passage_corpus() -> Vec<crate::semantic::index::SemanticIndexCandidate> {
        let conversation = test_conversation(
            "/projects/project-a/session-1.jsonl",
            "one",
            vec!["visible alpha".to_string()],
        );
        semantic_index_candidates(&[&conversation])
    }

    /// The request cache generation makes for `candidates`.
    fn prewarm_request(
        candidates: &[crate::semantic::index::SemanticIndexCandidate],
    ) -> crate::semantic::index::SemanticIndexRequest<'_> {
        crate::semantic::index::SemanticIndexRequest {
            query: "",
            literal_filters: &[],
            full_corpus: candidates,
            scope: candidates,
            prewarm: true,
            chunk_hits: crate::semantic::types::ChunkHits::None,
            window: TimeFilter::default(),
        }
    }

    #[test]
    fn cache_generation_fails_before_loading_the_model_when_the_store_doesnt_open() {
        let candidates = one_passage_corpus();
        let clock = Arc::new(crate::clock::TestClock::default());
        let unopenable = temporary_store_path();
        // A folder where the store's file belongs: SQLite cannot open it.
        std::fs::create_dir_all(&unopenable).unwrap();
        let mut state = crate::semantic::index::SemanticIndexState::with_store(
            ChunkConfig::default(),
            temporary_store_at(unopenable, clock.clone()),
            clock,
        );
        let mut model_loaded = false;

        let generated = embed_into_store(&prewarm_request(&candidates), &mut state, || {
            model_loaded = true;
            Ok(FakeEmbedder::new(None))
        });

        assert!(
            matches!(generated, Err(AppError::PassageStoreNotOpened(_))),
            "cache generation fails with the store's error"
        );
        assert!(!model_loaded, "the failure comes before the model loads");
    }

    #[test]
    fn cache_generation_fails_with_count_and_reason_when_the_store_skipped_a_write() {
        use crate::free_space::DriveRunningOut;
        use crate::semantic::store::SkipReason;
        use crate::semantic::store::test_support::temporary_store_on;
        let candidates = one_passage_corpus();
        let clock = Arc::new(crate::clock::TestClock::default());
        let path = temporary_store_path();
        drop(temporary_store_at(path.clone(), clock.clone()).open());
        // The drive has room at the check before embedding and is low on
        // space at the write.
        let drive = Arc::new(DriveRunningOut::after(1, || {}));
        let mut state = crate::semantic::index::SemanticIndexState::with_store(
            ChunkConfig::default(),
            temporary_store_on(path.clone(), clock.clone(), drive),
            clock.clone(),
        );

        let generated = embed_into_store(&prewarm_request(&candidates), &mut state, || {
            Ok(FakeEmbedder::new(None))
        });

        assert!(
            matches!(
                generated,
                Err(AppError::PassagesNotStored { count: 1, reason })
                    if reason == SkipReason::LowSpace.sentence()
            ),
            "cache generation reports the one passage not stored and the reason"
        );
        assert_eq!(
            stored_passage_count(&temporary_store_at(path, clock)),
            0,
            "the skipped passage is not in the store"
        );
    }

    #[test]
    fn selection_includes_all_conversations() {
        let conversations = vec![
            test_conversation(
                "/projects/project-a/session-1.jsonl",
                "one",
                vec!["one".to_string()],
            ),
            test_conversation(
                "/projects/project-a/session-2.jsonl",
                "two",
                vec!["two".to_string()],
            ),
        ];

        let selected = conversations_in_workspace(&conversations, None);

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].custom_title.as_deref(), Some("one"));
        assert_eq!(selected[1].custom_title.as_deref(), Some("two"));
    }

    /// Each agent's own rule decides: a Claude session by the project folder
    /// it sits in, any other agent's by the directory it recorded.
    #[test]
    fn selection_filters_to_given_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = crate::history::workspace::tests::workspace_at_home(dir.path());
        let claude_project =
            crate::history::provider::claude::convert_path_to_project_dir_name(dir.path());
        let mut other_agent_here = test_conversation(
            "/sessions/2026/10/05/rollout.jsonl",
            "other agent, here",
            vec!["here".to_string()],
        );
        other_agent_here.source = crate::history::Source::Codex;
        other_agent_here.cwd = Some(dir.path().to_path_buf());
        let mut other_agent_elsewhere = other_agent_here.clone();
        other_agent_elsewhere.custom_title = Some("other agent, elsewhere".to_string());
        other_agent_elsewhere.cwd = Some(std::path::PathBuf::from("/elsewhere"));
        let conversations = vec![
            test_conversation(
                &format!("projects/{claude_project}/session-1.jsonl"),
                "local",
                vec!["local".to_string()],
            ),
            test_conversation(
                "projects/other-project/session-2.jsonl",
                "other",
                vec!["other".to_string()],
            ),
            other_agent_here,
            other_agent_elsewhere,
        ];

        let selected = conversations_in_workspace(&conversations, Some(&workspace));

        assert_eq!(
            selected
                .iter()
                .map(|conversation| conversation.custom_title.as_deref())
                .collect::<Vec<_>>(),
            [Some("local"), Some("other agent, here")]
        );
    }

    #[test]
    fn empty_conversation_message_mentions_local_scope() {
        assert_eq!(
            no_conversations_message(true),
            "No conversations available for semantic search in the current workspace."
        );
        assert_eq!(
            no_conversations_message(false),
            "No conversations available for semantic search."
        );
    }

    #[test]
    fn semantic_index_candidates_use_selected_slice_indices() {
        let conversations = [
            test_conversation(
                "/projects/project-a/session-1.jsonl",
                "one",
                vec!["one".to_string()],
            ),
            test_conversation(
                "/projects/project-a/session-2.jsonl",
                "two",
                vec!["two".to_string()],
            ),
        ];
        let selected = vec![&conversations[1], &conversations[0]];

        let candidates = semantic_index_candidates(&selected);

        assert_eq!(candidates[0].index(), 0);
        assert_eq!(
            candidates[0].conversation().custom_title.as_deref(),
            Some("two")
        );
        assert_eq!(candidates[1].index(), 1);
        assert_eq!(
            candidates[1].conversation().custom_title.as_deref(),
            Some("one")
        );
    }

    #[test]
    fn empty_corpus_returns_before_model_initialization() {
        run(
            "cache",
            &[],
            1,
            None,
            Arc::new(crate::clock::TestClock::default()),
        )
        .expect("empty corpus returns");
    }

    #[test]
    fn quoted_only_exact_fallback_returns_literal_matches_newest_first() {
        let mut older = test_conversation(
            "/projects/project-a/session-1.jsonl",
            "literal needle",
            vec![],
        );
        older.timestamp = Local::now() - chrono::Duration::days(1);
        let newer = test_conversation(
            "/projects/project-a/session-2.jsonl",
            "literal needle",
            vec![],
        );
        let miss = test_conversation("/projects/project-a/session-3.jsonl", "other", vec![]);
        let conversations = [older, newer, miss];
        let selected = conversations.iter().collect::<Vec<_>>();
        let parsed = ParsedQuery::parse("\"literal needle\"");

        let results = exact_literal_indices(&selected, &parsed);

        assert_eq!(results, vec![1, 0]);
    }

    #[test]
    fn quoted_uuid_exact_fallback_is_literal_search() {
        let uuid = "e7d318b1-4274-4ee2-a341-e94893b5df49";
        let conversations = [
            test_conversation("/projects/project-a/session-1.jsonl", uuid, vec![]),
            test_conversation("/projects/project-a/session-2.jsonl", "other", vec![]),
        ];
        let selected = conversations.iter().collect::<Vec<_>>();
        let parsed = ParsedQuery::parse(&format!("\"{uuid}\""));

        let results = exact_literal_indices(&selected, &parsed);

        assert_eq!(parsed.semantic_text(), "");
        assert_eq!(results, vec![0]);
    }

    #[test]
    fn semantic_filters_use_smart_case_literals() {
        let conversations = [
            test_conversation(
                "/projects/project-a/session-1.jsonl",
                "restaurant_signals lower phrase",
                vec!["visible alpha restaurant_signals".to_string()],
            ),
            test_conversation(
                "/projects/project-a/session-2.jsonl",
                "RESTAURANT_SIGNALS lower phrase",
                vec!["visible alpha".to_string()],
            ),
        ];
        let chunks = conversations
            .iter()
            .enumerate()
            .map(
                |(conversation_index, conversation)| crate::semantic::types::EmbeddedChunk {
                    conversation_index,
                    session: format!("session-{}", conversation_index + 1),
                    chunk_index: 0,
                    text: conversation.semantic_turns[0].clone(),
                    message_range: crate::agent::refs::MessageRange::single(1),
                    embedding: [1.0, 0.0].into(),
                },
            )
            .collect::<Vec<_>>();

        let insensitive = filter_embedded_chunks_by_literals(
            chunks.clone(),
            ParsedQuery::parse("alpha \"restaurant_signals\"").literals(),
        );
        let sensitive = filter_embedded_chunks_by_literals(
            chunks,
            ParsedQuery::parse("alpha \"RESTAURANT_SIGNALS\"").literals(),
        );

        assert_eq!(insensitive.len(), 1);
        assert_eq!(insensitive[0].conversation_index, 0);
        assert!(sensitive.is_empty());
    }

    #[test]
    fn parsed_query_debug_output_reports_intent_and_literals() {
        let parsed = ParsedQuery::parse("alpha \"RESTAURANT_SIGNALS\" \"lower phrase\"");

        let output = format_parsed_query(&parsed);

        assert!(output.contains("intent: \"alpha\""));
        assert!(output.contains("\"RESTAURANT_SIGNALS\" (Sensitive)"));
        assert!(output.contains("\"lower phrase\" (Insensitive)"));
    }
}
