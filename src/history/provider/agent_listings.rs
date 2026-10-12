//! Checks the lists of supported agents kept outside the registry. The rows
//! of the README's `## Supported agents` table match the registered agents
//! by list label, and their current-session cells agree with the registry.
//! The README introduction and the crate description name exactly the
//! agents in the table's Agent column.

use super::markdown_outline::{Block, Table, outline};
use super::providers;
use pulldown_cmark::HeadingLevel;
use regex::Regex;
use std::sync::LazyLock;

const README: &str = include_str!("../../../README.md");
const CARGO_TOML: &str = include_str!("../../../Cargo.toml");
const SUPPORTED_AGENTS_HEADING: &str = "Supported agents";
const AGENT_COLUMN: &str = "Agent";
const LIST_LABEL_COLUMN: &str = "List label";
const CURRENT_SESSION_COLUMN: &str = "Current session";
const MARKED: &str = "Yes";
const UNMARKED: &str = "—";
const INTRO_PREFIX: &str = "rearview supports ";
const DESCRIPTION_PREFIX: &str = "Browse and search ";
const DESCRIPTION_SUFFIX: &str = " conversation history";
const UNREGISTERED_LABEL: &str = "an unregistered list label";
const MISSING_FROM_TABLE: &str = "an agent missing from the `Supported agents` table";

/// One registered agent's facts, as the README states them.
struct Agent<'a> {
    list_label: &'a str,
    sets_session_variable: bool,
}

fn registered_agents() -> Vec<Agent<'static>> {
    providers()
        .iter()
        .map(|provider| Agent {
            list_label: provider.labels().list,
            sets_session_variable: provider.current_session_env_var().is_some(),
        })
        .collect()
}

fn readme_violations(markdown: &str, registered: &[Agent]) -> Vec<String> {
    let blocks = outline(markdown);
    let table = match SupportedAgentsTable::from_blocks(&blocks) {
        Ok(table) => table,
        Err(violations) => return violations,
    };
    let mut violations = table.row_violations(registered);
    match intro_names(&blocks) {
        Some(names) => violations.extend(listing_violations(
            "the README introduction",
            &names,
            &table.agent_names(),
            MISSING_FROM_TABLE,
        )),
        None => violations.push(format!(
            "the README has no paragraph starting `{}`",
            INTRO_PREFIX.trim_end()
        )),
    }
    violations
}

fn description_violations(description: &str, table_agents: &[&str]) -> Vec<String> {
    let listed = description
        .strip_prefix(DESCRIPTION_PREFIX)
        .and_then(|rest| rest.split_once(DESCRIPTION_SUFFIX))
        .map(|(list, _)| names_in_list(list));
    match listed {
        Some(names) => listing_violations(
            "the crate description",
            &names,
            table_agents,
            MISSING_FROM_TABLE,
        ),
        None => vec![format!(
            "the crate description no longer reads `{DESCRIPTION_PREFIX}<agents>{DESCRIPTION_SUFFIX}…`"
        )],
    }
}

/// The `## Supported agents` table's rows, with link Markdown stripped from
/// their cells.
struct SupportedAgentsTable {
    rows: Vec<SupportedAgentRow>,
}

struct SupportedAgentRow {
    agent: String,
    list_label: String,
    current_session: String,
}

impl SupportedAgentsTable {
    fn from_blocks(blocks: &[Block]) -> Result<Self, Vec<String>> {
        let table = supported_agents_table(blocks).ok_or_else(|| {
            vec![format!(
                "the README has no table under `## {SUPPORTED_AGENTS_HEADING}`"
            )]
        })?;
        let column = |name: &str| {
            table
                .header
                .iter()
                .position(|heading| heading == name)
                .ok_or_else(|| {
                    format!("the `## {SUPPORTED_AGENTS_HEADING}` table has no `{name}` column")
                })
        };
        let (agent, list_label, current_session) = match (
            column(AGENT_COLUMN),
            column(LIST_LABEL_COLUMN),
            column(CURRENT_SESSION_COLUMN),
        ) {
            (Ok(agent), Ok(list_label), Ok(current_session)) => {
                (agent, list_label, current_session)
            }
            (agent, list_label, current_session) => {
                return Err([agent.err(), list_label.err(), current_session.err()]
                    .into_iter()
                    .flatten()
                    .collect());
            }
        };
        let cell = |row: &[String], index: usize| {
            without_link_markdown(row.get(index).map_or("", String::as_str))
        };
        Ok(Self {
            rows: table
                .rows
                .iter()
                .map(|row| SupportedAgentRow {
                    agent: cell(row, agent),
                    list_label: cell(row, list_label),
                    current_session: cell(row, current_session),
                })
                .collect(),
        })
    }

    fn agent_names(&self) -> Vec<&str> {
        self.rows.iter().map(|row| row.agent.as_str()).collect()
    }

