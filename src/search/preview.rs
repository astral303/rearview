//! Match selection and truncation for list-row previews: which slice of a
//! preview to show at a width, and where the query matches inside it.

use crate::history::MESSAGE_SEPARATOR;
use crate::search::evidence::select_hidden_context_ranges;
use crate::search::literal::{Literal, match_literal_ranges};
use crate::search::normalize_for_search;
use crate::search::query::ParsedQuery;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate text to max_width chars, adding "…" suffix if truncated.
pub(crate) fn simple_truncate(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let mut result = String::new();
    let ellipsis_width = UnicodeWidthChar::width('…').unwrap_or(1);
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result.push('…');
    result
}

/// Build a display string showing context around each match, joined by "…".
/// Operates on already-sanitized text (e.g. preview). Falls back to simple
/// truncation when all matches already fit within max_width.
pub(crate) fn build_match_segments_for_query(
    text: &str,
    query: &HighlightQuery,
    max_width: usize,
) -> String {
    build_match_segments_with_ranges(text, query.match_ranges(text), max_width, || {
        simple_truncate(text, max_width)
    })
}

pub(crate) fn build_match_segments(text: &str, query: &str, max_width: usize) -> String {
    if query.is_empty() || max_width == 0 {
        return simple_truncate(text, max_width);
    }

    let ranges = find_normalized_match_ranges(text, query);
    build_match_segments_with_ranges(text, ranges, max_width, || {
        truncate_around_match(text, query, max_width)
    })
}

fn build_match_segments_with_ranges(
    text: &str,
    ranges: Vec<(usize, usize)>,
    max_width: usize,
    fallback: impl Fn() -> String,
) -> String {
    if ranges.is_empty() {
        return simple_truncate(text, max_width);
    }

    // Convert byte ranges to char ranges for width budgeting
    let char_indices: Vec<(usize, char)> = text.char_indices().collect();
    let text_char_len = char_indices.len();

    // Map byte offset → char index
    let byte_to_char = |byte_pos: usize| -> usize {
        char_indices
            .iter()
            .position(|(b, _)| *b >= byte_pos)
            .unwrap_or(text_char_len)
    };

    let char_ranges: Vec<(usize, usize)> = ranges
        .iter()
        .map(|(s, e)| (byte_to_char(*s), byte_to_char(*e)))
        .collect();

    // If all matches fit within simple truncation, use that
    let last_match_end = char_ranges.last().map(|(_, e)| *e).unwrap_or(0);
    if last_match_end <= max_width.saturating_sub(1) {
        return simple_truncate(text, max_width);
    }

    // Cluster nearby matches (gap < 20 chars)
    let merge_gap = 20;
    let mut clusters: Vec<(usize, usize)> = Vec::new(); // (char_start, char_end) of cluster
    for &(cs, ce) in &char_ranges {
        if let Some(last) = clusters.last_mut()
            && cs <= last.1 + merge_gap
        {
            last.1 = last.1.max(ce);
            continue;
        }
        clusters.push((cs, ce));
    }

    // Cap at 3 clusters
    clusters.truncate(3);

    // Calculate how many ellipsis chars we need
    let num_clusters = clusters.len();
    // Ellipsis between clusters + possibly leading + possibly trailing
    let match_chars: usize = clusters.iter().map(|(s, e)| e - s).sum();
    // We need at least 1 ellipsis between each pair + leading if first doesn't start at 0
    // + trailing (assume we always need trailing since text was too long)
    let max_ellipsis = num_clusters + 1; // worst case: leading + between each + trailing
    let available_context = max_width
        .saturating_sub(match_chars)
        .saturating_sub(max_ellipsis);
    let padding_per_side = if num_clusters > 0 {
        available_context / (num_clusters * 2)
    } else {
        0
    };

    // Build segments, tracking last position to prevent overlap
    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut last_seg_end: usize = 0;

    for (i, &(cl_start, cl_end)) in clusters.iter().enumerate() {
        let mut seg_start = cl_start.saturating_sub(padding_per_side);
        let seg_end = (cl_end + padding_per_side).min(text_char_len);

        // Prevent overlapping with previous segment
        if i > 0 {
            seg_start = seg_start.max(last_seg_end);
        }

        if (i == 0 && seg_start > 0) || (i > 0 && seg_start > last_seg_end) {
            result.push('…');
        }

        let segment: String = chars[seg_start..seg_end].iter().collect();
        result.push_str(&segment);
        last_seg_end = seg_end;
    }

    // Add trailing ellipsis if we didn't reach the end
    let last_cluster_end = clusters.last().map(|(_, e)| *e).unwrap_or(0);
    if last_cluster_end + padding_per_side < text_char_len {
        result.push('…');
    }

    if UnicodeWidthStr::width(result.as_str()) > max_width {
        return fallback();
    }

    result
}

