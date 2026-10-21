pub mod evidence;
pub mod lexical;
#[cfg(test)]
mod list_search_coverage;
pub mod literal;
pub mod mode;
pub mod preview;
pub mod query;
#[cfg(test)]
pub mod test_fixtures;

pub use crate::text_match::{is_word_separator, normalize_for_search};
pub use evidence::{LexicalEvidence, build_lexical_evidence};
pub use lexical::{
    SearchableConversation, is_uuid, precompute_agent_search_text, precompute_search_text,
    searchable_conversation, session_id_for_lookup, session_id_matches, try_agent_search,
    try_debug_search, try_search,
};
#[cfg(test)]
pub use lexical::{agent_search, search};