    /// Rows match registered agents by list label; a matched row's
    /// current-session cell is `Yes` exactly when its agent sets a session
    /// variable.
    fn row_violations(&self, registered: &[Agent]) -> Vec<String> {
        let listed: Vec<String> = self.rows.iter().map(|row| row.list_label.clone()).collect();
        let labels: Vec<&str> = registered.iter().map(|agent| agent.list_label).collect();
        let mut violations = listing_violations(
            &format!("the `{SUPPORTED_AGENTS_HEADING}` table's `{LIST_LABEL_COLUMN}` column"),
            &listed,
            &labels,
            UNREGISTERED_LABEL,
        );
        for agent in registered {
            let mut rows = self
                .rows
                .iter()
                .filter(|row| row.list_label == agent.list_label);
            let (Some(row), None) = (rows.next(), rows.next()) else {
                continue;
            };
            let expected = if agent.sets_session_variable {
                MARKED
            } else {
                UNMARKED
            };
            if row.current_session != expected {
                violations.push(format!(
                    "{}: \"{CURRENT_SESSION_COLUMN}\" is `{}`; expected `{expected}`",
                    row.agent, row.current_session
                ));
            }
        }
        violations
    }
}

/// One message per name in `expected` that `listed` leaves out or repeats,
/// and per name in `listed` missing from `expected`; `unknown` describes
/// such a name.
fn listing_violations(
    place: &str,
    listed: &[String],
    expected: &[&str],
    unknown: &str,
) -> Vec<String> {
    let unexpected = listed
        .iter()
        .filter(|name| !expected.contains(&name.as_str()))
        .map(|name| {
            format!(
                "{place} names {name}, {unknown}; expected: {}",
                expected.join(", ")
            )
        });
    let miscounted = expected.iter().filter_map(|name| {
        match listed.iter().filter(|listed| listed == name).count() {
            0 => Some(format!("{place} leaves out {name}")),
            1 => None,
            count => Some(format!("{place} names {name} {count} times")),
        }
    });
    unexpected.chain(miscounted).collect()
}

/// `text` with each Markdown link replaced by its text.
fn without_link_markdown(text: &str) -> String {
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").expect("valid link pattern"));
    LINK.replace_all(text, "$1").into_owned()
}

/// The names in the first sentence of the paragraph starting
/// [`INTRO_PREFIX`].
fn intro_names(blocks: &[Block]) -> Option<Vec<String>> {
    blocks.iter().find_map(|block| match block {
        Block::Paragraph(text) => {
            let plain = without_link_markdown(text.strip_prefix(INTRO_PREFIX)?);
            let sentence = plain.split(". ").next().unwrap_or_default();
            Some(names_in_list(sentence.trim_end_matches('.')))
        }
        _ => None,
    })
}

/// The first table under `## Supported agents`.
fn supported_agents_table(blocks: &[Block]) -> Option<&Table> {
    let mut in_section = false;
    blocks.iter().find_map(|block| match block {
        Block::Heading { level, text } if *level == HeadingLevel::H2 => {
            in_section = text == SUPPORTED_AGENTS_HEADING;
            None
        }
        Block::Table(table) if in_section => Some(table),
        _ => None,
    })
}

/// The names in `A, B, and C`, `A, B and C` or `A and B`.
fn names_in_list(list: &str) -> Vec<String> {
    let mut names: Vec<String> = list.split(", ").map(str::to_owned).collect();
    if let Some(last) = names.pop() {
        match last.strip_prefix("and ") {
            Some(name) => names.push(name.to_owned()),
            None => names.extend(last.split(" and ").map(str::to_owned)),
        }
    }
    names
}

fn crate_description() -> String {
    let manifest: toml::Table = CARGO_TOML.parse().expect("Cargo.toml parses");
    manifest["package"]["description"]
        .as_str()
        .expect("the package has a description")
        .to_owned()
}