fn truncate_start(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let ellipsis_width = UnicodeWidthChar::width('…').unwrap_or(1);
    let mut chars = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        chars.push(ch);
        width += ch_width;
    }
    chars.reverse();
    format!("…{}", chars.into_iter().collect::<String>())
}

fn truncate_around_match(text: &str, query: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let ranges = find_normalized_match_ranges(text, query);
    let Some((start, end)) = ranges.first().copied() else {
        return simple_truncate(text, max_width);
    };
    let matched = &text[start..end];
    let matched_width = UnicodeWidthStr::width(matched);
    if matched_width >= max_width {
        return simple_truncate(matched, max_width);
    }

    let ellipsis_budget = usize::from(start > 0) + usize::from(end < text.len());
    let context_budget = max_width.saturating_sub(matched_width + ellipsis_budget);
    let left_budget = context_budget / 2;
    let right_budget = context_budget - left_budget;
    format!(
        "{}{}{}",
        truncate_start(&text[..start], left_budget + usize::from(start > 0)),
        matched,
        simple_truncate(&text[end..], right_budget + usize::from(end < text.len()))
    )
}

/// Build a context string showing snippets around hidden matches in full_text.
///
/// Selection is cluster-based: collect every term hit in `full_text`, group
/// nearby hits into clusters, then rank clusters by:
///
/// 1. how many *missing* (not-in-preview) terms they cover,
/// 2. how many adjacent term pairs they contain (e.g. literal phrase match),
/// 3. total unique-term coverage,
/// 4. tighter span,
/// 5. earlier position.
///
/// This makes the literal phrase `audio generation` win over a far-apart pair
/// of `audio` + `generation` occurrences in unrelated boilerplate.
/// Operates on raw full_text and sanitizes each extracted slice independently.
pub(crate) fn build_literal_context_segments(
    full_text: &str,
    preview: &str,
    query: &HighlightQuery,
    max_width: usize,
) -> Option<String> {
    build_context_segments_with_specs(
        full_text,
        preview,
        &query.literal_context_specs(),
        max_width,
    )
}

#[cfg(test)]
fn build_context_segments_for_query(
    full_text: &str,
    preview: &str,
    query: &HighlightQuery,
    max_width: usize,
) -> Option<String> {
    build_context_segments_with_specs(full_text, preview, &query.context_specs(), max_width)
}

#[cfg(test)]
fn build_context_segments(
    full_text: &str,
    preview: &str,
    query: &str,
    max_width: usize,
) -> Option<String> {
    build_context_segments_with_specs(
        full_text,
        preview,
        &HighlightQuery::normalized_context_specs(query),
        max_width,
    )
}

pub(crate) fn build_context_segments_from_ranges(
    full_text: &str,
    hidden_matches: &[(usize, usize)],
    max_width: usize,
) -> Option<String> {
    if hidden_matches.is_empty() || max_width == 0 {
        return None;
    }

    build_context_segments_from_ranges_unchecked(full_text, hidden_matches, max_width)
}

