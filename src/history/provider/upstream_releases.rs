//! Checks the `## Upstream releases` table in PROVIDERS.md against the
//! registry, and each agent's row against its `### Upstream findings: <agent>`
//! subsection, by the rules PROVIDERS.md states under that table.

use super::providers;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::cmp::Ordering;

const PROVIDERS_MD: &str = include_str!("../../../PROVIDERS.md");
const RELEASES_HEADING: &str = "Upstream releases";
const FINDINGS_HEADING_PREFIX: &str = "Upstream findings: ";
const AGENT_COLUMN: &str = "Agent";
const REVIEWED_COLUMN: &str = "Reviewed through";
const ACCOUNTED_COLUMN: &str = "Accounted for through";
const NOT_REVIEWED: &str = "—";

/// One message per rule `markdown` breaks; `registered` holds each agent's
/// `labels().display`.
fn violations(markdown: &str, registered: &[&str]) -> Vec<String> {
    let releases = UpstreamReleases::read(markdown);
    let rows = match releases.rows() {
        Ok(rows) => rows,
        Err(violation) => return vec![violation],
    };
    let mut violations = row_count_violations(&rows, registered);
    violations.extend(findings_section_violations(&releases.findings, registered));
    for row in rows
        .iter()
        .filter(|row| registered.contains(&row.agent.as_str()))
    {
        violations.extend(agent_violations(row, releases.findings_of(&row.agent)));
    }
    violations
}

fn row_count_violations(rows: &[Row], registered: &[&str]) -> Vec<String> {
    let unregistered = rows
        .iter()
        .filter(|row| !registered.contains(&row.agent.as_str()))
        .map(|row| {
            format!(
                "the row for {} names no registered agent; {}",
                row.agent,
                expected_agents(registered)
            )
        });
    let miscounted = registered.iter().filter_map(|agent| {
        match rows.iter().filter(|row| row.agent == *agent).count() {
            0 => Some(format!("{agent} has no row")),
            1 => None,
            count => Some(format!("{agent} has {count} rows")),
        }
    });
    unregistered.chain(miscounted).collect()
}

fn findings_section_violations(sections: &[FindingsSection], registered: &[&str]) -> Vec<String> {
    let unregistered = sections
        .iter()
        .filter(|section| !registered.contains(&section.agent.as_str()))
        .map(|section| {
            format!(
                "`### {FINDINGS_HEADING_PREFIX}{}` names no registered agent; {}",
                section.agent,
                expected_agents(registered)
            )
        });
    let miscounted = registered.iter().filter_map(|agent| {
        match sections
            .iter()
            .filter(|section| section.agent == *agent)
            .count()
        {
            0 => Some(format!(
                "{agent} has no `### {FINDINGS_HEADING_PREFIX}{agent}` subsection"
            )),
            1 => None,
            count => Some(format!("{agent} has {count} findings subsections")),
        }
    });
    unregistered.chain(miscounted).collect()
}

fn expected_agents(registered: &[&str]) -> String {
    format!("expected one of: {}", registered.join(", "))
}

fn agent_violations(row: &Row, lines: Vec<&FindingLine>) -> Vec<String> {
    let agent = &row.agent;
    let (reviewed, accounted) = match (
        VersionCell::parse(agent, REVIEWED_COLUMN, &row.reviewed),
        VersionCell::parse(agent, ACCOUNTED_COLUMN, &row.accounted),
    ) {
        (Ok(reviewed), Ok(accounted)) => (reviewed, accounted),
        (reviewed, accounted) => {
            return reviewed.err().into_iter().chain(accounted.err()).collect();
        }
    };
    let mut violations = Vec::new();
    let mut findings = Vec::new();
    for line in lines {
        match Finding::parse(agent, line) {
            Ok(finding) => findings.push(finding),
            Err(violation) => violations.push(violation),
        }
    }
    match (reviewed, accounted) {
        (VersionCell::NotReviewed, VersionCell::NotReviewed) => {
            violations.extend(unreviewed_agent_violations(agent, &findings));
        }
        (VersionCell::Version(reviewed), VersionCell::Version(accounted)) => {
            violations.extend(reviewed_agent_violations(
                agent, reviewed, accounted, &findings,
            ));
        }
        _ => violations.push(format!(
            "{agent}: `{NOT_REVIEWED}` in one version column but not the other"
        )),
    }
    violations
}

