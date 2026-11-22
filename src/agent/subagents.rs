//! The sub-agents of a session as `agent read` names them: a handle `sN` per
//! sub-agent, numbered in launch order over the whole session so it stays the
//! same in every read, and the facts its legend record states.

use crate::agent::address::MessageAddresses;
use crate::agent::call_links::{CallAddress, CallLinks, numbered_calls};
use crate::agent::protocol::{escape_atom, protocol_snippet};
use crate::agent::transcript::{AgentMessage, AgentMessagePart, AgentTranscript, SubagentOrigin};
use crate::log_entry::Tool;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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
    /// The ordinals of its own messages, in order. Empty for a sub-agent
    /// that has not written a message.
    messages: Vec<usize>,
    /// The messages carrying its reported text, in order.
    reports: Vec<usize>,
    /// The messages carrying its launching call's results, for a sub-agent
    /// that has not written a message: a read of it alone shows them.
    launch_results: Vec<usize>,
    /// `in=`: the sub-agent whose call launched it.
    parent: Option<Handle>,
    /// Its handle and legend record, shared by every record naming it.
    tag: SubagentTag,
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
    /// The sub-agents of `transcript` under `handles`, their legend records
    /// naming messages as `addresses` names them.
    pub fn of(
        transcript: &AgentTranscript,
        links: &CallLinks<'_>,
        handles: &SubagentHandles<'_>,
        addresses: &MessageAddresses,
    ) -> Self {
        let mut subagents = Self::default();
        for (thread, &handle) in handles.threads.iter().zip(&handles.thread_handles) {
            for &ordinal in &thread.messages {
                subagents.by_message.insert(ordinal, handle);
            }
        }
        for (index, owner) in handles.owners.iter().enumerate() {
            let handle = index + 1;
            let results_of = |call: IndexedCall<'_>| links.results_of(call.message, call.id);
            let (origin, launch, messages, launch_results, report) = match *owner {
                HandleOwner::Thread(thread) => {
                    let thread = &handles.threads[thread];
                    let report = thread
                        .launch
                        .filter(IndexedCall::waits_for_report)
                        .and_then(|call| results_of(call).first().copied());
                    let messages = thread.messages.clone();
                    (
                        Some(thread.origin),
                        thread.launch,
                        messages,
                        Vec::new(),
                        report,
                    )
                }
                HandleOwner::Launch(call) => (None, Some(call), Vec::new(), results_of(call), None),
            };
            let parent =
                launch.and_then(|call| subagents.by_message.get(&call.address.ordinal).copied());
            subagents.list.push(Subagent {
                handle,
                kind: kind_of(origin, launch),
                name: name_of(origin, launch),
                launch: launch.map(|call| call.address),
                reports: Vec::new(),
                launch_results,
                parent,
                tag: SubagentTag {
                    handle,
                    legend: Arc::from(""),
                },
                messages,
            });
            if let Some(call) = launch {
                subagents.launches.insert(call.address, handle);
            }
            if let Some(result) = report {
                subagents.add_report(result, handle);
            }
        }
        subagents.link_reports(&transcript.messages, handles);
        subagents.link_targets(&transcript.messages, handles);
        for subagent in &mut subagents.list {
            subagent.reports.sort_unstable();
            subagent.tag.legend = Arc::from(subagent.legend_record(addresses));
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

    /// The number of sub-agents, those without messages included: the
    /// highest handle.
    pub fn count(&self) -> usize {
        self.list.len()
    }

    /// The sub-agent with `handle`.
    pub fn tag(&self, handle: Handle) -> Option<&SubagentTag> {
        self.get(handle).map(|subagent| &subagent.tag)
    }

    /// The handle of the sub-agent message `ordinal` belongs to.
    pub fn handle_of(&self, ordinal: usize) -> Option<Handle> {
        self.by_message.get(&ordinal).copied()
    }

    /// The sub-agent message `ordinal` belongs to.
    pub fn tag_of(&self, ordinal: usize) -> Option<SubagentTag> {
        self.tag(self.handle_of(ordinal)?).cloned()
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
    /// its own messages and its reports, or its launching call's results
    /// when it has not written a message.
    pub fn messages_read_with(&self, handle: Handle) -> Option<Vec<usize>> {
        let subagent = self.get(handle)?;
        let mut messages = subagent
            .launch
            .map(|call| call.ordinal)
            .into_iter()
            .chain(subagent.messages.iter().copied())
            .chain(subagent.reports.iter().copied())
            .chain(subagent.launch_results.iter().copied())
            .collect::<Vec<_>>();
        messages.sort_unstable();
        messages.dedup();
        Some(messages)
    }

    /// Record each report message against the sub-agent it names, by the
    /// call that launched it, or by its agent id or agent path. Of the
    /// sub-agents a reused agent path names, the report answers the one
    /// started last before it.
    fn link_reports(&mut self, messages: &[AgentMessage], handles: &SubagentHandles<'_>) {
        let starts = &handles.starts;
        for message in messages.iter().filter(|message| message.is_report) {
            let sender = &message.report_sender;
            let reporter = handles
                .threads
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
                let handle = handles.thread_handles[thread];
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
    fn link_targets(&mut self, messages: &[AgentMessage], handles: &SubagentHandles<'_>) {
        let starts = &handles.starts;
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
                let target = handles
                    .threads
                    .iter()
                    .enumerate()
                    .filter(|&(index, _)| starts[index].precedes_call(address))
                    .filter(|(_, thread)| !thread.messages.contains(&message.ordinal))
                    .filter(|(_, thread)| thread.origin.is_addressed_as(recipient))
                    .max_by_key(|&(index, _)| starts[index]);
                if let Some((index, _)) = target {
                    self.targets.insert(address, handles.thread_handles[index]);
                }
            }
        }
    }
}

impl Subagent {
    /// `subagent s326 type=suite_runner launch=m15397 span=m15399..m15435
    /// report=m15411,m15437 in=s12 : fuel_generation_focused`, each atom left
    /// out when the transcript does not record it, and `span=` when the
    /// sub-agent has not written a message.
    fn legend_record(&self, addresses: &MessageAddresses) -> String {
        let mut record = format!("subagent s{}", self.handle);
        if let Some(kind) = &self.kind {
            record.push_str(&format!(" type={}", escape_atom(kind)));
        }
        if let Some(launch) = self.launch {
            record.push_str(&format!(" launch={}", addresses.message(launch.ordinal)));
        }
        if let (Some(&first), Some(&last)) = (self.messages.first(), self.messages.last()) {
            record.push_str(&format!(" span={}", addresses.range(first, last)));
        }
        if !self.reports.is_empty() {
            let reports = self
                .reports
                .iter()
                .map(|&ordinal| addresses.message(ordinal))
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

/// The handle of each sub-agent of a transcript. A sub-agent is a thread of
/// sub-agent messages, or, from the session's first linked launch onwards, a
/// launching call whose sub-agent has not written a message yet. Handles
/// count from 1 in launch order over the whole session, so a sub-agent keeps
/// its handle when one launched after it writes first.
pub struct SubagentHandles<'a> {
    /// The threads, in the order their first messages come.
    threads: Vec<Thread<'a>>,
    /// The start of each thread, by its index in `threads`.
    starts: Vec<ThreadStart>,
    /// The handle of each thread, by its index in `threads`.
    thread_handles: Vec<Handle>,
    /// The sub-agent each handle names, in handle order: handle `N` at
    /// `N - 1`.
    owners: Vec<HandleOwner<'a>>,
}

/// The sub-agent a handle names.
#[derive(Clone, Copy)]
enum HandleOwner<'a> {
    /// The thread at this index in [`SubagentHandles::threads`].
    Thread(usize),
    /// A launching call without a thread.
    Launch(IndexedCall<'a>),
}

impl<'a> SubagentHandles<'a> {
    pub fn of(transcript: &'a AgentTranscript) -> Self {
        let calls = launches_by_id(&transcript.messages);
        let mut threads = threads_of(&transcript.messages);
        for thread in &mut threads {
            thread.launch = thread
                .origin
                .launch_call
                .as_deref()
                .and_then(|id| calls.get(id).copied());
        }
        let thread_launches = threads
            .iter()
            .map(|thread| ThreadLaunch {
                first_message: thread.messages[0],
                launch: thread.launch.map(|call| call.address.into()),
            })
            .collect::<Vec<_>>();
        let starts = thread_launches
            .iter()
            .map(|&thread| ThreadStart::of(thread))
            .collect::<Vec<_>>();
        let launches = calls
            .values()
            .map(|call| (CallPosition::from(call.address), *call))
            .collect::<HashMap<_, _>>();
        let launch_order = handles_at_launch(&thread_launches, launches.keys().copied().collect());
        let mut owners = (0..threads.len())
            .map(HandleOwner::Thread)
            .zip(launch_order.threads.iter().copied())
            .chain(
                launch_order
                    .threadless
                    .iter()
                    .map(|&(address, handle)| (HandleOwner::Launch(launches[&address]), handle)),
            )
            .collect::<Vec<_>>();
        owners.sort_by_key(|&(_, handle)| handle);
        Self {
            threads,
            starts,
            thread_handles: launch_order.threads,
            owners: owners.into_iter().map(|(owner, _)| owner).collect(),
        }
    }

    /// Each sub-agent that has written messages: its handle and the
    /// ordinals of its messages, in order.
    pub fn threads(&self) -> impl Iterator<Item = (Handle, &[usize])> {
        self.thread_handles
            .iter()
            .zip(&self.threads)
            .map(|(&handle, thread)| (handle, thread.messages.as_slice()))
    }

    /// The number of handles, those of sub-agents without messages included.
    pub fn count(&self) -> usize {
        self.owners.len()
    }
}

/// One sub-agent thread's messages, in transcript order, and the call that
/// launched it when the transcript links the two.
struct Thread<'a> {
    origin: &'a SubagentOrigin,
    messages: Vec<usize>,
    launch: Option<IndexedCall<'a>>,
}

/// The start of a sub-agent in the session: the position of the call that
/// launched it, or its thread's first message when the launch is unknown. A
/// first message orders before the calls it holds.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ThreadStart {
    ordinal: usize,
    /// The launching call's rank in its message; `None` for a first message.
    call: Option<usize>,
}

impl ThreadStart {
    fn of(thread: ThreadLaunch) -> Self {
        match thread.launch {
            Some(call) => Self::of_call(call),
            None => Self {
                ordinal: thread.first_message,
                call: None,
            },
        }
    }

    fn of_call(position: CallPosition) -> Self {
        Self {
            ordinal: position.ordinal,
            call: Some(position.rank),
        }
    }

    fn precedes_message(self, ordinal: usize) -> bool {
        self.ordinal < ordinal
    }

    fn precedes_call(self, address: CallAddress) -> bool {
        self < Self::of_call(address.into())
    }
}

/// A sub-agent's place in launch order: its start, then its thread's first
/// message, which a launching call without a thread lacks.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LaunchOrder {
    start: ThreadStart,
    first_message: Option<usize>,
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
                launch: None,
            });
            threads.len() - 1
        });
        threads[index].messages.push(message.ordinal);
    }
    threads
}

