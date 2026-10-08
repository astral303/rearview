pub mod evidence;
pub mod lexical;
pub mod literal;
pub mod mode;
pub mod preview;
pub mod query;
#[cfg(test)]
pub mod test_fixtures;

pub use crate::text_match::{is_word_separator, normalize_for_search};
pub use evidence::{LexicalEvidence, build_lexical_evidence};
pub use lexical::{
    LexicalDebugSearch, SearchableConversation, agent_search, debug_agent_search, debug_search,
    is_uuid, precompute_agent_search_text, precompute_search_text, score_text_debug, search,
    searchable_conversation, session_id_for_lookup, session_id_matches,
};
