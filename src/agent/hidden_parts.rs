//! The parts of messages that `agent read` hides, by the flag that shows
//! each kind: `--tools`, `--tool-results`, `--thinking` and `--subagents`.

use crate::agent::protocol::escape_atom;
use crate::agent::transcript::AgentMessagePart;

#[derive(Clone, Debug, Default)]
pub struct HiddenParts<'a> {
    /// The name of each hidden tool call, in call order.
    tool_names: Vec<&'a str>,
    tool_results: usize,
    thinking: usize,
    /// Hidden sub-agent messages. Their parts are not counted by kind.
    subagent_messages: usize,
}

impl<'a> HiddenParts<'a> {
    /// Count `part`, a part its kind's flag hides.
    pub fn add_part(&mut self, part: &'a AgentMessagePart) {
        match part {
            AgentMessagePart::ToolUse { name, .. } => self.tool_names.push(name),
            AgentMessagePart::ToolResult { .. } => self.tool_results += 1,
            AgentMessagePart::Thinking { .. } => self.thinking += 1,
            // Text shows whatever the flags.
            AgentMessagePart::Text { .. } => {}
        }
    }

    pub fn add_subagent_message(&mut self) {
        self.subagent_messages += 1;
    }

    pub fn add(&mut self, other: &Self) {
        self.tool_names.extend_from_slice(&other.tool_names);
        self.tool_results += other.tool_results;
        self.thinking += other.thinking;
        self.subagent_messages += other.subagent_messages;
    }

    pub fn is_empty(&self) -> bool {
        self.counts().iter().all(|(_, count)| *count == 0)
    }

    /// ` tools=3 tool-results=12`: each hidden kind and its count, for a
    /// record of messages hidden whole.
    pub fn count_atoms(&self) -> String {
        counted_atoms("", &self.counts())
    }

    /// ` hidden-tools=37 hidden-thinking=4`: [`Self::count_atoms`] for a
    /// read's header.
    pub fn total_atoms(&self) -> String {
        counted_atoms("hidden-", &self.counts())
    }

    /// ` hidden-tools=Write,PowerShell hidden-thinking=1`: a shown message's
    /// hidden calls by name, and its hidden results and thinking by count.
    pub fn message_atoms(&self) -> String {
        let mut atoms = String::new();
        if !self.tool_names.is_empty() {
            let names = self
                .tool_names
                .iter()
                .map(|name| escape_atom(name))
                .collect::<Vec<_>>();
            atoms.push_str(&format!(" hidden-tools={}", names.join(",")));
        }
        atoms.push_str(&counted_atoms(
            "hidden-",
            &[
                ("tool-results", self.tool_results),
                ("thinking", self.thinking),
            ],
        ));
        atoms
    }

    /// Each kind and its count, in the order of the `policy=` atom.
    fn counts(&self) -> [(&'static str, usize); 4] {
        [
            ("tools", self.tool_names.len()),
            ("tool-results", self.tool_results),
            ("thinking", self.thinking),
            ("subagents", self.subagent_messages),
        ]
    }
}

/// ` {prefix}{kind}={count}` for each kind with a count.
fn counted_atoms(prefix: &str, counts: &[(&str, usize)]) -> String {
    counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(kind, count)| format!(" {prefix}{kind}={count}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::source;
    use crate::agent::transcript::AgentMessageRole::{Assistant, User};
    use crate::log_entry::Tool;
    use serde_json::json;

    fn call(name: &str) -> AgentMessagePart {
        AgentMessagePart::ToolUse {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            tool: Tool::Other,
            input: json!({}),
            source: source(Assistant),
        }
    }

    fn result() -> AgentMessagePart {
        AgentMessagePart::ToolResult {
            tool_use_id: "toolu_1".to_owned(),
            content: Some(json!("ok")),
            source: source(User),
        }
    }

    fn thinking() -> AgentMessagePart {
        AgentMessagePart::Thinking {
            thinking: "weigh it".to_owned(),
            source: source(Assistant),
        }
    }

    fn hidden<'a>(parts: &'a [AgentMessagePart]) -> HiddenParts<'a> {
        let mut hidden = HiddenParts::default();
        for part in parts {
            hidden.add_part(part);
        }
        hidden
    }

    #[test]
    fn empty_hidden_parts_dont_print_an_atom() {
        let hidden = HiddenParts::default();

        assert!(hidden.is_empty());
        assert_eq!(hidden.message_atoms(), "");
        assert_eq!(hidden.total_atoms(), "");
        assert_eq!(hidden.count_atoms(), "");
    }

    #[test]
    fn a_message_names_its_hidden_calls_in_order_and_counts_its_other_hidden_parts() {
        let parts = [call("Write"), thinking(), call("Power Shell"), result()];

        assert_eq!(
            hidden(&parts).message_atoms(),
            " hidden-tools=Write,Power%20Shell hidden-tool-results=1 hidden-thinking=1"
        );
    }

    #[test]
    fn totals_and_runs_count_each_kind_and_leave_out_kinds_without_any() {
        let parts = [call("Read"), call("Read"), result()];
        let mut total = hidden(&parts);
        let mut subagent = HiddenParts::default();
        subagent.add_subagent_message();

        total.add(&subagent);

        assert_eq!(
            total.total_atoms(),
            " hidden-tools=2 hidden-tool-results=1 hidden-subagents=1"
        );
        assert_eq!(total.count_atoms(), " tools=2 tool-results=1 subagents=1");
    }
}