/// A launching call's position in session order: its message's ordinal,
/// then its rank among that message's calls. [`handles_at_launch`] reads
/// their order and nothing else: a read ranks a call among every call of its
/// message, and search's cache lookup among the message's launching calls.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CallPosition {
    pub ordinal: usize,
    pub rank: usize,
}

impl From<CallAddress> for CallPosition {
    fn from(address: CallAddress) -> Self {
        Self {
            ordinal: address.ordinal,
            rank: address.number,
        }
    }
}

/// A sub-agent thread as launch order counts it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ThreadLaunch {
    pub first_message: usize,
    /// The call that launched it, when the transcript links the two.
    pub launch: Option<CallPosition>,
}

/// The handles [`handles_at_launch`] gives.
#[derive(Debug)]
pub(crate) struct LaunchOrderHandles {
    /// The handle of each thread, in the threads' order.
    pub threads: Vec<Handle>,
    /// Each launching call that holds a handle without a thread, with it.
    pub threadless: Vec<(CallPosition, Handle)>,
}

/// The handle of each of `threads`, and of each of `launches` that holds one
/// without a thread, numbered from 1 in launch order. `launches` are the
/// session's launching calls, each id's first call alone: a later call with
/// the same id is a copy. Reads and search's cache lookup both count handles
/// here, so a hit names the sub-agent a read of it names.
pub(crate) fn handles_at_launch(
    threads: &[ThreadLaunch],
    mut launches: Vec<CallPosition>,
) -> LaunchOrderHandles {
    launches.sort_unstable();
    let linked = threads
        .iter()
        .filter_map(|thread| thread.launch)
        .collect::<HashSet<_>>();
    let threadless = threadless_launches(launches, &linked);
    let orders = threads
        .iter()
        .map(|&thread| LaunchOrder {
            start: ThreadStart::of(thread),
            first_message: Some(thread.first_message),
        })
        .chain(threadless.iter().map(|&call| LaunchOrder {
            start: ThreadStart::of_call(call),
            first_message: None,
        }))
        .collect::<Vec<_>>();
    let mut handles = handles_in_launch_order(&orders);
    let threadless_handles = handles.split_off(threads.len());
    LaunchOrderHandles {
        threads: handles,
        threadless: threadless.into_iter().zip(threadless_handles).collect(),
    }
}

