//! The sub-agents of a session as `agent read` names them: a handle `sN` per
//! sub-agent, numbered in launch order over the whole session so it stays the
//! same in every read, and the facts its legend record states.

use crate::agent::call_links::{CallAddress, CallLinks, numbered_calls};
use crate::agent::protocol::{escape_atom, protocol_snippet};
use crate::agent::record_writer::ordinal_range;
use crate::agent::transcript::{AgentMessage, AgentMessagePart, AgentTranscript, SubagentOrigin};
use crate::log_entry::Tool;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// The characters a sub-agent's name keeps in its legend record.
const NAME_CHARS: usize = 120;

/// A sub-agent's handle: `sN`.
pub type Handle = usize;

/// The sub-agent a record names in position 2, with its legend record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentTag {
    pub handle: Handle,
    legend: Arc<str>,
}

impl SubagentTag {
    /// The sub-agent's legend record, with its line break.
    pub fn legend(&self) -> &str {
        &self.legend
    }
}

/// The sub-agents of one transcript.
#[derive(Debug, Default)]
pub struct Subagents {
    /// In handle order: the sub-agent with handle `N` is at `N - 1`.
    list: Vec<Subagent>,
    by_message: HashMap<usize, Handle>,
    launches: HashMap<CallAddress, Handle>,
    targets: HashMap<CallAddress, Handle>,
    reports: HashMap<usize, Handle>,
}

/// One sub-agent and the facts its legend record states.
#[derive(Debug)]
struct Subagent {
    handle: Handle,
    /// `type=`: the agent type it ran as, when recorded.
    kind: Option<String>,
    name: String,
    /// The call that launched it, when the transcript links the two.
    launch: Option<CallAddress>,
    /// The ordinals of its own messages, in order.
    messages: Vec<usize>,
    /// The messages carrying its reported text, in order.
    reports: Vec<usize>,
    /// `in=`: the sub-agent whose call launched it.
    parent: Option<Handle>,
    /// Its legend record, shared by every record naming it.
    legend: Arc<str>,
}

#[derive(Clone, Copy)]
struct IndexedCall<'a> {
    message: &'a AgentMessage,
    id: &'a str,
    address: CallAddress,
    name: &'a str,
    tool: Tool,
    input: &'a Value,
}

impl IndexedCall<'_> {
    /// True for a Claude Code `Agent` or `Task` call that waits for the
    /// sub-agent it launches: its result carries the sub-agent's final
    /// reply. A call run in the background returns at once, and the report
    /// arrives later as a notification.
    fn waits_for_report(&self) -> bool {
        self.tool == Tool::Agent
            && matches!(self.name, "Agent" | "Task")
            && self.input.get("run_in_background").and_then(Value::as_bool) != Some(true)
    }
}