fn unreviewed_agent_violations(agent: &str, findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .filter(|finding| finding.status != Status::NeedsEvaluation)
        .map(|finding| {
            format!(
                "{agent} is not reviewed yet; its finding at {} must be `needs evaluation`",
                finding.version
            )
        })
        .collect()
}

fn reviewed_agent_violations(
    agent: &str,
    reviewed: &str,
    accounted: &str,
    findings: &[Finding],
) -> Vec<String> {
    let mut violations = Vec::new();
    let accounted_against_reviewed = compare_versions(accounted, reviewed);
    if accounted_against_reviewed == Ordering::Greater {
        violations.push(format!(
            "{agent}: \"{ACCOUNTED_COLUMN}\" {accounted} is after \"{REVIEWED_COLUMN}\" {reviewed}"
        ));
    }
    let has_open_finding = findings.iter().any(|finding| finding.status.is_open());
    if accounted_against_reviewed == Ordering::Less && !has_open_finding {
        violations.push(format!(
            "{agent}: \"{ACCOUNTED_COLUMN}\" {accounted} trails \"{REVIEWED_COLUMN}\" {reviewed} with no open finding"
        ));
    }
    for finding in findings {
        let after_accounted = compare_versions(&finding.version, accounted) == Ordering::Greater;
        let after_reviewed = compare_versions(&finding.version, reviewed) == Ordering::Greater;
        if finding.status.is_open() && (!after_accounted || after_reviewed) {
            violations.push(format!(
                "{agent}: the open finding at {} must be after {accounted} and at or before {reviewed}",
                finding.version
            ));
        }
        if finding.status == Status::Deferred && after_reviewed {
            violations.push(format!(
                "{agent}: the deferred finding at {} is after \"{REVIEWED_COLUMN}\" {reviewed}",
                finding.version
            ));
        }
    }
    violations
}

/// Version-sort order: dot-separated parts compare as numbers where both are
/// numbers, a missing part counts as 0, and a prerelease (`0.146.1-dev.1`)
/// sorts before its release.
fn compare_versions(left: &str, right: &str) -> Ordering {
    let (left_release, left_prerelease) = split_prerelease(left);
    let (right_release, right_prerelease) = split_prerelease(right);
    compare_dotted(left_release, right_release).then_with(|| {
        match (left_prerelease, right_prerelease) {
            (None, None) => Ordering::Equal,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(left), Some(right)) => compare_dotted(left, right),
        }
    })
}

/// True for digits and dots with an optional `-` prerelease of letters,
/// digits and dots: `0.153.4`, `0.146.1-dev.1`.
fn is_bare_version(text: &str) -> bool {
    let (release, prerelease) = split_prerelease(text);
    is_dotted(release, |character| character.is_ascii_digit())
        && prerelease.is_none_or(|prerelease| {
            is_dotted(prerelease, |character| character.is_ascii_alphanumeric())
        })
}

fn is_dotted(text: &str, is_part_character: impl Fn(char) -> bool) -> bool {
    text.split('.')
        .all(|part| !part.is_empty() && part.chars().all(&is_part_character))
}

fn split_prerelease(version: &str) -> (&str, Option<&str>) {
    match version.split_once('-') {
        Some((release, prerelease)) => (release, Some(prerelease)),
        None => (version, None),
    }
}

