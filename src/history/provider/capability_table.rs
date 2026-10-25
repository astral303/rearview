//! The capability table in PROVIDERS.md, generated from the `SessionProvider`
//! methods returning `Capability`: each one's "Capable when" doc sentence and
//! the registered providers that declare it Not Capable.

use super::{SessionProvider, providers};
use regex::Regex;
use std::ops::Range;
use std::path::Path;
use unicode_width::UnicodeWidthStr;

/// The file that declares `SessionProvider`.
const TRAIT_SOURCE: &str = include_str!("mod.rs");
const BEGIN_MARKER: &str = "<!-- generated capability table: begin -->";
const END_MARKER: &str = "<!-- generated capability table: end -->";
/// `1` makes the table test rewrite the table instead of failing.
const UPDATE_VARIABLE: &str = "REARVIEW_UPDATE_DOCS";
const UPDATE_TASK: &str = "mise run update-capability-table";

/// One `SessionProvider` method returning `Capability`.
#[derive(Debug, PartialEq)]
struct CapabilityMethod {
    name: String,
    /// The doc comment's first sentence after "Capable when", as Markdown.
    capable_when: String,
}

struct CapabilityRow {
    name: String,
    capable_when: String,
    /// Display names of the providers that declare the capability Not
    /// Capable, in registration order.
    not_capable: Vec<&'static str>,
}

/// The methods of the `SessionProvider` trait in `source` that return
/// `Capability`, with any type argument, in source order.
fn capability_methods(source: &str) -> Vec<CapabilityMethod> {
    let file = syn::parse_file(source).expect("parse the file declaring SessionProvider");
    let provider_trait = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Trait(item) if item.ident == "SessionProvider" => Some(item),
            _ => None,
        })
        .expect("the source declares the SessionProvider trait");
    provider_trait
        .items
        .iter()
        .filter_map(|item| match item {
            syn::TraitItem::Fn(method) if returns_capability(&method.sig.output) => {
                let name = method.sig.ident.to_string();
                let capable_when = capable_when(&name, &method.attrs);
                Some(CapabilityMethod { name, capable_when })
            }
            _ => None,
        })
        .collect()
}

fn returns_capability(output: &syn::ReturnType) -> bool {
    let syn::ReturnType::Type(_, return_type) = output else {
        return false;
    };
    let syn::Type::Path(path) = &**return_type else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Capability")
}

/// The first sentence of the doc comment in `attributes`, after "Capable
/// when" and without its period. An intra-doc link becomes its text.
fn capable_when(method: &str, attributes: &[syn::Attribute]) -> String {
    let doc = attributes
        .iter()
        .filter_map(doc_line)
        .collect::<Vec<_>>()
        .join(" ");
    let doc = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    let first_sentence = doc
        .split_once(". ")
        .map_or(doc.trim_end_matches('.'), |(first, _)| first);
    let sentence = first_sentence
        .strip_prefix("Capable when ")
        .unwrap_or_else(|| {
            panic!(
                "the doc comment of `SessionProvider::{method}` does not start with \
                 \"Capable when\": the capability table shows that sentence"
            )
        });
    let intra_doc_link = Regex::new(r"\[(`[^`]+`)\](\([^)]*\))?").unwrap();
    let sentence = intra_doc_link.replace_all(sentence, "$1").into_owned();
    assert!(
        !sentence.contains('|'),
        "the \"Capable when\" sentence of `SessionProvider::{method}` holds a `|`: the \
         capability table would end its cell there"
    );
    sentence
}

/// The text of a `///` line.
fn doc_line(attribute: &syn::Attribute) -> Option<String> {
    if !attribute.path().is_ident("doc") {
        return None;
    }
    match &attribute.meta {
        syn::Meta::NameValue(syn::MetaNameValue {
            value:
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(text),
                    ..
                }),
            ..
        }) => Some(text.value()),
        _ => None,
    }
}