fn build_context_segments_with_specs(
    full_text: &str,
    preview: &str,
    specs: &[ContextMatchSpec],
    max_width: usize,
) -> Option<String> {
    if specs.is_empty() || max_width == 0 {
        return None;
    }

    let specs = dedupe_context_specs(specs);
    if specs.is_empty() {
        return None;
    }

    let has_normalized_specs = specs
        .iter()
        .any(|spec| matches!(spec, ContextMatchSpec::Normalized(_)));
    let preview_normalized = has_normalized_specs.then(|| NormalizedText::new(preview));
    let full_text_normalized = has_normalized_specs.then(|| NormalizedText::new(full_text));

    let mut missing_mask: u64 = 0;
    let mut missing_count = 0u32;
    for (i, spec) in specs.iter().enumerate() {
        if spec
            .find_ranges(preview, preview_normalized.as_ref())
            .is_empty()
        {
            missing_mask |= 1 << i;
            missing_count += 1;
        }
    }

    let all_hits = find_all_spec_hits(full_text, &specs, full_text_normalized.as_ref());
    if all_hits.is_empty() {
        return None;
    }

    // Fallback: if every term is already visible in the preview, only emit a
    // context line when full_text contains *more* hits than the preview does.
    // We don't try to skip positionally — `preview` is sanitized/truncated and
    // `full_text` is raw, so positional alignment between the two hit streams
    // is unreliable. Instead we let the cluster ranker pick the most
    // informative cluster across the whole document. Worst case it picks one
    // that overlaps preview content, which is still the best snippet we have.
    if missing_count == 0 {
        let preview_hit_count =
            find_all_spec_hits(preview, &specs, preview_normalized.as_ref()).len();
        if all_hits.len() <= preview_hit_count {
            return None;
        }
    }

    let hit_spans: Vec<(usize, usize, usize)> = all_hits
        .iter()
        .map(|hit| (hit.start, hit.end, hit.term_idx))
        .collect();
    let hidden_matches =
        select_hidden_context_ranges(full_text, &hit_spans, missing_mask, missing_count)?;

    build_context_segments_from_ranges_unchecked(full_text, &hidden_matches, max_width)
}

fn build_context_segments_from_ranges_unchecked(
    full_text: &str,
    hidden_matches: &[(usize, usize)],
    max_width: usize,
) -> Option<String> {
    // For each hidden match cluster, extract a context window from raw full_text,
    // then sanitize just that slice
    let num_segments = hidden_matches.len();
    let budget_per_segment = max_width.saturating_sub(num_segments + 1) / num_segments; // reserve for ellipsis

    let mut result = String::new();
    let mut remaining_width = max_width;
    let mut prev_end_byte: usize = 0;

    for (i, &(match_start, match_end)) in hidden_matches.iter().enumerate() {
        let match_char_len = full_text[match_start..match_end].chars().count();
        let context_chars = budget_per_segment
            .saturating_sub(match_char_len)
            .saturating_sub(2) // reserve for "…" on each side
            / 2;

        // Find char boundaries for the context window in raw full_text
        let mut start_byte = full_text[..match_start]
            .char_indices()
            .rev()
            .nth(context_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(0);

        // Prevent overlapping with previous segment
        start_byte = start_byte.max(prev_end_byte);

        let end_byte = full_text[match_end..]
            .char_indices()
            .nth(context_chars)
            .map(|(idx, _)| match_end + idx)
            .unwrap_or(full_text.len())
            .min(full_text.len());

        let snippet = &full_text[start_byte..end_byte];
        let sanitized = sanitize_preview(snippet);

        // Add ellipsis if there's a gap before this segment
        let has_gap = if i == 0 {
            start_byte > 0
        } else {
            start_byte > prev_end_byte
        };
        if has_gap {
            result.push('…');
            remaining_width = remaining_width.saturating_sub(1);
        }

        prev_end_byte = end_byte;

        // Append segment, truncating if needed
        let seg_char_count = sanitized.chars().count();
        if seg_char_count <= remaining_width {
            result.push_str(&sanitized);
            remaining_width = remaining_width.saturating_sub(seg_char_count);
        } else {
            // Truncate this segment to fit
            let budget = remaining_width.saturating_sub(1);
            let trunc: String = sanitized.chars().take(budget).collect();
            result.push_str(&trunc);
            result.push('…');
            remaining_width = 0;
            break;
        }
    }

    // Add trailing ellipsis if last match didn't reach end of full_text
    if remaining_width > 0 {
        let last_end = hidden_matches.last().map(|(_, e)| *e).unwrap_or(0);
        if last_end < full_text.len() {
            result.push('…');
        }
    }

    if result.is_empty() {
        None
    } else {
        // Final safety truncation
        Some(simple_truncate(&result, max_width))
    }
}

/// Sanitize preview text by removing XML-like tags and normalizing whitespace.
/// The separator between two messages' text shows as a space.
pub(crate) fn sanitize_preview(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_tag = false;
    let mut last_was_space = false;

    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            '\n' | '\r' | '\t' | MESSAGE_SEPARATOR => {
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            }
            ' ' => {
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            }
            _ => {
                result.push(ch);
                last_was_space = false;
            }
        }
    }

    result.trim().to_string()
}