/// The launching calls, of `launches` in session order, that hold a handle
/// without a thread: from the session's first call `linked` to a thread
/// onwards, each one not linked to a thread. A launching call before that
/// call, or in a session without a link, doesn't hold a handle; its thread,
/// once it writes, gets one by its first message.
fn threadless_launches(
    launches: Vec<CallPosition>,
    linked: &HashSet<CallPosition>,
) -> Vec<CallPosition> {
    let Some(&first_linked) = linked.iter().min() else {
        return Vec::new();
    };
    launches
        .into_iter()
        .filter(|call| *call >= first_linked && !linked.contains(call))
        .collect()
}

/// The handle of each sub-agent, by its index in `orders`: numbered from 1 in
/// launch order.
fn handles_in_launch_order(orders: &[LaunchOrder]) -> Vec<Handle> {
    let mut by_order = (0..orders.len()).collect::<Vec<_>>();
    by_order.sort_by_key(|&index| orders[index]);
    let mut handles = vec![0; orders.len()];
    for (position, index) in by_order.into_iter().enumerate() {
        handles[index] = position + 1;
    }
    handles
}

/// The first launching call, of tool kind `Agent`, of each id in session
/// order. A later call with the same id is a copy, such as the parent's
/// history a Claude Code fork opens with: it launches nothing. The cache's
/// index on launching calls reads the same calls for search.
fn launches_by_id(messages: &[AgentMessage]) -> HashMap<&str, IndexedCall<'_>> {
    let mut calls: HashMap<&str, IndexedCall<'_>> = HashMap::new();
    for message in messages {
        for (number, part) in numbered_calls(message) {
            let AgentMessagePart::ToolUse {
                id,
                name,
                tool: tool @ Tool::Agent,
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
            calls.entry(id.as_str()).or_insert(call);
        }
    }
    calls
}

