use super::{Conversation, MESSAGE_SEPARATOR, SEMANTIC_ROUTE_VERSION};
use crate::semantic::test_fixtures::SemanticConversationFixture;

/// A row holding exactly the inputs of its route text.
fn row(
    title: Option<&str>,
    summary: Option<&str>,
    messages: &[&str],
    agent_text: &str,
) -> Conversation {
    let mut fixture = SemanticConversationFixture::new("/projects/route/session.jsonl", ["turn"]);
    if let Some(title) = title {
        fixture = fixture.with_title(title);
    }
    let mut row = fixture.with_summary(summary).build();
    if title.is_none() {
        row.custom_title = None;
    }
    row.full_text = messages.join(&MESSAGE_SEPARATOR.to_string());
    row.agent_search_text = agent_text.to_owned();
    row
}

/// A row whose one message runs past the route's limits: short words past
/// the keyword count, long words past the keywords' characters, and both
/// past the excerpt's.
fn long_row(stem: &str, base: usize) -> Conversation {
    let text = (0..3_000)
        .map(|index| format!("{stem}{} ", base + index % 150 + index % 7))
        .collect::<String>();
    row(None, None, &[&text], "")
}

/// Fails when the route algorithm's output changes without a new
/// `SEMANTIC_ROUTE_VERSION`: the session cache would otherwise keep each
/// stored route as the old algorithm computed it. The inputs are this
/// test's own, so a change to a provider's fixtures leaves it passing.
#[test]
fn route_text_changes_only_with_semantic_route_version() {
    use sha2::{Digest, Sha256};
    let rows = [
        row(
            Some("Cache migration plan"),
            Some("Moves the session cache to schema 32"),
            &[
                "Why does the search cache rebuild after every upgrade?",
                "The schema version changed, so the cache rebuilds its tables.",
                "Then keep the parts table and rebuild only the routes.",
            ],
            "sub-agent: audit the migration steps for the parts table",
        ),
        row(
            None,
            None,
            &["their these they this through using very what when where which while"],
            "",
        ),
        row(
            None,
            Some("A summary alone"),
            &["keyword keyword keyword repeated repeated other"],
            "keyword from a sub-agent",
        ),
        long_row("w", 1_000),
        long_row("keyword", 1_000_000),
    ];
    let mut routes = Sha256::new();
    for row in &rows {
        let route = row.semantic_route_text();
        assert!(!route.is_empty(), "each row has a route text");
        routes.update(route.len().to_le_bytes());
        routes.update(route.as_bytes());
    }

    assert_eq!(
        (SEMANTIC_ROUTE_VERSION, format!("{:x}", routes.finalize())),
        (
            1,
            "5ea97eb13759adac8790a6dfecc14bba3996e783802a2a828d71606529263be7".to_owned()
        ),
        "the route texts change only with a new SEMANTIC_ROUTE_VERSION: bump it, then \
         record the new digest"
    );
}