/// One match used to compute term coverage and phrase density per cluster.
#[derive(Clone, Copy, Debug)]
struct TermHit {
    start: usize,
    end: usize,
    term_idx: usize,
}

/// Pre-normalized text with char-to-byte mapping for efficient repeated searches.
struct NormalizedText {
    norm_chars: Vec<char>,
    char_map: Vec<(usize, usize)>,
}

impl NormalizedText {
    fn new(text: &str) -> Self {
        let mut norm_chars: Vec<char> = Vec::new();
        let mut char_map: Vec<(usize, usize)> = Vec::new();

        let mut iter = text.char_indices().peekable();
        while let Some((byte_start, ch)) = iter.next() {
            let byte_end = iter.peek().map_or(text.len(), |(i, _)| *i);
            if ch == '_' {
                norm_chars.push(' ');
                char_map.push((byte_start, byte_end));
            } else {
                for lc in ch.to_lowercase() {
                    norm_chars.push(lc);
                    char_map.push((byte_start, byte_end));
                }
            }
        }

        Self {
            norm_chars,
            char_map,
        }
    }

    /// Find all non-overlapping matches of a single term, with left word boundary.
    fn find_term_ranges(&self, term: &str) -> Vec<(usize, usize)> {
        let query_chars: Vec<char> = term.chars().collect();
        if query_chars.is_empty() {
            return Vec::new();
        }

        let query_starts_alnum = query_chars.first().is_some_and(|c| c.is_alphanumeric());
        let mut matches = Vec::new();

        let mut i = 0;
        while i + query_chars.len() <= self.norm_chars.len() {
            if self.norm_chars[i..i + query_chars.len()] == query_chars[..] {
                let prev_is_alnum = i > 0 && self.norm_chars[i - 1].is_alphanumeric();
                let valid_start = !query_starts_alnum || !prev_is_alnum;

                if valid_start {
                    let start_byte = self.char_map[i].0;
                    let end_byte = self.char_map[i + query_chars.len() - 1].1;
                    matches.push((start_byte, end_byte));
                    i += query_chars.len();
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        matches
    }

    /// Find all matches for a multi-word query, sorted and merged.
    fn find_all_ranges(&self, query_normalized: &str) -> Vec<(usize, usize)> {
        let terms: Vec<&str> = query_normalized.split_whitespace().collect();
        if terms.is_empty() {
            return Vec::new();
        }

        let mut all_matches = Vec::new();
        for term in &terms {
            all_matches.extend(self.find_term_ranges(term));
        }

        // Sort and merge overlapping or separator-adjacent ranges.
        // This ensures "run_with_loader" highlights as one span including the underscores
        // when searching "run with loader" (underscores normalized to spaces).
        all_matches.sort_unstable_by_key(|m| m.0);
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(all_matches.len());
        for m in all_matches {
            if let Some(last) = merged.last_mut() {
                if m.0 <= last.1 {
                    // Overlapping — merge
                    last.1 = last.1.max(m.1);
                    continue;
                }
                // Check if the gap between ranges is only separators (_, -, /)
                let gap = &self.norm_chars[..];
                let gap_start = self.byte_to_char_index(last.1);
                let gap_end = self.byte_to_char_index(m.0);
                if gap_start < gap_end
                    && gap[gap_start..gap_end]
                        .iter()
                        .all(|c| *c == ' ' || *c == '_' || *c == '-' || *c == '/')
                {
                    last.1 = m.1;
                    continue;
                }
            }
            merged.push(m);
        }

        merged
    }

    /// Convert a byte offset in the original text to a char index in norm_chars
    fn byte_to_char_index(&self, byte_offset: usize) -> usize {
        self.char_map
            .iter()
            .position(|(start, _)| *start >= byte_offset)
            .unwrap_or(self.char_map.len())
    }
}

/// Find all non-overlapping matches of `query_normalized` in `text` after normalizing `text`.
/// Returns byte ranges in the original `text` for each match.
pub(crate) fn find_normalized_match_ranges(
    text: &str,
    query_normalized: &str,
) -> Vec<(usize, usize)> {
    NormalizedText::new(text).find_all_ranges(query_normalized)
}

#[derive(Debug, Default)]
pub(crate) struct HighlightQuery {
    unquoted: String,
    literals: Vec<Literal>,
}

impl HighlightQuery {
    pub(crate) fn parse(query: &str) -> Self {
        let parsed = ParsedQuery::parse(query);
        let unquoted_terms = parsed.unquoted().split_whitespace().collect::<Vec<_>>();
        let identifier_literals = unquoted_terms
            .iter()
            .copied()
            .filter(|term| term.contains('_'))
            .map(|term| Literal::new(term.to_string()))
            .collect::<Vec<_>>();
        let unquoted = unquoted_terms
            .into_iter()
            .filter(|term| !term.contains('_'))
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            unquoted: normalize_highlight_query(&unquoted),
            literals: parsed
                .literals()
                .iter()
                .cloned()
                .chain(identifier_literals)
                .collect(),
        }
    }

    pub(crate) fn context_text(&self) -> String {
        normalize_highlight_query(
            [self.unquoted.as_str()]
                .into_iter()
                .chain(self.literals.iter().map(Literal::text))
                .collect::<Vec<_>>()
                .join(" ")
                .as_str(),
        )
    }

    #[cfg(test)]
    fn context_specs(&self) -> Vec<ContextMatchSpec> {
        Self::normalized_context_specs(&self.unquoted)
            .into_iter()
            .chain(self.literals.iter().cloned().map(ContextMatchSpec::Literal))
            .collect()
    }

    fn literal_context_specs(&self) -> Vec<ContextMatchSpec> {
        self.literals
            .iter()
            .cloned()
            .map(ContextMatchSpec::Literal)
            .collect()
    }

    pub(crate) fn has_match(&self, text: &str) -> bool {
        !find_normalized_match_ranges(text, &self.unquoted).is_empty()
            || self.has_literal_match(text)
    }

    fn has_literal_match(&self, text: &str) -> bool {
        self.literals.iter().any(|literal| literal.matches(text))
    }

    pub(crate) fn needs_literal_context(&self, preview: &str) -> bool {
        self.literals
            .iter()
            .any(|literal| !literal.matches(preview))
    }

    pub(crate) fn match_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let mut ranges = find_normalized_match_ranges(text, &self.unquoted);
        ranges.extend(match_literal_ranges(text, &self.literals));
        merge_match_ranges(ranges)
    }

    #[cfg(test)]
    fn normalized_context_specs(query: &str) -> Vec<ContextMatchSpec> {
        query
            .split_whitespace()
            .map(|term| ContextMatchSpec::Normalized(term.to_string()))
            .collect()
    }
}

#[allow(dead_code)]
#[derive(Clone)]
enum ContextMatchSpec {
    Normalized(String),
    Literal(Literal),
}

impl ContextMatchSpec {
    fn key(&self) -> (&str, bool) {
        match self {
            Self::Normalized(term) => (term.as_str(), false),
            Self::Literal(literal) => (literal.text(), true),
        }
    }

