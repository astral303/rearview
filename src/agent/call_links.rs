//! The links between a transcript's tool calls and the messages holding their
//! results: `agent read` prints `result=` on a call and `call=` on a result.
//! A call and a result pair by id within one thread: the session's own
//! messages, or one sub-agent's.

use crate::agent::transcript::{AgentMessage, AgentMessagePart, AgentTranscript, SubagentOrigin};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

/// The id readers give a call or a result recorded without an id. Link
/// nothing by it: unrelated calls share it.
const UNKNOWN_ID: &str = "unknown";

/// A tool call's address: the `number`th call of message `mN`, counting every
/// call of the message in order from 1.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CallAddress {
    pub ordinal: usize,
    pub number: usize,
}

impl fmt::Display for CallAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "m{}.{}", self.ordinal, self.number)
    }
}

/// A call that a result in another message answers: the addresses of the
/// parts a reader split it into (`X`, `X#2`, …), or its own alone, and its
/// tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnsweredCall<'a> {
    pub parts: Vec<CallAddress>,
    pub tool: &'a str,
}

/// The thread of a message: the label and transcript identifying its
/// sub-agent thread ([`SubagentOrigin::key`]), as two threads can share a
/// label; `None` for the session's own messages.
type Thread<'a> = Option<(&'a str, Option<&'a Path>)>;

fn thread_of(message: &AgentMessage) -> Thread<'_> {
    message.subagent.as_deref().map(SubagentOrigin::key)
}

/// An id and the thread of the message holding it.
type IdInThread<'a> = (Thread<'a>, &'a str);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Call<'a> {
    pub address: CallAddress,
    pub tool: &'a str,
}

#[derive(Debug, Default)]
pub struct CallLinks<'a> {
    calls: HashMap<IdInThread<'a>, Vec<Call<'a>>>,
    /// The id and address of each part after the first of a call a reader
    /// split, by the id of the first: `X#2`, `X#3`, … under `X`.
    later_parts: HashMap<IdInThread<'a>, Vec<(&'a str, CallAddress)>>,
    /// The ordinal of each message holding a result for the id, in
    /// transcript order.
    results: HashMap<IdInThread<'a>, Vec<usize>>,
}

impl<'a> CallLinks<'a> {
    pub fn of(transcript: &'a AgentTranscript) -> Self {
        let mut links = Self::default();
        for message in &transcript.messages {
            let thread = thread_of(message);
            for (number, part) in numbered_calls(message) {
                if let AgentMessagePart::ToolUse { id, name, .. } = part {
                    let address = CallAddress {
                        ordinal: message.ordinal,
                        number,
                    };
                    links
                        .calls
                        .entry((thread, id.as_str()))
                        .or_default()
                        .push(Call {
                            address,
                            tool: name,
                        });
                    let first_part = unsplit_call_id(id);
                    if first_part != id {
                        links
                            .later_parts
                            .entry((thread, first_part))
                            .or_default()
                            .push((id, address));
                    }
                }
            }
            for part in &message.parts {
                if let AgentMessagePart::ToolResult { tool_use_id, .. } = part {
                    let ordinals = links
                        .results
                        .entry((thread, tool_use_id.as_str()))
                        .or_default();
                    if ordinals.last() != Some(&message.ordinal) {
                        ordinals.push(message.ordinal);
                    }
                }
            }
        }
        links
    }

    /// The messages other than `message` holding the result of its call
    /// `call_id`. Each part of a call a reader split (`X`, `X#2`, …) has the
    /// result of `X`. Empty when the id is not one call's alone.
    pub fn results_of(&self, message: &AgentMessage, call_id: &str) -> Vec<usize> {
        let thread = thread_of(message);
        let result_id = unsplit_call_id(call_id);
        if result_id == UNKNOWN_ID
            || self.call_with(thread, call_id).is_none()
            || (result_id != call_id && self.calls_with(thread, result_id) > 1)
        {
            return Vec::new();
        }
        self.results
            .get(&(thread, result_id))
            .into_iter()
            .flatten()
            .copied()
            .filter(|&ordinal| ordinal != message.ordinal)
            .collect()
    }

