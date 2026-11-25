//! The addresses `agent read`, `agent outline` and search hits print for a
//! transcript's messages and calls, and the messages a ref names. A message's address is
//! its number among its thread's messages: `m7` for the session's own,
//! `s3:m2` for sub-agent s3's. Records, links, legends and recipes print every
//! address through [`MessageAddresses`], so a ref copied from any of them
//! names the same message.

use crate::agent::call_links::CallAddress;
use crate::agent::diagnostic::AgentError;
use crate::agent::refs::{MessageRange, Thread, ThreadRange};
use crate::agent::subagents::{Handle, SubagentHandles};
use crate::agent::transcript::AgentTranscript;
use crate::error::Result;
use std::collections::HashMap;

/// The addresses of one transcript's messages.
#[derive(Debug)]
pub struct MessageAddresses {
    /// Each message's ordinal, in order.
    ordinals: Vec<usize>,
    /// The place of the message at the same index of `ordinals`.
    places: Vec<Place>,
    /// The ordinals of each thread's messages, in order.
    threads: HashMap<Thread, Vec<usize>>,
    /// The number of sub-agent handles, those of sub-agents without messages
    /// included.
    handles: usize,
}

/// A message's thread and its number among that thread's messages.
#[derive(Clone, Copy, Debug)]
struct Place {
    thread: Thread,
    number: usize,
}

/// The messages a ref names, by their ordinals: every message from
/// `range.start` to `range.end`, or sub-agent `subagent`'s alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedRange {
    pub range: MessageRange,
    pub subagent: Option<Handle>,
    /// The thread's last message, when the ref's range ended past it and
    /// was shortened to it: `last=` in the header.
    pub shortened_to: Option<ThreadRange>,
}

impl MessageAddresses {
    /// The addresses of `transcript`'s messages, its sub-agents named by
    /// `handles`.
    pub fn of(transcript: &AgentTranscript, handles: &SubagentHandles<'_>) -> Self {
        let mut thread_of = HashMap::new();
        for (handle, messages) in handles.threads() {
            for &ordinal in messages {
                thread_of.insert(ordinal, Thread::Subagent(handle));
            }
        }
        let mut threads: HashMap<Thread, Vec<usize>> = HashMap::new();
        let mut ordinals = Vec::with_capacity(transcript.messages.len());
        let mut places = Vec::with_capacity(transcript.messages.len());
        for message in &transcript.messages {
            let thread = thread_of
                .get(&message.ordinal)
                .copied()
                .unwrap_or(Thread::Parent);
            let messages = threads.entry(thread).or_default();
            messages.push(message.ordinal);
            ordinals.push(message.ordinal);
            places.push(Place {
                thread,
                number: messages.len(),
            });
        }
        Self {
            ordinals,
            places,
            threads,
            handles: handles.count(),
        }
    }

    /// The addresses of `transcript`'s messages.
    pub fn of_transcript(transcript: &AgentTranscript) -> Self {
        Self::of(transcript, &SubagentHandles::of(transcript))
    }

    /// `m7` or `s3:m2`: message `ordinal`.
    pub fn message(&self, ordinal: usize) -> String {
        self.address(ordinal).to_string()
    }

    /// Message `ordinal`'s thread and number in it.
    pub fn address(&self, ordinal: usize) -> ThreadRange {
        let place = self.place(ordinal);
        ThreadRange::one(place.thread, place.number)
    }

    /// `m7`, `m7..m9` or `s3:m2..m4`: messages `first` to `last`. Messages of
    /// two threads print as the groups that contain them, `m7..m9`.
    pub fn range(&self, first: usize, last: usize) -> String {
        let (first_place, last_place) = (self.place(first), self.place(last));
        if first_place.thread == last_place.thread {
            ThreadRange::closed(first_place.thread, first_place.number, last_place.number)
                .to_string()
        } else {
            self.groups(first, last)
        }
    }

    /// `m7.2` or `s3:m2.2`: the call at `call`.
    pub fn call(&self, call: CallAddress) -> String {
        format!("{}.{}", self.message(call.ordinal), call.number)
    }

