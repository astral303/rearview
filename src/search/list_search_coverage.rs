//! The session list's search text over each registered agent's every-record
//! session: it holds each searched marker and no hidden one.

use crate::history::Conversation;
use crate::history::provider::contract_tests::every_record_sessions;
use crate::search::normalize_for_search;

fn is_in_list_search_text(row: &Conversation, text: &str) -> bool {
    row.search_text_lower.contains(&normalize_for_search(text))
}

#[test]
fn the_session_list_search_finds_every_searched_marker_and_no_hidden_marker() {
    let mut gaps = Vec::new();
    for session in every_record_sessions() {
        let row = &session.row;
        for marker in session.markers.iter().filter(|marker| marker.searched) {
            if !is_in_list_search_text(row, marker.text) {
                gaps.push(format!("{:?} misses the {}", row.source, marker.record));
            }
        }
        for (record, text) in session.hidden {
            if is_in_list_search_text(row, text) {
                gaps.push(format!("{:?} finds the hidden {record}", row.source));
            }
        }
    }

    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}