impl Subagents {
    pub fn of(transcript: &AgentTranscript, links: &CallLinks<'_>) -> Self {
        let threads = threads_of(&transcript.messages);
        if threads.is_empty() {
            return Self::default();
        }
        let calls = calls_by_id(&transcript.messages);
        let thread_of_message = threads
            .iter()
            .enumerate()
            .flat_map(|(index, thread)| {
                thread.messages.iter().map(move |&ordinal| (ordinal, index))
            })
            .collect::<HashMap<_, _>>();
        let launch_of = |thread: &Thread<'_>| {
            thread
                .origin
                .launch_call
                .as_deref()
                .and_then(|id| calls.get(id).copied().flatten())
        };
        let starts = threads
            .iter()
            .map(|thread| ThreadStart::of(thread, launch_of(thread)))
            .collect::<Vec<_>>();
        let handle_of_thread = handles_in_launch_order(&threads, &starts);
        let mut subagents = Self::default();
        for (index, _) in sorted_by_handle(&handle_of_thread) {
            let thread = &threads[index];
            let launch = launch_of(thread);
            let handle = handle_of_thread[&index];
            let parent = launch
                .and_then(|call| thread_of_message.get(&call.address.ordinal))
                .map(|thread| handle_of_thread[thread]);
            subagents.list.push(Subagent {
                handle,
                kind: kind_of(thread.origin, launch),
                name: name_of(thread.origin, launch),
                launch: launch.map(|call| call.address),
                messages: thread.messages.clone(),
                reports: Vec::new(),
                parent,
                legend: Arc::from(""),
            });
            for &ordinal in &thread.messages {
                subagents.by_message.insert(ordinal, handle);
            }
            if let Some(call) = launch {
                subagents.launches.insert(call.address, handle);
                if call.waits_for_report()
                    && let Some(&result) = links.results_of(call.message, call.id).first()
                {
                    subagents.add_report(result, handle);
                }
            }
        }
        subagents.link_reports(&transcript.messages, &threads, &starts, &handle_of_thread);
        subagents.link_targets(&transcript.messages, &threads, &starts, &handle_of_thread);
        for subagent in &mut subagents.list {
            subagent.reports.sort_unstable();
            subagent.legend = Arc::from(subagent.legend_record());
        }
        subagents
    }

    /// Record message `ordinal` as carrying sub-agent `handle`'s report:
    /// `from=` on its record and `report=` in the legend.
    fn add_report(&mut self, ordinal: usize, handle: Handle) {
        self.add_finish_notice(ordinal, handle);
        self.list[handle - 1].reports.push(ordinal);
    }

    /// Record message `ordinal` as sub-agent `handle`'s finish notice,
    /// without the reported text: `from=` on its record alone.
    fn add_finish_notice(&mut self, ordinal: usize, handle: Handle) {
        self.reports.insert(ordinal, handle);
    }

    fn get(&self, handle: Handle) -> Option<&Subagent> {
        handle.checked_sub(1).and_then(|index| self.list.get(index))
    }

    /// The sub-agent message `ordinal` belongs to.
    pub fn tag_of(&self, ordinal: usize) -> Option<SubagentTag> {
        let handle = *self.by_message.get(&ordinal)?;
        self.get(handle).map(Subagent::tag)
    }

    /// The sub-agent the call at `address` launches: `launches=`.
    pub fn launched_by(&self, address: CallAddress) -> Option<Handle> {
        self.launches.get(&address).copied()
    }

    /// The running sub-agent the call at `address` sends to: `target=`.
    pub fn targeted_by(&self, address: CallAddress) -> Option<Handle> {
        self.targets.get(&address).copied()
    }

    /// The sub-agent whose report or finish notice message `ordinal`
    /// carries: `from=`.
    pub fn reporter_of(&self, ordinal: usize) -> Option<Handle> {
        self.reports.get(&ordinal).copied()
    }

    /// Each message a read of sub-agent `handle` alone shows: its launch,
    /// its own messages and its reports.
    pub fn messages_read_with(&self, handle: Handle) -> Option<Vec<usize>> {
        let subagent = self.get(handle)?;
        let mut messages = subagent
            .launch
            .map(|call| call.ordinal)
            .into_iter()
            .chain(subagent.messages.iter().copied())
            .chain(subagent.reports.iter().copied())
            .collect::<Vec<_>>();
        messages.sort_unstable();
        messages.dedup();
        Some(messages)
    }

    /// Record each report message against the sub-agent it names, by the
    /// call that launched it, or by its agent id or agent path. Of the
    /// sub-agents a reused agent path names, the report answers the one
    /// started last before it.
    fn link_reports(
        &mut self,
        messages: &[AgentMessage],
        threads: &[Thread<'_>],
        starts: &[ThreadStart],
        handle_of_thread: &HashMap<usize, Handle>,
    ) {
        for message in messages.iter().filter(|message| message.is_report) {
            let sender = &message.report_sender;
            let reporter = threads
                .iter()
                .enumerate()
                .filter(|(_, thread)| {
                    let origin = thread.origin;
                    let by_launch =
                        sender.launch_call.is_some() && sender.launch_call == origin.launch_call;
                    let by_id = sender.task_id.as_ref().is_some_and(|task_id| {
                        origin.identity.agent_id.as_ref() == Some(task_id)
                            || origin.identity.agent_path.as_ref() == Some(task_id)
                    });
                    by_launch || by_id
                })
                .filter(|&(index, _)| starts[index].precedes_message(message.ordinal))
                .max_by_key(|&(index, _)| starts[index])
                .map(|(index, _)| index);
            if let Some(thread) = reporter {
                let handle = handle_of_thread[&thread];
                if message.report_has_text {
                    self.add_report(message.ordinal, handle);
                } else {
                    self.add_finish_notice(message.ordinal, handle);
                }
            }
        }
    }

    /// Record each call sending to a running sub-agent against it. A call
    /// names its recipient by label, agent id, agent path or task name, and
    /// sends to the sub-agent of that name started last before it. That
    /// sub-agent may not have written a message yet.
    fn link_targets(
        &mut self,
        messages: &[AgentMessage],
        threads: &[Thread<'_>],
        starts: &[ThreadStart],
        handle_of_thread: &HashMap<usize, Handle>,
    ) {
        for message in messages {
            for (number, part) in numbered_calls(message) {
                let AgentMessagePart::ToolUse {
                    tool: Tool::AgentMessage,
                    input,
                    ..
                } = part
                else {
                    continue;
                };
                let Some(recipient) = input.get("recipient").and_then(Value::as_str) else {
                    continue;
                };
                let address = CallAddress {
                    ordinal: message.ordinal,
                    number,
                };
                let target = threads
                    .iter()
                    .enumerate()
                    .filter(|&(index, _)| starts[index].precedes_call(address))
                    .filter(|(_, thread)| !thread.messages.contains(&message.ordinal))
                    .filter(|(_, thread)| thread.origin.is_addressed_as(recipient))
                    .max_by_key(|&(index, _)| starts[index]);
                if let Some((index, _)) = target {
                    self.targets.insert(address, handle_of_thread[&index]);
                }
            }
        }
    }
}

impl Subagent {
    /// `subagent s326 type=suite_runner launch=m15397 span=m15399..m15435
    /// report=m15411,m15437 in=s12 : fuel_generation_focused`, each atom left
    /// out when the transcript does not record it.
    fn legend_record(&self) -> String {
        let mut record = format!("subagent s{}", self.handle);
        if let Some(kind) = &self.kind {
            record.push_str(&format!(" type={}", escape_atom(kind)));
        }
        if let Some(launch) = self.launch {
            record.push_str(&format!(" launch=m{}", launch.ordinal));
        }
        let (first, last) = (self.messages[0], self.messages[self.messages.len() - 1]);
        record.push_str(&format!(" span={}", ordinal_range(first, last)));
        if !self.reports.is_empty() {
            let reports = self
                .reports
                .iter()
                .map(|ordinal| format!("m{ordinal}"))
                .collect::<Vec<_>>();
            record.push_str(&format!(" report={}", reports.join(",")));
        }
        if let Some(parent) = self.parent {
            record.push_str(&format!(" in=s{parent}"));
        }
        record.push_str(&format!(
            " : {}\n",
            protocol_snippet(&self.name, NAME_CHARS)
        ));
        record
    }

    fn tag(&self) -> SubagentTag {
        SubagentTag {
            handle: self.handle,
            legend: self.legend.clone(),
        }
    }
}

impl SubagentOrigin {
    /// True when a call addresses the thread as `recipient`: by its label,
    /// its agent id, its agent path or the task name its path ends with. A
    /// Codex thread's label is its thread id; `send_input` names it.
    fn is_addressed_as(&self, recipient: &str) -> bool {
        let identity = &self.identity;
        self.label == recipient
            || identity.agent_id.as_deref() == Some(recipient)
            || identity
                .agent_path
                .as_deref()
                .is_some_and(|path| path == recipient || task_name(path) == Some(recipient))
    }
}

/// One sub-agent thread's messages, in transcript order.
struct Thread<'a> {
    origin: &'a SubagentOrigin,
    messages: Vec<usize>,
}