    /// The call that `message`'s result for `tool_use_id` answers, with every
    /// part a reader split it into, when that call is in another message. A
    /// part whose id is on two calls is left out, as [`Self::results_of`]
    /// gives it no result.
    pub fn call_of(&self, message: &AgentMessage, tool_use_id: &str) -> Option<AnsweredCall<'a>> {
        let thread = thread_of(message);
        let call = self
            .call_with(thread, tool_use_id)
            .filter(|call| call.address.ordinal != message.ordinal)?;
        let later_parts = self
            .later_parts
            .get(&(thread, tool_use_id))
            .into_iter()
            .flatten()
            .filter(|&&(id, part)| {
                part.ordinal == call.address.ordinal && self.call_with(thread, id).is_some()
            })
            .map(|&(_, part)| part);
        Some(AnsweredCall {
            parts: std::iter::once(call.address).chain(later_parts).collect(),
            tool: call.tool,
        })
    }

    /// The call that `message`'s result for `tool_use_id` answers, when that
    /// call is in `message` itself.
    pub fn call_in_own_message(
        &self,
        message: &AgentMessage,
        tool_use_id: &str,
    ) -> Option<Call<'a>> {
        self.call_with(thread_of(message), tool_use_id)
            .filter(|call| call.address.ordinal == message.ordinal)
    }

    /// The call with `id` in `thread`, when exactly one call has it and it is
    /// not [`UNKNOWN_ID`].
    fn call_with(&self, thread: Thread<'_>, id: &str) -> Option<Call<'a>> {
        match self.calls.get(&(thread, id))?.as_slice() {
            [call] if id != UNKNOWN_ID => Some(*call),
            _ => None,
        }
    }

    fn calls_with(&self, thread: Thread<'_>, id: &str) -> usize {
        self.calls.get(&(thread, id)).map_or(0, Vec::len)
    }
}

/// Each tool call of `message` with its number, counting from 1 in order.
pub fn numbered_calls(message: &AgentMessage) -> impl Iterator<Item = (usize, &AgentMessagePart)> {
    message
        .parts
        .iter()
        .filter(|part| matches!(part, AgentMessagePart::ToolUse { .. }))
        .enumerate()
        .map(|(index, part)| (index + 1, part))
}