/// `type=`: the role the sub-agent's identity records, else the agent type
/// its launching call asked for.
fn kind_of(origin: Option<&SubagentOrigin>, launch: Option<IndexedCall<'_>>) -> Option<String> {
    let requested = || {
        let input = launch?.input;
        input
            .get("subagent_type")
            .or_else(|| input.get("launch")?.get("agent_type"))
            .and_then(Value::as_str)
    };
    origin
        .and_then(|origin| origin.identity.role.as_deref())
        .or_else(requested)
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
}

/// The name a legend record gives the sub-agent: the task name its agent
/// path ends with (Codex), else its launching call's description, else the
/// label its provider knows its thread by, else the launching call's name.
fn name_of(origin: Option<&SubagentOrigin>, launch: Option<IndexedCall<'_>>) -> String {
    let task = origin
        .and_then(|origin| origin.identity.agent_path.as_deref())
        .and_then(task_name);
    let description = || {
        let call = launch?;
        (call.tool == Tool::Agent)
            .then(|| call.input.get("description")?.as_str())
            .flatten()
    };
    task.or_else(description)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| Some(origin?.label.as_str()))
        .or_else(|| Some(launch?.name))
        .unwrap_or_default()
        .to_owned()
}

/// The task name an agent path ends with: `scout` for `/root/scout`.
fn task_name(agent_path: &str) -> Option<&str> {
    agent_path.rsplit('/').find(|segment| !segment.is_empty())
}