#[test]
fn readme_and_crate_description_list_every_registered_agent() {
    let mut violations = readme_violations(README, &registered_agents());
    if let Ok(table) = SupportedAgentsTable::from_blocks(&outline(README)) {
        violations.extend(description_violations(
            &crate_description(),
            &table.agent_names(),
        ));
    }

    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

const SAMPLE_AGENTS: &[Agent] = &[
    Agent {
        list_label: "AL",
        sets_session_variable: true,
    },
    Agent {
        list_label: "BE",
        sets_session_variable: false,
    },
];

const SAMPLE_README: &str = "\
`rearview` supports Alpha Code and [Beta](https://example.com/beta). It finds them.

## Supported agents

| Agent      | List label | Current session | Fork |
|------------|------------|-----------------|------|
| Alpha Code | `AL`       | Yes             | yes  |
| Beta       | `BE`       | —               | no   |

## Install
";

fn readme_violations_after(replacements: &[(&str, &str)]) -> Vec<String> {
    let markdown = replacements
        .iter()
        .fold(SAMPLE_README.to_owned(), |markdown, (from, to)| {
            assert!(markdown.contains(from), "the sample holds `{from}`");
            markdown.replacen(from, to, 1)
        });
    readme_violations(&markdown, SAMPLE_AGENTS)
}

#[test]
fn a_consistent_readme_has_no_violations() {
    assert_eq!(
        readme_violations(SAMPLE_README, SAMPLE_AGENTS),
        Vec::<String>::new()
    );
}

#[test]
fn a_registered_agent_without_a_table_row_is_reported() {
    let violations =
        readme_violations_after(&[("| Beta       | `BE`       | —               | no   |\n", "")]);

    assert_eq!(
        violations,
        [
            "the `Supported agents` table's `List label` column leaves out BE",
            "the README introduction names Beta, an agent missing from the `Supported agents` table; expected: Alpha Code",
        ]
    );
}

#[test]
fn a_second_table_row_for_an_agent_is_reported() {
    let violations = readme_violations_after(&[(
        "| Beta       |",
        "| Beta       | `BE`       | —               | no   |\n| Beta       |",
    )]);

    assert_eq!(
        violations,
        ["the `Supported agents` table's `List label` column names BE 2 times"]
    );
}

#[test]
fn a_table_row_with_an_unregistered_list_label_is_reported() {
    let violations = readme_violations_after(&[(
        "| Beta       |",
        "| Gamma      | `GA`       | —               | no   |\n| Beta       |",
    )]);

    assert_eq!(
        violations,
        [
            "the `Supported agents` table's `List label` column names GA, an unregistered list label; expected: AL, BE",
            "the README introduction leaves out Gamma",
        ]
    );
}

#[test]
fn a_list_label_other_than_the_registry_label_is_reported() {
    let violations = readme_violations_after(&[("`BE`", "`BT`")]);

    assert_eq!(
        violations,
        [
            "the `Supported agents` table's `List label` column names BT, an unregistered list label; expected: AL, BE",
            "the `Supported agents` table's `List label` column leaves out BE",
        ]
    );
}

#[test]
fn a_linked_agent_cell_reads_as_its_text() {
    let violations =
        readme_violations_after(&[("| Beta       |", "| [Beta](https://example.com/beta) |")]);

    assert_eq!(violations, Vec::<String>::new());
}

#[test]
fn a_renamed_column_is_reported() {
    let violations = readme_violations_after(&[("| List label |", "| Label      |")]);

    assert_eq!(
        violations,
        ["the `## Supported agents` table has no `List label` column"]
    );
}

#[test]
fn a_readme_without_the_introduction_paragraph_is_reported() {
    let violations = readme_violations_after(&[("`rearview` supports", "`rearview` reads")]);

    assert_eq!(
        violations,
        ["the README has no paragraph starting `rearview supports`"]
    );
}

#[test]
fn a_current_session_mark_disagreeing_with_the_registry_is_reported() {
    let violations =
        readme_violations_after(&[("| —               | no", "| Yes             | no")]);

    assert_eq!(
        violations,
        ["Beta: \"Current session\" is `Yes`; expected `—`"]
    );
}

#[test]
fn an_introduction_that_leaves_out_an_agent_is_reported() {
    let violations = readme_violations_after(&[(
        "Alpha Code and [Beta](https://example.com/beta).",
        "Alpha Code.",
    )]);

    assert_eq!(violations, ["the README introduction leaves out Beta"]);
}

#[test]
fn an_introduction_that_names_an_agent_missing_from_the_table_is_reported() {
    let violations =
        readme_violations_after(&[("Alpha Code and [Beta]", "Alpha Code, Gamma, and [Beta]")]);

    assert_eq!(
        violations,
        [
            "the README introduction names Gamma, an agent missing from the `Supported agents` table; expected: Alpha Code, Beta"
        ]
    );
}

#[test]
fn a_readme_without_the_supported_agents_table_is_reported() {
    let violations = readme_violations_after(&[("## Supported agents", "## Agents")]);

    assert_eq!(
        violations,
        ["the README has no table under `## Supported agents`"]
    );
}

#[test]
fn a_crate_description_that_leaves_out_an_agent_is_reported() {
    let violations = description_violations(
        "Browse and search Alpha Code conversation history from the terminal.",
        &["Alpha Code", "Beta"],
    );

    assert_eq!(violations, ["the crate description leaves out Beta"]);
}

#[test]
fn a_list_reads_with_or_without_a_serial_comma() {
    assert_eq!(names_in_list("Alpha"), ["Alpha"]);
    assert_eq!(names_in_list("Alpha and Beta"), ["Alpha", "Beta"]);
    assert_eq!(
        names_in_list("Alpha, Beta and Gamma"),
        ["Alpha", "Beta", "Gamma"]
    );
    assert_eq!(
        names_in_list("Alpha, Beta, and Gamma"),
        ["Alpha", "Beta", "Gamma"]
    );
}