fn compare_dotted(left: &str, right: &str) -> Ordering {
    let mut left_parts = left.split('.');
    let mut right_parts = right.split('.');
    loop {
        match (left_parts.next(), right_parts.next()) {
            (None, None) => return Ordering::Equal,
            (left_part, right_part) => {
                let order = compare_part(left_part.unwrap_or("0"), right_part.unwrap_or("0"));
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

fn compare_part(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

#[derive(Debug, PartialEq)]
enum VersionCell<'a> {
    NotReviewed,
    Version(&'a str),
}

impl<'a> VersionCell<'a> {
    fn parse(agent: &str, column: &str, cell: &'a str) -> Result<Self, String> {
        if cell == NOT_REVIEWED {
            Ok(Self::NotReviewed)
        } else if is_bare_version(cell) {
            Ok(Self::Version(cell))
        } else {
            Err(format!(
                "{agent}: \"{column}\" `{cell}` is not a bare version or `{NOT_REVIEWED}`"
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Status {
    NeedsEvaluation,
    PendingSupport,
    Deferred,
}

impl Status {
    fn parse(status: &str) -> Option<Self> {
        match status {
            "needs evaluation" => Some(Self::NeedsEvaluation),
            "pending support" => Some(Self::PendingSupport),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }

    /// Deferred findings do not hold "accounted for through" back: they record
    /// a decision not to support the change for now.
    fn is_open(self) -> bool {
        self != Self::Deferred
    }
}

#[derive(Debug)]
struct Finding {
    status: Status,
    version: String,
}

impl Finding {
    /// A bullet written `**<status>** (<version>): <what changed>`.
    fn parse(agent: &str, line: &FindingLine) -> Result<Self, String> {
        let malformed = || {
            format!(
                "{agent}: the finding `{}` is not written `**<status>** (<version>): <what changed>`",
                line.text.trim()
            )
        };
        let bold_lead = line.bold_lead.as_deref().ok_or_else(malformed)?;
        let version = line
            .text
            .trim_start()
            .strip_prefix('(')
            .and_then(|rest| rest.split_once("):"))
            .map(|(version, _)| version.trim())
            .filter(|version| is_bare_version(version))
            .ok_or_else(malformed)?;
        let status = Status::parse(bold_lead)
            .ok_or_else(|| format!("{agent}: unknown finding status `{bold_lead}`"))?;
        Ok(Self {
            status,
            version: version.to_owned(),
        })
    }
}

#[derive(Debug)]
struct Row {
    agent: String,
    reviewed: String,
    accounted: String,
}

/// One top-level bullet under a findings heading: its leading bold text, if
/// any, and the text after it, without its sub-bullets.
#[derive(Debug)]
struct FindingLine {
    bold_lead: Option<String>,
    text: String,
}

#[derive(Debug)]
struct FindingsSection {
    agent: String,
    lines: Vec<FindingLine>,
}

#[derive(Debug, Default)]
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

#[derive(Debug, Default)]
struct UpstreamReleases {
    table: Option<Table>,
    findings: Vec<FindingsSection>,
}

impl UpstreamReleases {
    /// The first table under `## Upstream releases`, and the bullets under
    /// each `### Upstream findings: <agent>` heading.
    fn read(markdown: &str) -> Self {
        let mut releases = Self::default();
        let mut section = String::new();
        let mut in_findings = false;
        for block in outline(markdown) {
            match block {
                Block::Heading { level, text } => {
                    if level == HeadingLevel::H2 {
                        section = text.clone();
                    }
                    in_findings = level == HeadingLevel::H3
                        && match text.strip_prefix(FINDINGS_HEADING_PREFIX) {
                            Some(agent) => {
                                releases.findings.push(FindingsSection {
                                    agent: agent.to_owned(),
                                    lines: Vec::new(),
                                });
                                true
                            }
                            None => false,
                        };
                }
                Block::Table(table) if section == RELEASES_HEADING && releases.table.is_none() => {
                    releases.table = Some(table);
                }
                Block::Item(line) if in_findings => {
                    if let Some(current) = releases.findings.last_mut() {
                        current.lines.push(line);
                    }
                }
                Block::Table(_) | Block::Item(_) => {}
            }
        }
        releases
    }

    fn rows(&self) -> Result<Vec<Row>, String> {
        let table = self
            .table
            .as_ref()
            .ok_or_else(|| format!("no table under `## {RELEASES_HEADING}`"))?;
        let column = |name: &str| {
            table
                .header
                .iter()
                .position(|heading| heading == name)
                .ok_or_else(|| format!("the `## {RELEASES_HEADING}` table has no `{name}` column"))
        };
        let (agent, reviewed, accounted) = (
            column(AGENT_COLUMN)?,
            column(REVIEWED_COLUMN)?,
            column(ACCOUNTED_COLUMN)?,
        );
        let cell = |row: &[String], index: usize| row.get(index).cloned().unwrap_or_default();
        Ok(table
            .rows
            .iter()
            .map(|row| Row {
                agent: cell(row, agent),
                reviewed: cell(row, reviewed),
                accounted: cell(row, accounted),
            })
            .collect())
    }

    fn findings_of(&self, agent: &str) -> Vec<&FindingLine> {
        self.findings
            .iter()
            .filter(|section| section.agent == agent)
            .flat_map(|section| &section.lines)
            .collect()
    }
}

enum Block {
    Heading { level: HeadingLevel, text: String },
    Table(Table),
    Item(FindingLine),
}

/// The headings, tables and top-level list items of `markdown`, in order, as
/// plain text. Links keep their Markdown, so a linked version is not read as
/// a bare one. A nested list item is detail under its parent and adds no text.
fn outline(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut table = Table::default();
    let mut row = Vec::new();
    let mut link_destinations = Vec::new();
    let mut item_bold_lead = None;
    let mut in_bold_lead = false;
    let mut item_depth = 0usize;
    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES) {
        let in_nested_item = item_depth > 1;
        match event {
            Event::Start(Tag::Item) => {
                item_depth += 1;
                if item_depth == 1 {
                    text.clear();
                    item_bold_lead = None;
                }
            }
            Event::End(TagEnd::Item) => {
                if item_depth == 1 {
                    blocks.push(Block::Item(FindingLine {
                        bold_lead: item_bold_lead.take(),
                        text: std::mem::take(&mut text),
                    }));
                }
                item_depth -= 1;
            }
            _ if in_nested_item => {}
            Event::Start(Tag::Heading { .. } | Tag::TableCell) => text.clear(),
            Event::Start(Tag::Strong)
                if item_depth == 1 && item_bold_lead.is_none() && text.trim().is_empty() =>
            {
                text.clear();
                in_bold_lead = true;
            }
            Event::End(TagEnd::Strong) if in_bold_lead => {
                item_bold_lead = Some(std::mem::take(&mut text));
                in_bold_lead = false;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                text.push('[');
                link_destinations.push(dest_url);
            }
            Event::End(TagEnd::Link) => {
                if let Some(destination) = link_destinations.pop() {
                    text.push_str("](");
                    text.push_str(&destination);
                    text.push(')');
                }
            }
            Event::Text(fragment) | Event::Code(fragment) => text.push_str(&fragment),
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            Event::End(TagEnd::Heading(level)) => blocks.push(Block::Heading {
                level,
                text: std::mem::take(&mut text).trim().to_owned(),
            }),
            Event::End(TagEnd::TableCell) => row.push(std::mem::take(&mut text).trim().to_owned()),
            Event::End(TagEnd::TableHead) => table.header = std::mem::take(&mut row),
            Event::End(TagEnd::TableRow) => table.rows.push(std::mem::take(&mut row)),
            Event::End(TagEnd::Table) => blocks.push(Block::Table(std::mem::take(&mut table))),
            _ => {}
        }
    }
    blocks
}

#[test]
fn providers_md_records_every_registered_agent_consistently() {
    let registered: Vec<&str> = providers()
        .iter()
        .map(|provider| provider.labels().display)
        .collect();

    let violations = violations(PROVIDERS_MD, &registered);

    assert!(
        violations.is_empty(),
        "PROVIDERS.md `## {RELEASES_HEADING}`:\n{}",
        violations.join("\n")
    );
}

const REGISTERED: &[&str] = &["Alpha", "Beta"];

const CONSISTENT: &str = "\
## Upstream releases

| Agent | Release source | Reviewed through | Accounted for through | Last review |
| --- | --- | --- | --- | --- |
| Alpha | [releases](https://example.com) | 1.2.0 | 1.0.0 | 2026-01-01 |
| Beta | [releases](https://example.com) | — | — | — |

## Alpha

### Upstream findings: Alpha

- **pending support** (1.1.0): a new record kind.
- **deferred** (0.9.0): an experimental format. Deferred: off by default.

## Beta

### Upstream findings: Beta

- **needs evaluation** (2.0.0): a major release.
";

fn violations_after(replacements: &[(&str, &str)]) -> Vec<String> {
    let markdown = replacements
        .iter()
        .fold(CONSISTENT.to_owned(), |markdown, (from, to)| {
            assert!(markdown.contains(from), "the sample holds `{from}`");
            markdown.replacen(from, to, 1)
        });
    violations(&markdown, REGISTERED)
}

#[test]
fn a_consistent_table_and_findings_have_no_violations() {
    assert_eq!(violations(CONSISTENT, REGISTERED), Vec::<String>::new());
}

#[test]
fn a_registered_agent_without_a_row_is_reported() {
    let violations = violations_after(&[(
        "| Beta | [releases](https://example.com) | — | — | — |\n",
        "",
    )]);

    assert_eq!(violations, ["Beta has no row"]);
}

#[test]
fn a_second_row_for_an_agent_is_reported() {
    let violations = violations_after(&[(
        "| Beta |",
        "| Alpha | [releases](https://example.com) | 1.2.0 | 1.0.0 | 2026-01-01 |\n| Beta |",
    )]);

    assert!(
        violations.contains(&"Alpha has 2 rows".to_owned()),
        "{violations:?}"
    );
}

#[test]
fn a_row_for_an_unregistered_agent_is_reported() {
    let violations = violations_after(&[("| Beta |", "| Gamma | x | — | — | — |\n| Beta |")]);

    assert_eq!(
        violations,
        ["the row for Gamma names no registered agent; expected one of: Alpha, Beta"]
    );
}

#[test]
fn accounted_for_after_reviewed_is_reported() {
    let violations = violations_after(&[("| 1.2.0 | 1.0.0 |", "| 1.2.0 | 1.3.0 |")]);

    assert!(
        violations.contains(
            &"Alpha: \"Accounted for through\" 1.3.0 is after \"Reviewed through\" 1.2.0"
                .to_owned()
        ),
        "{violations:?}"
    );
}

#[test]
fn accounted_for_trailing_reviewed_with_no_open_finding_is_reported() {
    let violations =
        violations_after(&[("- **pending support** (1.1.0): a new record kind.\n", "")]);

    assert_eq!(
        violations,
        [
            "Alpha: \"Accounted for through\" 1.0.0 trails \"Reviewed through\" 1.2.0 with no open finding"
        ]
    );
}

#[test]
fn an_unknown_finding_status_is_reported() {
    let violations = violations_after(&[("**pending support**", "**blocked**")]);

    assert!(
        violations.contains(&"Alpha: unknown finding status `blocked`".to_owned()),
        "{violations:?}"
    );
}

#[test]
fn a_finding_not_written_in_the_bullet_format_is_reported() {
    let violations =
        violations_after(&[("**pending support** (1.1.0):", "pending support in 1.1.0:")]);

    assert!(
        violations
            .iter()
            .any(|violation| violation.starts_with("Alpha: the finding `pending support in 1.1.0")),
        "{violations:?}"
    );
}

#[test]
fn an_open_finding_at_or_before_accounted_for_is_reported() {
    let violations = violations_after(&[("(1.1.0)", "(1.0.0)")]);

    assert_eq!(
        violations,
        ["Alpha: the open finding at 1.0.0 must be after 1.0.0 and at or before 1.2.0"]
    );
}

#[test]
fn an_open_finding_after_reviewed_is_reported() {
    let violations = violations_after(&[("(1.1.0)", "(1.3.0)")]);

    assert_eq!(
        violations,
        ["Alpha: the open finding at 1.3.0 must be after 1.0.0 and at or before 1.2.0"]
    );
}

#[test]
fn a_deferred_finding_after_reviewed_is_reported() {
    let violations = violations_after(&[("(0.9.0)", "(1.3.0)")]);

    assert_eq!(
        violations,
        ["Alpha: the deferred finding at 1.3.0 is after \"Reviewed through\" 1.2.0"]
    );
}

#[test]
fn a_dash_in_one_version_column_only_is_reported() {
    let violations = violations_after(&[("| — | — | — |", "| 2.0.0 | — | — |")]);

    assert_eq!(
        violations,
        ["Beta: `—` in one version column but not the other"]
    );
}

#[test]
fn an_unreviewed_agent_with_a_finding_other_than_needs_evaluation_is_reported() {
    let violations = violations_after(&[("**needs evaluation**", "**pending support**")]);

    assert_eq!(
        violations,
        ["Beta is not reviewed yet; its finding at 2.0.0 must be `needs evaluation`"]
    );
}

#[test]
fn a_version_cell_with_a_prefix_is_reported() {
    let violations = violations_after(&[("| 1.2.0 | 1.0.0 |", "| v1.2.0 | 1.0.0 |")]);

    assert_eq!(
        violations,
        ["Alpha: \"Reviewed through\" `v1.2.0` is not a bare version or `—`"]
    );
}

#[test]
fn a_registered_agent_without_a_findings_subsection_is_reported() {
    let violations = violations_after(&[(
        "### Upstream findings: Beta\n\n- **needs evaluation** (2.0.0): a major release.\n",
        "",
    )]);

    assert_eq!(
        violations,
        ["Beta has no `### Upstream findings: Beta` subsection"]
    );
}

#[test]
fn a_findings_subsection_for_an_unregistered_agent_is_reported() {
    let violations =
        violations_after(&[("## Beta\n", "### Upstream findings: Gamma\n\n## Beta\n")]);

    assert_eq!(
        violations,
        ["`### Upstream findings: Gamma` names no registered agent; expected one of: Alpha, Beta"]
    );
}

#[test]
fn a_second_findings_subsection_for_an_agent_is_reported() {
    let violations = violations_after(&[("## Beta\n", "### Upstream findings: Beta\n\n## Beta\n")]);

    assert_eq!(violations, ["Beta has 2 findings subsections"]);
}

#[test]
fn a_version_cell_that_is_not_a_bare_version_is_reported() {
    for cell in [
        "1.2.0 (beta)",
        "0.150–0.153",
        "1.0.0rc1",
        "[1.2.0](https://example.com)",
    ] {
        let violations = violations_after(&[("| 1.2.0 | 1.0.0 |", &format!("| {cell} | 1.0.0 |"))]);

        assert_eq!(
            violations,
            [format!(
                "Alpha: \"Reviewed through\" `{cell}` is not a bare version or `—`"
            )],
            "{cell}"
        );
    }
}

#[test]
fn a_finding_version_that_is_not_a_bare_version_is_reported() {
    for version in [
        "1.1.0 (beta)",
        "1.0–1.1",
        "1.1.0rc1",
        "[1.1.0](https://example.com)",
    ] {
        let violations = violations_after(&[("(1.1.0)", &format!("({version})"))]);

        assert!(
            violations
                .iter()
                .any(|violation| violation.starts_with(&format!("Alpha: the finding `({version})"))),
            "{version}: {violations:?}"
        );
    }
}

#[test]
fn a_finding_with_sub_bullets_keeps_its_status_and_version() {
    let violations = violations_after(&[(
        "- **pending support** (1.1.0): a new record kind.\n",
        "- **pending support** (1.1.0): a new record kind.\n  - First written by 1.1.0.\n  - **Detail** in bold.\n",
    )]);

    assert_eq!(violations, Vec::<String>::new());
}

#[test]
fn a_nested_bullet_shaped_like_a_finding_doesnt_count_as_one() {
    let violations = violations_after(&[
        ("- **pending support** (1.1.0): a new record kind.\n", ""),
        (
            "Deferred: off by default.\n",
            "Deferred: off by default.\n  - **pending support** (1.1.0): a new record kind.\n",
        ),
    ]);

    assert_eq!(
        violations,
        [
            "Alpha: \"Accounted for through\" 1.0.0 trails \"Reviewed through\" 1.2.0 with no open finding"
        ]
    );
}

#[test]
fn version_parts_compare_as_numbers() {
    assert_eq!(compare_versions("0.153.4", "0.150.0"), Ordering::Greater);
    assert_eq!(compare_versions("1.10.0", "1.9.2"), Ordering::Greater);
    assert_eq!(compare_versions("2.1.272", "2.1.296"), Ordering::Less);
    assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
}

#[test]
fn a_prerelease_sorts_before_its_release() {
    assert_eq!(compare_versions("0.146.1-dev.1", "0.146.1"), Ordering::Less);
    assert_eq!(
        compare_versions("0.146.1-dev.1", "0.146.0"),
        Ordering::Greater
    );
    assert_eq!(
        compare_versions("0.163.0-alpha.5", "0.163.0-alpha.10"),
        Ordering::Less
    );
}