/// The start of a sub-agent thread in the session: the address of the call
/// that launched it, or its first message when the launch is unknown. A
/// first message orders before the calls it holds.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ThreadStart {
    ordinal: usize,
    /// The launching call's number; `None` for a first message.
    call: Option<usize>,
}

impl ThreadStart {
    fn of(thread: &Thread<'_>, launch: Option<IndexedCall<'_>>) -> Self {
        match launch {
            Some(call) => Self {
                ordinal: call.address.ordinal,
                call: Some(call.address.number),
            },
            None => Self {
                ordinal: thread.messages[0],
                call: None,
            },
        }
    }

    fn precedes_message(self, ordinal: usize) -> bool {
        self.ordinal < ordinal
    }

    fn precedes_call(self, address: CallAddress) -> bool {
        self < Self {
            ordinal: address.ordinal,
            call: Some(address.number),
        }
    }
}

/// The threads of `messages`, each with the ordinals of its messages, in
/// the order their first messages come.
fn threads_of(messages: &[AgentMessage]) -> Vec<Thread<'_>> {
    let mut threads: Vec<Thread<'_>> = Vec::new();
    let mut index_of: HashMap<(&str, Option<&Path>), usize> = HashMap::new();
    for message in messages {
        let Some(origin) = message.subagent.as_deref() else {
            continue;
        };
        let index = *index_of.entry(origin.key()).or_insert_with(|| {
            threads.push(Thread {
                origin,
                messages: Vec::new(),
            });
            threads.len() - 1
        });
        threads[index].messages.push(message.ordinal);
    }
    threads
}