    /// `m7.2..4`: calls `first` to `last` of message `ordinal`.
    pub fn calls(&self, ordinal: usize, first: usize, last: usize) -> String {
        let call = |number| CallAddress { ordinal, number };
        if first == last {
            self.call(call(first))
        } else {
            format!("{}..{last}", self.call(call(first)))
        }
    }

    /// `m7..m9`: the session's own messages whose groups contain messages
    /// `first` to `last`. A group is a message of the session's own and the
    /// sub-agent messages after it, before the next; the first group also
    /// contains the sub-agent messages before it.
    pub fn groups(&self, first: usize, last: usize) -> String {
        ThreadRange::closed(Thread::Parent, self.group_of(first), self.group_of(last)).to_string()
    }

    /// The number of the session's own message whose group contains message
    /// `ordinal`; 0 in a session without messages of its own.
    pub fn group_of(&self, ordinal: usize) -> usize {
        group_in(self.thread_messages(Thread::Parent), ordinal)
    }

    /// The places of the messages `ordinals` names, for search hits on them.
    pub fn hit_places(&self, ordinals: impl IntoIterator<Item = usize>) -> HitPlaces {
        HitPlaces::of(&self.threads, &ordinals.into_iter().collect::<Vec<_>>())
    }

    /// The refs, after the conversation's, that read the messages of `runs`,
    /// consecutive ordinals, in order:
    /// - the groups containing the session's own messages of `runs` as one range
    ///   per run of consecutive groups, `m119..m125` or `m7`;
    /// - the other sub-agent messages as one range per sub-agent's
    ///   consecutive messages, `s3:m4..m9`.
    ///
    /// The refs come in the order of their first messages. A ref of the
    /// session's own messages reads their whole groups with `--subagents`: in
    /// a group with messages left out, it reads them too.
    pub fn refs(&self, runs: &[(usize, usize)]) -> Vec<String> {
        self.ref_ranges(&self.indices_of(runs))
            .into_iter()
            .map(|range| self.ref_address(range).to_string())
            .collect()
    }