/// The id a split call's result carries: `X` for each part `X`, `X#2`, ….
fn unsplit_call_id(id: &str) -> &str {
    id.rsplit_once('#')
        .filter(|(_, number)| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
        .map_or(id, |(base, _)| base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{source, text_message, transcript};
    use crate::agent::transcript::AgentMessageRole::{Assistant, User};
    use crate::log_entry::{SubagentIdentity, Tool};
    use serde_json::json;
    use std::sync::Arc;

    fn call(id: &str, name: &str) -> AgentMessagePart {
        AgentMessagePart::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            tool: Tool::Other,
            input: json!({}),
            source: source(Assistant),
        }
    }

    fn result(id: &str) -> AgentMessagePart {
        AgentMessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: Some(json!("ok")),
            source: source(User),
        }
    }

    fn message(ordinal: usize, parts: Vec<AgentMessagePart>) -> AgentMessage {
        AgentMessage {
            parts,
            ..text_message(ordinal, Assistant, "")
        }
    }

    fn address(ordinal: usize, number: usize) -> CallAddress {
        CallAddress { ordinal, number }
    }

    #[test]
    fn a_call_and_its_result_in_other_messages_link_both_ways() {
        let transcript = transcript(
            vec![
                message(1, vec![call("a", "Read"), call("b", "Bash")]),
                message(2, vec![result("a")]),
                message(3, vec![result("b")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);
        let [calls, first, second] = &transcript.messages[..] else {
            unreachable!()
        };

        assert_eq!(links.results_of(calls, "a"), [2]);
        assert_eq!(links.results_of(calls, "b"), [3]);
        assert_eq!(
            links.call_of(second, "b"),
            Some(AnsweredCall {
                parts: vec![address(1, 2)],
                tool: "Bash"
            })
        );
        assert_eq!(links.call_of(first, "a").unwrap().parts, [address(1, 1)]);
    }

    #[test]
    fn a_hook_result_adds_a_second_result_message() {
        let transcript = transcript(
            vec![
                message(1, vec![call("a", "Bash")]),
                message(2, vec![result("a")]),
                message(3, vec![result("a")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);

        assert_eq!(links.results_of(&transcript.messages[0], "a"), [2, 3]);
    }

    #[test]
    fn a_result_in_the_calls_own_message_doesnt_link_and_names_its_call_in_the_message() {
        let transcript = transcript(
            vec![message(1, vec![call("a", "shell"), result("a")])],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);
        let shell = &transcript.messages[0];

        assert!(links.results_of(shell, "a").is_empty());
        assert_eq!(links.call_of(shell, "a"), None);
        assert_eq!(
            links.call_in_own_message(shell, "a"),
            Some(Call {
                address: address(1, 1),
                tool: "shell"
            })
        );
    }

    #[test]
    fn a_result_of_a_call_in_another_message_doesnt_name_a_call_in_its_own() {
        let transcript = transcript(
            vec![
                message(1, vec![call("a", "Bash")]),
                message(2, vec![result("a")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);

        assert_eq!(
            links.call_in_own_message(&transcript.messages[1], "a"),
            None
        );
    }

    #[test]
    fn an_unknown_id_doesnt_link() {
        let transcript = transcript(
            vec![
                message(1, vec![call("unknown", "exec")]),
                message(2, vec![result("unknown")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);

        assert!(
            links
                .results_of(&transcript.messages[0], "unknown")
                .is_empty()
        );
        assert_eq!(links.call_of(&transcript.messages[1], "unknown"), None);
    }

    #[test]
    fn an_id_on_two_calls_doesnt_link() {
        let transcript = transcript(
            vec![
                message(1, vec![call("twice", "Read"), call("twice", "Read")]),
                message(2, vec![result("twice")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);

        assert!(
            links
                .results_of(&transcript.messages[0], "twice")
                .is_empty()
        );
        assert_eq!(links.call_of(&transcript.messages[1], "twice"), None);
    }

    #[test]
    fn a_split_unknown_call_doesnt_link() {
        let transcript = transcript(
            vec![
                message(1, vec![call("unknown", "exec"), call("unknown#2", "exec")]),
                message(2, vec![result("unknown")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);
        let calls = &transcript.messages[0];

        assert!(links.results_of(calls, "unknown").is_empty());
        assert!(links.results_of(calls, "unknown#2").is_empty());
        assert_eq!(links.call_of(&transcript.messages[1], "unknown"), None);
    }

    #[test]
    fn each_part_of_a_split_call_has_the_result_that_answers_every_part() {
        let transcript = transcript(
            vec![
                message(1, vec![call("x", "exec"), call("x#2", "exec")]),
                message(2, vec![result("x")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);
        let calls = &transcript.messages[0];

        assert_eq!(links.results_of(calls, "x"), [2]);
        assert_eq!(links.results_of(calls, "x#2"), [2]);
        assert_eq!(
            links.call_of(&transcript.messages[1], "x"),
            Some(AnsweredCall {
                parts: vec![address(1, 1), address(1, 2)],
                tool: "exec"
            })
        );
    }

    #[test]
    fn a_split_part_whose_id_is_on_two_calls_doesnt_link_either_way() {
        let transcript = transcript(
            vec![
                message(
                    1,
                    vec![call("x", "exec"), call("x#2", "exec"), call("x#2", "exec")],
                ),
                message(2, vec![result("x")]),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);
        let calls = &transcript.messages[0];

        assert_eq!(links.results_of(calls, "x"), [2]);
        assert!(links.results_of(calls, "x#2").is_empty());
        assert_eq!(
            links.call_of(&transcript.messages[1], "x").unwrap().parts,
            [address(1, 1)]
        );
    }

    fn in_thread(mut message: AgentMessage, label: &str, transcript: &str) -> AgentMessage {
        message.parent_tool_use_id = Some(label.to_owned());
        message.subagent = Some(Arc::new(SubagentOrigin {
            label: label.to_owned(),
            transcript: Some(Arc::from(Path::new(transcript))),
            launch_call: None,
            identity: SubagentIdentity::default(),
        }));
        message
    }

    /// Two Claude threads of one agent type share the label `agent-1` and
    /// the call id `a`; a third holds a result for `a` without its call.
    #[test]
    fn a_sub_agents_call_doesnt_link_to_a_result_in_another_thread() {
        let transcript = transcript(
            vec![
                in_thread(message(1, vec![call("a", "Read")]), "agent-1", "one.jsonl"),
                message(2, vec![result("a")]),
                in_thread(message(3, vec![result("a")]), "agent-1", "one.jsonl"),
                in_thread(message(4, vec![call("a", "Grep")]), "agent-1", "two.jsonl"),
                in_thread(message(5, vec![result("a")]), "agent-1", "two.jsonl"),
                in_thread(message(6, vec![result("a")]), "agent-1", "three.jsonl"),
            ],
            "session.jsonl",
        );
        let links = CallLinks::of(&transcript);
        let messages = &transcript.messages;

        assert_eq!(links.results_of(&messages[0], "a"), [3]);
        assert_eq!(links.results_of(&messages[3], "a"), [5]);
        assert_eq!(links.call_of(&messages[1], "a"), None);
        assert_eq!(
            links.call_of(&messages[2], "a").unwrap().parts,
            [address(1, 1)]
        );
        assert_eq!(
            links.call_of(&messages[4], "a").unwrap().parts,
            [address(4, 1)]
        );
        assert_eq!(links.call_of(&messages[5], "a"), None);
    }

    #[test]
    fn a_calls_number_counts_every_call_of_its_message() {
        let mut mixed = text_message(1, Assistant, "I'll check both.");
        mixed.parts.extend([call("a", "Read"), call("b", "Grep")]);

        let numbers = numbered_calls(&mixed)
            .map(|(number, _)| number)
            .collect::<Vec<_>>();

        assert_eq!(numbers, [1, 2]);
    }
}