/// The handle of each thread, by its index in `threads`: numbered from 1 in
/// the order of their `starts`, then of their first messages.
fn handles_in_launch_order(
    threads: &[Thread<'_>],
    starts: &[ThreadStart],
) -> HashMap<usize, Handle> {
    let mut order = (0..threads.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| (starts[index], threads[index].messages[0]));
    order
        .into_iter()
        .enumerate()
        .map(|(position, index)| (index, position + 1))
        .collect()
}

fn sorted_by_handle(handles: &HashMap<usize, Handle>) -> Vec<(usize, Handle)> {
    let mut sorted = handles
        .iter()
        .map(|(&index, &handle)| (index, handle))
        .collect::<Vec<_>>();
    sorted.sort_by_key(|&(_, handle)| handle);
    sorted
}

/// Each call by id; `None` for an id on more than one call.
fn calls_by_id(messages: &[AgentMessage]) -> HashMap<&str, Option<IndexedCall<'_>>> {
    let mut calls: HashMap<&str, Option<IndexedCall<'_>>> = HashMap::new();
    for message in messages {
        for (number, part) in numbered_calls(message) {
            let AgentMessagePart::ToolUse {
                id,
                name,
                tool,
                input,
                ..
            } = part
            else {
                continue;
            };
            let call = IndexedCall {
                message,
                id,
                address: CallAddress {
                    ordinal: message.ordinal,
                    number,
                },
                name,
                tool: *tool,
                input,
            };
            calls
                .entry(id.as_str())
                .and_modify(|seen| *seen = None)
                .or_insert(Some(call));
        }
    }
    calls
}

/// `type=`: the role the sub-agent's identity records, else the agent type
/// its launching call asked for.
fn kind_of(origin: &SubagentOrigin, launch: Option<IndexedCall<'_>>) -> Option<String> {
    let requested = || {
        let input = launch?.input;
        input
            .get("subagent_type")
            .or_else(|| input.get("launch")?.get("agent_type"))
            .and_then(Value::as_str)
    };
    origin
        .identity
        .role
        .as_deref()
        .or_else(requested)
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
}

/// The name a legend record gives the sub-agent: the task name its agent
/// path ends with (Codex), else its launching call's description (a Claude
/// Code `Agent` call), else the label its provider knows it by.
fn name_of(origin: &SubagentOrigin, launch: Option<IndexedCall<'_>>) -> String {
    let task = origin.identity.agent_path.as_deref().and_then(task_name);
    let description = || {
        let call = launch?;
        (call.tool == Tool::Agent)
            .then(|| call.input.get("description")?.as_str())
            .flatten()
    };
    task.or_else(description)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&origin.label)
        .to_owned()
}

/// The task name an agent path ends with: `scout` for `/root/scout`.
fn task_name(agent_path: &str) -> Option<&str> {
    agent_path.rsplit('/').find(|segment| !segment.is_empty())
}