    /// The indices, in `ordinals`, of the messages of `runs`, in order and
    /// each once.
    fn indices_of(&self, runs: &[(usize, usize)]) -> Vec<usize> {
        let mut indices = runs
            .iter()
            .flat_map(|&(first, last)| self.index(first)..=self.index(last))
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    /// The ranges of the refs that read the messages at the sorted `indices`,
    /// in the order of their first messages.
    fn ref_ranges(&self, indices: &[usize]) -> Vec<RefRange> {
        let mut ranges = RefRanges::default();
        for group in indices.chunk_by(|&left, &right| self.group_at(left) == self.group_at(right)) {
            if self.contains_own_message(group) {
                ranges.add_group(self.ordinals[group[0]], self.group_at(group[0]));
            } else {
                for &index in group {
                    ranges.add_subagent_message(self.ordinals[index], self.places[index]);
                }
            }
        }
        ranges.in_order()
    }

    /// True when one of the messages at `indices` is the session's own.
    fn contains_own_message(&self, indices: &[usize]) -> bool {
        indices
            .iter()
            .any(|&index| self.places[index].thread == Thread::Parent)
    }

    /// `m7`, `m7..m9` or `s3:m4..m9`: the ref reading `range`.
    fn ref_address(&self, range: RefRange) -> ThreadRange {
        match range {
            RefRange::Groups(first, last) => ThreadRange::closed(Thread::Parent, first, last),
            RefRange::Run(thread, first, last) => ThreadRange::closed(thread, first, last),
        }
    }

    /// The messages `reference` names, or the error naming what it misses:
    /// - `mA..mB` is messages A to B of the session's own with the
    ///   sub-agent messages written after mA, from the session's start when
    ///   A is 1, and before m(B+1); `mN` is `mN..mN`;
    /// - `sN:mA..mB` is sub-agent N's messages A to B alone.
    ///
    /// An open end, or an end past the thread's last message, is that last
    /// message. A start past it is `out-of-range`.
    pub fn resolve(&self, reference: ThreadRange) -> Result<ResolvedRange> {
        let thread = reference.thread;
        if let Thread::Subagent(handle) = thread
            && (handle == 0 || handle > self.handles)
        {
            return Err(AgentError::unknown_subagent(handle).into());
        }
        let messages = self.thread_messages(thread);
        let length = messages.len();
        if reference.first > length {
            return Err(AgentError::out_of_range(None, self.past_end(reference)).into());
        }
        let (first, last) = (
            reference.first,
            reference.last.unwrap_or(length).min(length),
        );
        let range = match thread {
            Thread::Parent => self.groups_span(first, last),
            Thread::Subagent(_) => MessageRange {
                start: messages[first - 1],
                end: messages[last - 1],
            },
        };
        Ok(ResolvedRange {
            range,
            subagent: match thread {
                Thread::Parent => None,
                Thread::Subagent(handle) => Some(handle),
            },
            shortened_to: reference
                .last
                .is_some_and(|named_last| named_last > length)
                .then(|| ThreadRange::one(thread, length)),
        })
    }

    /// The detail of the error for `reference` starting past its thread's
    /// last message.
    fn past_end(&self, reference: ThreadRange) -> String {
        let length = self.thread_messages(reference.thread).len();
        let last = ThreadRange::one(reference.thread, length);
        match (reference.thread, length) {
            (Thread::Parent, 0) => format!(
                "message range {reference}: the session does not have messages of its own{}",
                self.subagent_numbering_note()
            ),
            (Thread::Parent, _) => format!(
                "message range {reference} starts after the last message, {last}{}",
                self.subagent_numbering_note()
            ),
            (Thread::Subagent(handle), 0) => {
                format!("message range {reference}: s{handle} has not written a message")
            }
            (Thread::Subagent(handle), _) => {
                format!("message range {reference} starts after s{handle}'s last message, {last}")
            }
        }
    }

    /// `; sub-agent messages are numbered sN:mK` in a transcript with
    /// sub-agent messages; empty otherwise.
    fn subagent_numbering_note(&self) -> &'static str {
        if self.threads.keys().any(|&thread| thread != Thread::Parent) {
            "; sub-agent messages are numbered sN:mK"
        } else {
            ""
        }
    }

    fn thread_messages(&self, thread: Thread) -> &[usize] {
        self.threads.get(&thread).map_or(&[], Vec::as_slice)
    }

    fn index(&self, ordinal: usize) -> usize {
        self.ordinals
            .binary_search(&ordinal)
            .expect("an address names a message of the transcript")
    }

    fn place(&self, ordinal: usize) -> Place {
        self.places[self.index(ordinal)]
    }

    /// The number of the session's own message whose group contains the
    /// message at `index` in `ordinals`.
    fn group_at(&self, index: usize) -> usize {
        self.group_of(self.ordinals[index])
    }

    /// The ordinals of the first and last messages of the groups of the
    /// session's own messages `first` to `last`.
    fn groups_span(&self, first: usize, last: usize) -> MessageRange {
        let parent = self.thread_messages(Thread::Parent);
        MessageRange {
            start: if first == 1 {
                self.ordinals[0]
            } else {
                parent[first - 1]
            },
            end: match parent.get(last) {
                Some(&next) => self.ordinals[self.index(next) - 1],
                None => *self.ordinals.last().expect("a group contains a message"),
            },
        }
    }
}

/// The messages one of [`MessageAddresses::refs`]'s refs reads, by number.
#[derive(Clone, Copy)]
enum RefRange {
    /// Every message of the groups of the session's own messages `first` to
    /// `last`.
    Groups(usize, usize),
    /// Messages `first` to `last` of one thread alone. `ref_ranges` adds a
    /// run for the messages of a group without its own message, all of
    /// them sub-agent messages.
    Run(Thread, usize, usize),
}

/// The ranges of [`MessageAddresses::refs`]'s refs, added in message order.
#[derive(Default)]
struct RefRanges {
    /// Each range with the ordinal of its first message.
    ranges: Vec<(usize, RefRange)>,
    /// Each sub-agent's run still growing: its first ordinal, first and last
    /// number.
    open_runs: HashMap<Thread, (usize, usize, usize)>,
}