/// A removed capability method fails to compile here, and a new one panics
/// until it has an arm.
fn declares_capable(provider: &dyn SessionProvider, capability: &str) -> bool {
    match capability {
        "resume" => provider.resume().is_capable(),
        "fork" => provider.fork().is_capable(),
        "rename" => provider.rename().is_capable(),
        "delete" => provider.delete().is_capable(),
        "subagents" => provider.subagents().is_capable(),
        "subagent_by_id" => provider.subagent_by_id().is_capable(),
        "session_id_lookup" => provider.session_id_lookup().is_capable(),
        "workspace_sessions_dir" => provider.workspace_sessions_dir().is_capable(),
        "current_session_variable" => provider.current_session_variable().is_capable(),
        unknown => panic!(
            "`SessionProvider::{unknown}` returns `Capability`: add it to the match in \
             `declares_capable`"
        ),
    }
}

fn rows(
    capability_methods: &[CapabilityMethod],
    providers: &[&dyn SessionProvider],
) -> Vec<CapabilityRow> {
    capability_methods
        .iter()
        .map(|method| CapabilityRow {
            name: method.name.clone(),
            capable_when: method.capable_when.clone(),
            not_capable: providers
                .iter()
                .filter(|provider| !declares_capable(**provider, &method.name))
                .map(|provider| provider.labels().display)
                .collect(),
        })
        .collect()
}

/// The Markdown table of `rows`, each column padded to its widest cell.
fn render(rows: &[CapabilityRow]) -> String {
    let header = [
        "Capability".to_owned(),
        "Capable when".to_owned(),
        "Not Capable".to_owned(),
    ];
    let body = rows.iter().map(|row| {
        let not_capable = if row.not_capable.is_empty() {
            "—".to_owned()
        } else {
            row.not_capable.join(", ")
        };
        [
            format!("`{}`", row.name),
            row.capable_when.clone(),
            not_capable,
        ]
    });
    let cells: Vec<[String; 3]> = std::iter::once(header).chain(body).collect();
    let widths: Vec<usize> = (0..3)
        .map(|column| {
            cells
                .iter()
                .map(|row| row[column].width())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |row: &[String; 3]| {
        let padded: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell}{}", " ".repeat(width - cell.width())))
            .collect();
        format!("| {} |", padded.join(" | "))
    };
    let separator = widths
        .iter()
        .map(|width| "-".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("|");
    let mut lines = vec![line(&cells[0]), format!("|{separator}|")];
    lines.extend(cells[1..].iter().map(line));
    lines.join("\n")
}

/// `table` between the markers, as PROVIDERS.md holds it.
fn generated_block(table: &str) -> String {
    format!("{BEGIN_MARKER}\n\n{table}\n\n{END_MARKER}")
}

/// `markdown` with its generated block, markers included, replaced by
/// `block`.
fn with_generated_block(markdown: &str, block: &str) -> String {
    let mut updated = markdown.to_owned();
    updated.replace_range(generated_block_range(markdown), block);
    updated
}

/// The byte range of the generated block in `markdown`, markers included.
fn generated_block_range(markdown: &str) -> Range<usize> {
    let start = markdown
        .find(BEGIN_MARKER)
        .unwrap_or_else(|| panic!("PROVIDERS.md has no line `{BEGIN_MARKER}`"));
    let end = markdown[start..]
        .find(END_MARKER)
        .map(|offset| start + offset + END_MARKER.len())
        .unwrap_or_else(|| {
            panic!("PROVIDERS.md has no line `{END_MARKER}` after `{BEGIN_MARKER}`")
        });
    start..end
}

#[allow(
    clippy::disallowed_methods,
    reason = "the variable switches this test to rewriting; no provider reads it"
)]
fn is_update_requested() -> bool {
    std::env::var_os(UPDATE_VARIABLE).is_some_and(|value| value == "1")
}

#[test]
fn providers_md_capability_table_is_up_to_date() {
    let expected = generated_block(&render(&rows(
        &capability_methods(TRAIT_SOURCE),
        providers(),
    )));
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("PROVIDERS.md");
    let markdown = std::fs::read_to_string(&path).unwrap();
    let updated = with_generated_block(&markdown, &expected);
    if updated == markdown {
        return;
    }
    if is_update_requested() {
        std::fs::write(&path, updated).unwrap();
        return;
    }
    panic!(
        "the capability table in PROVIDERS.md is out of date. Expected:\n\n{expected}\n\n\
         Rewrite it with `{UPDATE_TASK}`, or rerun this test with {UPDATE_VARIABLE}=1 set."
    );
}