    fn find_ranges(&self, text: &str, normalized: Option<&NormalizedText>) -> Vec<(usize, usize)> {
        match self {
            Self::Normalized(term) => normalized
                .map(|normalized| normalized.find_all_ranges(term))
                .unwrap_or_else(|| find_normalized_match_ranges(text, term)),
            Self::Literal(literal) => literal.match_ranges(text),
        }
    }
}

fn dedupe_context_specs(specs: &[ContextMatchSpec]) -> Vec<ContextMatchSpec> {
    let mut deduped = Vec::new();
    for spec in specs {
        let (text, is_literal) = spec.key();
        if !deduped.iter().any(|existing: &ContextMatchSpec| {
            let (existing_text, existing_is_literal) = existing.key();
            existing_is_literal == is_literal && existing_text.eq_ignore_ascii_case(text)
        }) {
            deduped.push(spec.clone());
            if deduped.len() == 64 {
                break;
            }
        }
    }
    deduped
}

fn find_all_spec_hits(
    text: &str,
    specs: &[ContextMatchSpec],
    normalized: Option<&NormalizedText>,
) -> Vec<TermHit> {
    let mut all_hits = Vec::new();
    for (term_idx, spec) in specs.iter().enumerate() {
        all_hits.extend(
            spec.find_ranges(text, normalized)
                .into_iter()
                .map(|(start, end)| TermHit {
                    start,
                    end,
                    term_idx,
                }),
        );
    }
    all_hits.sort_unstable_by_key(|hit| hit.start);
    all_hits
}

fn normalize_highlight_query(query: &str) -> String {
    normalize_for_search(query)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn merge_match_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable_by_key(|range| range.0);
    let mut merged = Vec::<(usize, usize)>::new();
    for (start, end) in ranges {
        if start >= end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separator_between_two_messages_previews_as_space() {
        assert_eq!(
            sanitize_preview("first reply \u{1e} second reply"),
            "first reply second reply"
        );
    }

    #[test]
    fn find_normalized_ranges_phrase() {
        let text = "hello red team world";
        let ranges = find_normalized_match_ranges(text, "red team");
        // Adjacent words separated by space merge into one range
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "red team");
    }

    #[test]
    fn find_normalized_ranges_prefix_match() {
        // "red" matches at start of "redaction" (prefix), "team" has no match
        let ranges = find_normalized_match_ranges("Extend log redaction to cover", "red team");
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            &"Extend log redaction to cover"[ranges[0].0..ranges[0].1],
            "red"
        );
    }