impl RefRanges {
    /// Add the whole group of the session's own message `number`, its first
    /// message `first_ordinal`. A group that follows the groups added last
    /// extends their range.
    fn add_group(&mut self, first_ordinal: usize, number: usize) {
        match self.ranges.last_mut() {
            Some((_, RefRange::Groups(_, last))) if *last + 1 == number => *last = number,
            _ => self
                .ranges
                .push((first_ordinal, RefRange::Groups(number, number))),
        }
    }

    /// Add sub-agent message `ordinal`, at `place`, to its thread's run when
    /// it follows that run's last message.
    fn add_subagent_message(&mut self, ordinal: usize, place: Place) {
        let Place { thread, number } = place;
        match self.open_runs.get_mut(&thread) {
            Some((_, _, last)) if *last + 1 == number => *last = number,
            _ => {
                if let Some((first_ordinal, first, last)) =
                    self.open_runs.insert(thread, (ordinal, number, number))
                {
                    self.ranges
                        .push((first_ordinal, RefRange::Run(thread, first, last)));
                }
            }
        }
    }

    /// The ranges, the runs still growing included, in the order of their
    /// first messages.
    fn in_order(mut self) -> Vec<RefRange> {
        for (thread, (first_ordinal, first, last)) in self.open_runs {
            self.ranges
                .push((first_ordinal, RefRange::Run(thread, first, last)));
        }
        self.ranges.sort_by_key(|&(first_ordinal, _)| first_ordinal);
        self.ranges.into_iter().map(|(_, range)| range).collect()
    }
}

/// The number of the message of `parent`, the ordinals of the session's own
/// messages, whose group contains message `ordinal`; 0 without any.
fn group_in(parent: &[usize], ordinal: usize) -> usize {
    parent
        .partition_point(|&start| start <= ordinal)
        .max(1)
        .min(parent.len())
}

/// The places of some of a session's messages, for the addresses of search
/// hits on them: each message's thread, number and group, and the length of
/// each thread containing one of them and of the session's own.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HitPlaces {
    messages: HashMap<usize, HitPlace>,
    lengths: HashMap<Thread, usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HitPlace {
    thread: Thread,
    number: usize,
    group: usize,
}

/// A search hit's `hit=` and the length of its thread. The range its
/// `read ref=` recipe reads ends at that length at the latest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HitAddresses {
    pub hit: ThreadRange,
    pub thread_length: usize,
}

impl HitAddresses {
    /// The range the hit's `read ref=` recipe reads: the hit and `context`'s
    /// messages of its thread before and after it, shortened at the
    /// thread's ends.
    pub fn recipe_range(&self, context: HitContext) -> ThreadRange {
        let hit = ThreadRange::closed(
            self.hit.thread,
            self.hit.first,
            self.hit.last.unwrap_or(self.hit.first),
        );
        let around = context.around(hit);
        let last = around
            .last
            .expect("the context around a closed range ends in a message");
        ThreadRange::closed(around.thread, around.first, last.min(self.thread_length))
    }
}

/// The messages of a thread before and after a hit or a ref that `-B`,
/// `-A` and `-C` add: a hit's `read ref=` recipe on `agent search` and
/// `agent within`, the messages `agent read` and `agent outline` read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HitContext {
    pub before: usize,
    pub after: usize,
}

impl HitContext {
    /// `range` with `before` messages of its thread before it, from the
    /// thread's first message, and `after` messages after it. An open end
    /// stays open; a closed end can pass the thread's last message.
    pub fn around(self, range: ThreadRange) -> ThreadRange {
        ThreadRange {
            thread: range.thread,
            first: range.first.saturating_sub(self.before).max(1),
            last: range.last.map(|last| last.saturating_add(self.after)),
        }
    }
}

impl Default for HitContext {
    /// One message before the hit and one after it.
    fn default() -> Self {
        Self {
            before: 1,
            after: 1,
        }
    }
}