#[test]
fn capability_methods_are_session_provider_methods_returning_capability() {
    let source = r#"
        trait Unrelated {
            /// Capable when it is not in `SessionProvider`.
            fn elsewhere(&self) -> Capability;
        }

        pub trait SessionProvider {
            /// Capable when a marker answers. Later sentences are left out.
            fn marker(&self) -> Capability;

            /// Capable when [`launcher`](Self::launcher) builds the
            /// command, and [`Capability`] names it.
            #[cfg_attr(not(test), expect(dead_code))]
            fn operation(&self) -> super::Capability<&dyn Operation>;

            /// Capable when `sessions[0]` and `[a](b)` keep their brackets.
            fn code_spans(&self) -> Capability;

            /// Not a capability.
            fn labels(&self) -> SourceLabels;
        }
    "#;

    assert_eq!(
        capability_methods(source),
        vec![
            CapabilityMethod {
                name: "marker".to_owned(),
                capable_when: "a marker answers".to_owned(),
            },
            CapabilityMethod {
                name: "operation".to_owned(),
                capable_when: "`launcher` builds the command, and `Capability` names it".to_owned(),
            },
            CapabilityMethod {
                name: "code_spans".to_owned(),
                capable_when: "`sessions[0]` and `[a](b)` keep their brackets".to_owned(),
            },
        ]
    );
}

#[test]
#[should_panic(expected = "does not start with \"Capable when\"")]
fn capability_doc_not_starting_with_capable_when_fails() {
    capability_methods(
        r#"
        pub trait SessionProvider {
            /// Sub-agent transcripts.
            fn subagents(&self) -> Capability;
        }
        "#,
    );
}

#[test]
#[should_panic(expected = "holds a `|`")]
fn capable_when_sentence_holding_pipe_fails() {
    capability_methods(
        r#"
        pub trait SessionProvider {
            /// Capable when `a | b` resolves.
            fn piped(&self) -> Capability;
        }
        "#,
    );
}

#[test]
fn table_lists_each_capability_with_its_capable_when_sentence_and_providers_not_capable() {
    let rows = [
        CapabilityRow {
            name: "short".to_owned(),
            capable_when: "a sentence wider than its header".to_owned(),
            not_capable: vec!["Alpha"],
        },
        CapabilityRow {
            name: "a_longer_capability_name".to_owned(),
            capable_when: "brief".to_owned(),
            not_capable: vec!["Alpha", "Beta", "Gamma"],
        },
    ];

    assert_eq!(
        render(&rows),
        [
            "| Capability                 | Capable when                     | Not Capable        |",
            "|----------------------------|----------------------------------|--------------------|",
            "| `short`                    | a sentence wider than its header | Alpha              |",
            "| `a_longer_capability_name` | brief                            | Alpha, Beta, Gamma |",
        ]
        .join("\n")
    );
}

#[test]
fn capability_every_provider_declares_capable_shows_dash() {
    let rows = [CapabilityRow {
        name: "resume".to_owned(),
        capable_when: "a command resumes the session".to_owned(),
        not_capable: Vec::new(),
    }];

    assert_eq!(
        render(&rows),
        [
            "| Capability | Capable when                  | Not Capable |",
            "|------------|-------------------------------|-------------|",
            "| `resume`   | a command resumes the session | —           |",
        ]
        .join("\n")
    );
}

#[test]
fn generated_block_is_replaced_and_text_around_it_kept() {
    let markdown =
        format!("# Title\n\nBefore.\n\n{BEGIN_MARKER}\n\nold table\n\n{END_MARKER}\n\nAfter.\n");

    assert_eq!(
        with_generated_block(&markdown, "NEW BLOCK"),
        "# Title\n\nBefore.\n\nNEW BLOCK\n\nAfter.\n"
    );
}

#[test]
#[should_panic(expected = "has no line `<!-- generated capability table: end -->`")]
fn generated_block_without_its_end_marker_fails() {
    with_generated_block(
        &format!("Before.\n\n{BEGIN_MARKER}\n\nold table\n"),
        "NEW BLOCK",
    );
}