    #[test]
    fn find_normalized_ranges_underscore() {
        let text = "set red_team flag";
        let ranges = find_normalized_match_ranges(text, "red team");
        // Adjacent words separated by underscore merge into one range
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "red_team");
    }

    // --- build_match_segments tests ---

    #[test]
    fn match_segments_no_query() {
        let text = "hello world this is a long text";
        let result = build_match_segments(text, "", 20);
        assert_eq!(result, simple_truncate(text, 20));
    }

    #[test]
    fn match_segments_no_matches() {
        let text = "hello world this is a long text";
        let result = build_match_segments(text, "xyz", 20);
        assert_eq!(result, simple_truncate(text, 20));
    }

    #[test]
    fn match_segments_all_fit() {
        // All matches within max_width, should use simple truncation
        let text = "foo bar baz and more text";
        let result = build_match_segments(text, "foo", 30);
        assert_eq!(result, text);
    }

    #[test]
    fn match_segments_distant_matches() {
        // Two matches far apart — should produce segmented output with "…"
        let text = "start secrets aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll mmm nnn ooo ppp plot end";
        let result = build_match_segments(text, "secrets plot", 40);
        assert!(result.contains("secrets"));
        assert!(result.contains("plot"));
        assert!(result.contains("…"));
        assert!(result.chars().count() <= 40);
    }

    #[test]
    fn match_segments_close_matches_merged() {
        // Two matches close together — should be one segment
        let text =
            "aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll secrets and plot end more text here";
        let result = build_match_segments(text, "secrets plot", 50);
        assert!(result.contains("secrets"));
        assert!(result.contains("plot"));
    }

    // --- build_context_segments tests ---

    #[test]
    fn context_segments_none_when_all_visible() {
        let full_text = "red team exercise";
        let preview = "red team exercise";
        let result = build_context_segments(full_text, preview, "red team", 80);
        assert!(result.is_none());
    }

    #[test]
    fn context_segments_one_hidden_match() {
        let full_text = "redaction stuff here and then red team exercise later";
        let preview = "redaction stuff here and then";
        let result = build_context_segments(full_text, preview, "red team", 80);
        assert!(result.is_some());
        let ctx = result.unwrap();
        // Should contain "red" and/or "team" from the hidden match area
        assert!(ctx.contains("red") || ctx.contains("team"));
        assert!(ctx.contains("…"));
    }

    #[test]
    fn context_segments_multiword_hidden() {
        let full_text = "I want secrets from the vault, and later write me a plot twist";
        let preview = "I want secrets from the";
        // Preview has "secrets", hidden has "plot" — context should prioritize "plot"
        let result = build_context_segments(full_text, preview, "secrets plot", 80);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(ctx.contains("plot"));
    }

    #[test]
    fn context_segments_prioritizes_missing_terms() {
        // "secrets" appears many times but "plot" only once deep in text.
        // Preview shows "secrets" — context should show "plot", not more "secrets".
        let full_text = "secrets here and secrets there and secrets everywhere and finally a plot twist at the end";
        let preview = "secrets here and secrets there";
        let result = build_context_segments(full_text, preview, "secrets plot", 80);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(
            ctx.contains("plot"),
            "context should contain 'plot' but was: {ctx}"
        );
    }

    #[test]
    fn context_segments_empty_query() {
        let result = build_context_segments("some text", "some", "", 80);
        assert!(result.is_none());
    }

    #[test]
    fn context_segments_prefers_adjacent_phrase_over_distant_terms() {
        // "audio" and "generation" each appear early in unrelated boilerplate,
        // and the literal phrase "audio generation" appears much later. The
        // snippet must surface the phrase, not the early independent hits.
        let mut full_text = String::new();
        full_text.push_str("Card generation is supported. -field:Audio is a filter. ");
        full_text.push_str(&"junk ".repeat(20));
        full_text
            .push_str("First-class audio generation (OpenAI TTS) and image support is missing. ");
        full_text.push_str(&"junk ".repeat(20));
        let preview = "Some unrelated preview line about deck workflow";
        let result = build_context_segments(&full_text, preview, "audio generation", 120);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(
            ctx.contains("audio generation"),
            "context should contain the literal phrase 'audio generation', got: {ctx}"
        );
    }

    #[test]
    fn context_segments_phrase_inside_markdown_bold() {
        // The phrase appears inside `**...**` (markdown bold). Adjacency must
        // still be detected — markdown punctuation is a word boundary.
        let mut full_text = String::new();
        full_text.push_str("audio is mentioned. generation is mentioned separately. ");
        full_text.push_str(&"x ".repeat(30));
        full_text.push_str("**Audio generation** is the actual recommendation here.");
        let preview = "boring preview text";
        let result = build_context_segments(&full_text, preview, "audio generation", 120);
        assert!(result.is_some());
        let ctx = result.unwrap();
        let lower = ctx.to_lowercase();
        assert!(
            lower.contains("audio generation"),
            "context should contain the phrase 'audio generation', got: {ctx}"
        );
    }

    #[test]
    fn context_segments_skips_clusters_only_covering_visible_terms() {
        // "audio" is in the preview already. A standalone "audio" cluster
        // should not be selected — the snippet should surface "generation"
        // (the missing term), preferably alongside "audio" if a phrase exists.
        let full_text = "audio first. then later generation alone. and finally audio generation together at the end.";
        let preview = "audio first. then later";
        let result = build_context_segments(full_text, preview, "audio generation", 120);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(
            ctx.contains("generation"),
            "context should contain 'generation', got: {ctx}"
        );
    }

    #[test]
    fn context_segments_distant_terms_dont_count_as_adjacent() {
        // Two clusters with the same unique_count, but only one has the
        // terms actually adjacent. The adjacent one must win.
        let mut full_text = String::new();
        // Cluster A: alpha and beta with junk between (within merge_gap=50
        // bytes but not adjacent — `aaaa` is alphanumeric in the gap).
        full_text.push_str("alpha aaaa bbbb cccc dddd beta ");
        full_text.push_str(&"x ".repeat(40));
        // Cluster B: literal phrase
        full_text.push_str("alpha beta together here");
        let preview = "boring preview line";
        let result = build_context_segments(&full_text, preview, "alpha beta", 100);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(
            ctx.contains("alpha beta together")
                || ctx.ends_with("here")
                || ctx.contains("beta together"),
            "expected the literal phrase cluster to be selected, got: {ctx}"
        );
    }

    #[test]
    fn context_segments_dedupes_query_terms() {
        // Repeated query terms must not inflate uniqueness/adjacency math.
        let full_text = "alpha alpha and later beta the end";
        let preview = "preview only";
        let r1 = build_context_segments(full_text, preview, "alpha alpha beta", 80);
        let r2 = build_context_segments(full_text, preview, "alpha beta", 80);
        // Should produce the same context — duplicates are folded away.
        assert_eq!(r1, r2);
    }

    #[test]
    fn context_segments_underscore_phrase_still_detected() {
        // Underscores normalize to spaces, so `audio_generation` should be
        // detected as the adjacent phrase `audio generation`.
        let mut full_text = String::new();
        full_text.push_str("audio early. generation early. ");
        full_text.push_str(&"x ".repeat(30));
        full_text.push_str("the relevant audio_generation pipeline lives here.");
        let preview = "preview only";
        let result = build_context_segments(&full_text, preview, "audio generation", 120);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(
            ctx.contains("audio_generation") || ctx.contains("audio generation"),
            "context should contain the adjacent occurrence, got: {ctx}"
        );
    }

    #[test]
    fn context_query_quoted_literal_preserves_punctuation_contract() {
        let query = HighlightQuery::parse("\"audio_generation\"");
        let mut full_text = String::new();
        full_text.push_str("punctuation-stripped audio generation appears early. ");
        full_text.push_str(&"x ".repeat(30));
        full_text.push_str("the exact audio_generation literal appears here.");
        let preview = "preview only";

        let result = build_context_segments_for_query(&full_text, preview, &query, 120);

        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(ctx.contains("audio_generation"), "got: {ctx}");
        assert!(
            !ctx.contains("audio generation appears early"),
            "got: {ctx}"
        );
    }

    // --- word boundary tests ---

    #[test]
    fn word_boundary_rejects_mid_word() {
        // "red" should not match inside "fired" (not at word start)
        let ranges = find_normalized_match_ranges("fired and tired", "red");
        assert_eq!(ranges.len(), 0);
    }

    #[test]
    fn word_boundary_allows_prefix() {
        // "red" matches at start of "redaction" (prefix matching)
        let ranges = find_normalized_match_ranges("redaction plan", "red");
        assert_eq!(ranges.len(), 1);
        assert_eq!(&"redaction plan"[ranges[0].0..ranges[0].1], "red");
    }

    #[test]
    fn word_boundary_accepts_whole_word() {
        let ranges = find_normalized_match_ranges("the red fox", "red");
        assert_eq!(ranges.len(), 1);
        assert_eq!(&"the red fox"[ranges[0].0..ranges[0].1], "red");
    }

    #[test]
    fn word_boundary_accepts_punctuation_adjacent() {
        // "red" after punctuation should match
        let ranges = find_normalized_match_ranges("it was (red) not blue", "red");
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn word_boundary_start_end_of_string() {
        let ranges = find_normalized_match_ranges("red", "red");
        assert_eq!(ranges.len(), 1);
        let ranges = find_normalized_match_ranges("red fox", "red");
        assert_eq!(ranges.len(), 1);
        let ranges = find_normalized_match_ranges("the red", "red");
        assert_eq!(ranges.len(), 1);
    }
}