impl HitPlaces {
    /// The places of `ordinals` in `threads`: the ordinals of the session's
    /// own messages and of each thread containing one of `ordinals`, in order.
    pub fn of(threads: &HashMap<Thread, Vec<usize>>, ordinals: &[usize]) -> Self {
        let parent = threads.get(&Thread::Parent).map_or(&[][..], Vec::as_slice);
        let messages = ordinals
            .iter()
            .filter_map(|&ordinal| {
                threads.iter().find_map(|(&thread, messages)| {
                    let index = messages.binary_search(&ordinal).ok()?;
                    let place = HitPlace {
                        thread,
                        number: index + 1,
                        group: group_in(parent, ordinal),
                    };
                    Some((ordinal, place))
                })
            })
            .collect();
        let lengths = threads
            .iter()
            .map(|(&thread, messages)| (thread, messages.len()))
            .collect();
        Self { messages, lengths }
    }

    /// The addresses of a hit on the messages of `hit`, ordinals: the hit
    /// messages' thread range and that thread's length. A hit over several
    /// threads names the groups that contain it, in the session's own
    /// thread. `None` when a hit message is not placed.
    pub fn hit_addresses(&self, hit: MessageRange) -> Option<HitAddresses> {
        let places = (hit.start..=hit.end)
            .map(|ordinal| self.messages.get(&ordinal).copied())
            .collect::<Option<Vec<_>>>()?;
        let (first, last) = (*places.first()?, *places.last()?);
        let (thread, start, end) = if places.iter().all(|place| place.thread == first.thread) {
            (first.thread, first.number, last.number)
        } else {
            (Thread::Parent, first.group, last.group)
        };
        Some(HitAddresses {
            hit: ThreadRange::closed(thread, start, end),
            thread_length: self.lengths.get(&thread).copied().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::diagnostic::AgentErrorKind;
    use crate::agent::refs::parse_messages;
    use crate::agent::test_support::{subagent_text_message, text_message, transcript};
    use crate::agent::transcript::AgentMessageRole;

    /// By ordinal: alpha (s1) writes 1, 3 and 6, beta (s2) 5 and 8; the
    /// session's own messages m1 to m4 are 2, 4, 7 and 9.
    fn addresses() -> MessageAddresses {
        let own = |ordinal| text_message(ordinal, AgentMessageRole::User, "own");
        MessageAddresses::of_transcript(&transcript(
            vec![
                subagent_text_message(1, "alpha", "alpha"),
                own(2),
                subagent_text_message(3, "alpha", "alpha"),
                own(4),
                subagent_text_message(5, "beta", "beta"),
                subagent_text_message(6, "alpha", "alpha"),
                own(7),
                subagent_text_message(8, "beta", "beta"),
                own(9),
            ],
            "session.jsonl",
        ))
    }

    fn resolved(reference: &str) -> Result<ResolvedRange> {
        addresses().resolve(parse_messages(reference).unwrap())
    }

    fn ordinals(start: usize, end: usize, subagent: Option<Handle>) -> ResolvedRange {
        ResolvedRange {
            range: MessageRange { start, end },
            subagent,
            shortened_to: None,
        }
    }

    #[test]
    fn message_prints_its_number_in_its_thread() {
        let addresses = addresses();

        assert_eq!(addresses.message(4), "m2");
        assert_eq!(addresses.message(6), "s1:m3");
        assert_eq!(addresses.range(5, 5), "s2:m1");
        assert_eq!(
            addresses.call(CallAddress {
                ordinal: 6,
                number: 2
            }),
            "s1:m3.2"
        );
        assert_eq!(addresses.calls(8, 2, 4), "s2:m2.2..4");
    }

    #[test]
    fn own_messages_contain_subagent_messages_written_during_them() {
        for (reference, expected) in [
            ("m1..m1", ordinals(1, 3, None)),
            ("m1", ordinals(1, 3, None)),
            ("m2..m2", ordinals(4, 6, None)),
            ("m2..m3", ordinals(4, 8, None)),
            ("m3", ordinals(7, 8, None)),
            ("m4..m4", ordinals(9, 9, None)),
        ] {
            assert_eq!(resolved(reference).unwrap(), expected, "{reference}");
        }
    }

    #[test]
    fn subagent_ref_names_its_messages_alone() {
        for (reference, expected) in [
            ("s1:m2..m3", ordinals(3, 6, Some(1))),
            ("s2:m2", ordinals(8, 8, Some(2))),
            ("s2:m2..m2", ordinals(8, 8, Some(2))),
        ] {
            assert_eq!(resolved(reference).unwrap(), expected, "{reference}");
        }
    }

    #[test]
    fn ref_ending_past_its_threads_end_is_shortened_to_its_last_message() {
        for (reference, expected, last) in [
            ("m3..m9", ordinals(7, 9, None), "m4"),
            ("s1:m2..m9", ordinals(3, 6, Some(1)), "s1:m3"),
        ] {
            let resolved = resolved(reference).unwrap();

            assert_eq!(
                (resolved.range, resolved.subagent),
                (expected.range, expected.subagent),
                "{reference}"
            );
            assert_eq!(
                resolved
                    .shortened_to
                    .map(|last| last.to_string())
                    .as_deref(),
                Some(last),
                "{reference} names its thread's last message"
            );
        }
    }

    #[test]
    fn open_ended_ref_reads_to_its_threads_end_without_being_shortened() {
        for (reference, closed) in [("m3..", "m3..m4"), ("s1:m2..", "s1:m2..m3")] {
            assert_eq!(
                resolved(reference).unwrap(),
                resolved(closed).unwrap(),
                "{reference} reads as {closed}"
            );
        }
    }

    #[test]
    fn ref_starting_past_its_threads_end_names_its_last_message() {
        for (reference, detail) in [
            (
                "m5",
                "message range m5 starts after the last message, m4; sub-agent messages are numbered sN:mK",
            ),
            (
                "m5..m9",
                "message range m5..m9 starts after the last message, m4; sub-agent messages are numbered sN:mK",
            ),
            (
                "s1:m4..",
                "message range s1:m4.. starts after s1's last message, s1:m3",
            ),
        ] {
            let error = resolved(reference).unwrap_err();

            assert!(
                error.to_string().contains(detail),
                "{reference} names its thread's last message: {error}"
            );
            assert!(
                matches!(
                    &error,
                    crate::error::AppError::Agent(AgentError {
                        kind: AgentErrorKind::OutOfRange,
                        ..
                    })
                ),
                "{reference} is out of range: {error}"
            );
        }
    }

    #[test]
    fn own_ref_in_session_without_own_messages_reports_it_has_none() {
        let addresses = MessageAddresses::of_transcript(&transcript(
            vec![subagent_text_message(1, "alpha", "alpha")],
            "session.jsonl",
        ));

        let error = addresses
            .resolve(parse_messages("m1").unwrap())
            .unwrap_err();

        assert!(
            error.to_string().contains(
                "message range m1: the session does not have messages of its own; sub-agent messages are numbered sN:mK"
            ),
            "the detail names the missing messages, not m0: {error}"
        );
    }

    #[test]
    fn ref_of_unknown_handle_is_not_found() {
        let error = resolved("s9:m1").unwrap_err();

        assert!(
            matches!(
                &error,
                crate::error::AppError::Agent(AgentError {
                    kind: AgentErrorKind::NotFound,
                    ..
                })
            ),
            "s9 is not found: {error}"
        );
    }

    /// The `hit=` and `read ref=` addresses, as printed, of a hit on the
    /// messages from ordinal `start` to `end`, the recipe covering the
    /// default context.
    fn hit(places: &HitPlaces, start: usize, end: usize) -> (String, String) {
        hit_with_context(places, start, end, HitContext::default())
    }

    /// [`hit`], the recipe covering `context`.
    fn hit_with_context(
        places: &HitPlaces,
        start: usize,
        end: usize,
        context: HitContext,
    ) -> (String, String) {
        let addresses = places
            .hit_addresses(MessageRange { start, end })
            .expect("the hit messages are placed");
        (
            addresses.hit.to_string(),
            addresses.recipe_range(context).to_string(),
        )
    }

    /// Alpha (s1) writes ordinals 1, 3 and 6: its m1 is its thread's first
    /// message and its m3 its last; m1 and m4 are the session's own first
    /// and last.
    #[test]
    fn hit_context_counts_messages_of_its_thread_shortened_at_its_ends() {
        let places = addresses().hit_places(1..=9);
        let context = |before, after| HitContext { before, after };

        for (start, end, context, hit_address, read) in [
            (1, 1, context(2, 1), "s1:m1", "s1:m1..m2"),
            (6, 6, context(1, 2), "s1:m3", "s1:m2..m3"),
            (6, 6, context(0, 0), "s1:m3", "s1:m3"),
            (2, 2, context(3, 2), "m1", "m1..m3"),
            (9, 9, context(2, 3), "m4", "m2..m4"),
            (4, 4, context(0, 9), "m2", "m2..m4"),
            (5, 7, context(1, 0), "m2..m3", "m1..m3"),
        ] {
            assert_eq!(
                hit_with_context(&places, start, end, context),
                (hit_address.to_owned(), read.to_owned()),
                "the hit on ordinals {start}..{end} with {context:?}"
            );
        }
    }

    #[test]
    fn context_around_ref_keeps_its_thread_and_an_open_end() {
        let context = HitContext {
            before: 2,
            after: 3,
        };

        for (reference, around) in [
            ("m5", "m3..m8"),
            ("m1..m2", "m1..m5"),
            ("s3:m4..m6", "s3:m2..m9"),
            ("m7..", "m5.."),
        ] {
            assert_eq!(
                context
                    .around(parse_messages(reference).unwrap())
                    .to_string(),
                around,
                "{reference}"
            );
        }
    }

    #[test]
    fn hit_reads_message_before_and_after_it_in_its_thread() {
        let addresses = addresses();
        let places = addresses.hit_places(1..=9);

        for (start, end, hit_address, read) in [
            (3, 3, "s1:m2", "s1:m1..m3"),
            (6, 6, "s1:m3", "s1:m2..m3"),
            (2, 2, "m1", "m1..m2"),
            (7, 7, "m3", "m2..m4"),
            (9, 9, "m4", "m3..m4"),
        ] {
            assert_eq!(
                hit(&places, start, end),
                (hit_address.to_owned(), read.to_owned()),
                "the hit on ordinals {start}..{end}"
            );
        }
    }

    #[test]
    fn hit_over_two_threads_names_groups_containing_it() {
        let places = addresses().hit_places(1..=9);

        assert_eq!(
            hit(&places, 5, 7),
            ("m2..m3".to_owned(), "m1..m4".to_owned()),
            "beta's m1, alpha's m3 and m3 are in the groups of m2 and m3"
        );
    }

    #[test]
    fn hit_on_message_left_out_of_places_doesnt_get_addresses() {
        let places = addresses().hit_places([3, 5, 7]);

        assert_eq!(
            places.hit_addresses(MessageRange::single(4)),
            None,
            "a message left out of the places doesn't get an address"
        );
    }

    #[test]
    fn refs_name_whole_groups_as_one_range() {
        let addresses = addresses();

        assert_eq!(addresses.refs(&[(1, 9)]), ["m1..m4"]);
        assert_eq!(addresses.refs(&[(4, 6)]), ["m2"]);
        assert_eq!(addresses.refs(&[(7, 9)]), ["m3..m4"]);
        assert_eq!(addresses.refs(&[(9, 9)]), ["m4"]);
    }

    #[test]
    fn refs_name_group_of_each_own_message_named_without_its_subagent_messages() {
        let addresses = addresses();

        assert_eq!(
            addresses.refs(&[(7, 7)]),
            ["m3"],
            "m3 without beta's m2 is m3's group"
        );
        assert_eq!(
            addresses.refs(&[(3, 7)]),
            ["s1:m2", "m2..m3"],
            "alpha's m2 is in m1's group, and m3 without beta's m2 is m3's group"
        );
    }

    #[test]
    fn refs_name_subagent_messages_of_group_without_its_own_message_by_thread() {
        assert_eq!(
            addresses().refs(&[(5, 6), (8, 8)]),
            ["s2:m1..m2", "s1:m3"],
            "beta's m1 and m2 are one run beside alpha's m3"
        );
    }
}
