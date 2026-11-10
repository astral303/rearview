pub(crate) mod byte_search;
pub mod evidence;
pub mod lexical;
#[cfg(test)]
mod list_search_coverage;
pub mod literal;
pub mod mode;
pub mod preview;
pub mod query;
pub(crate) mod separated_texts;
#[cfg(test)]
pub mod test_fixtures;

pub use crate::text_match::{is_word_separator, normalize_for_search};
pub use evidence::{LexicalEvidence, build_session_list_evidence};
pub use lexical::{
    AgentRanking, RetrievalTerms, SearchTextsInMemory, SearchableConversation, SessionHitMessages,
    is_uuid, precompute_agent_search_text, precompute_search_text, search_with_texts,
    searchable_conversation, session_id_for_lookup, session_id_matches, try_agent_search,
    try_debug_search,
};
pub(crate) use lexical::{CopiesToRead, SessionsWithoutTexts, TextsRead};
#[cfg(test)]
pub use lexical::{agent_search, search};
