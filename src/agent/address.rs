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
        let place = self.place(ordinal);
        ThreadRange::one(place.thread, place.number).to_string()
    }

    /// `m7`, `m7..m9` or `s3:m2..m4`: messages `first` to `last` of one
    /// thread, printed as [`Self::closed_range`] prints two.
    pub fn range(&self, first: usize, last: usize) -> String {
        if first == last {
            self.message(first)
        } else {
            self.closed_range(first, last)
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

    /// `m7..m9` or `s3:m2..m4`, and `m7..m7` for one message: the form of an
    /// outline's segment. Messages of two threads print as the groups that
    /// hold them, `m7..m9`.
    pub fn closed_range(&self, first: usize, last: usize) -> String {
        let (first_place, last_place) = (self.place(first), self.place(last));
        if first_place.thread == last_place.thread {
            ThreadRange::closed(first_place.thread, first_place.number, last_place.number)
                .to_string()
        } else {
            self.groups(first, last)
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

    /// The refs, after the conversation's, that read exactly the messages of
    /// `runs`, consecutive ordinals, in order:
    /// - every message of consecutive groups as one range, `m119..m125`, or
    ///   `m7` for a group of one message;
    /// - of a group with messages left out, its own message as `m7`, and
    ///   each sub-agent's consecutive messages as one range, `s3:m4..m9`.
    ///
    /// The refs come in the order of their first messages.
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

    /// The ranges of the refs that read exactly the messages at the sorted
    /// `indices`, in the order of their first messages.
    fn ref_ranges(&self, indices: &[usize]) -> Vec<RefRange> {
        let mut ranges = RefRanges::default();
        for group in indices.chunk_by(|&left, &right| self.group_at(left) == self.group_at(right)) {
            if self.is_whole_group(group) {
                ranges.add_group(self.ordinals[group[0]], self.group_at(group[0]));
            } else {
                for &index in group {
                    ranges.add_message(self.ordinals[index], self.places[index]);
                }
            }
        }
        ranges.in_order()
    }

    /// True when `indices`, of messages of one group, hold every message of
    /// that group.
    fn is_whole_group(&self, indices: &[usize]) -> bool {
        !self.thread_messages(Thread::Parent).is_empty()
            && indices.len() == self.group_len(self.group_at(indices[0]))
    }

    /// `m7`, `m7..m9` or `s3:m4..m9`: the ref reading `range`.
    fn ref_address(&self, range: RefRange) -> ThreadRange {
        match range {
            RefRange::Groups(first, last) if first == last && self.group_len(first) == 1 => {
                ThreadRange::one(Thread::Parent, first)
            }
            RefRange::Groups(first, last) => ThreadRange::closed(Thread::Parent, first, last),
            RefRange::Run(thread, first, last) if first == last => ThreadRange::one(thread, first),
            RefRange::Run(thread, first, last) => ThreadRange::closed(thread, first, last),
        }
    }

    /// The messages `reference` names, or the error naming what it misses:
    /// - `mN` is the session's own message N;
    /// - `mA..mB` is messages A to B of the session's own with the
    ///   sub-agent messages written after mA, from the session's start when
    ///   A is 1, and before m(B+1);
    /// - `sN:mA..mB` is sub-agent N's messages A to B alone.
    pub fn resolve(&self, reference: ThreadRange) -> Result<ResolvedRange> {
        let MessageRange { start, end } = reference.range;
        let messages = self.thread_messages(reference.thread);
        if let Thread::Subagent(handle) = reference.thread
            && (handle == 0 || handle > self.handles)
        {
            return Err(AgentError::unknown_subagent(handle).into());
        }
        if end > messages.len() {
            return Err(AgentError::out_of_range(None, self.past_end(reference)).into());
        }
        let range = match reference.thread {
            Thread::Parent if reference.is_range => MessageRange {
                start: if start == 1 {
                    self.ordinals[0]
                } else {
                    messages[start - 1]
                },
                end: match messages.get(end) {
                    Some(&next) => self.ordinals[self.index(next) - 1],
                    None => *self.ordinals.last().expect("the range ends in a message"),
                },
            },
            _ => MessageRange {
                start: messages[start - 1],
                end: messages[end - 1],
            },
        };
        Ok(ResolvedRange {
            range,
            subagent: match reference.thread {
                Thread::Parent => None,
                Thread::Subagent(handle) => Some(handle),
            },
        })
    }

    /// The detail of the error for `reference` ending past its thread's last
    /// message.
    fn past_end(&self, reference: ThreadRange) -> String {
        let length = self.thread_messages(reference.thread).len();
        let last = ThreadRange::one(reference.thread, length);
        match reference.thread {
            Thread::Parent => {
                let has_subagent_messages =
                    self.threads.keys().any(|&thread| thread != Thread::Parent);
                let numbering = if has_subagent_messages {
                    "; sub-agent messages are numbered sN:mK"
                } else {
                    ""
                };
                format!("message range {reference} exceeds transcript length {last}{numbering}")
            }
            Thread::Subagent(handle) if length == 0 => {
                format!(
                    "message range {reference} exceeds s{handle}'s length: it has not written a message"
                )
            }
            Thread::Subagent(handle) => {
                format!("message range {reference} exceeds s{handle}'s length {last}")
            }
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

    /// The indices, in `ordinals`, of the group holding message `ordinal`.
    fn group_indices(&self, ordinal: usize) -> std::ops::RangeInclusive<usize> {
        let parent = self.thread_messages(Thread::Parent);
        if parent.is_empty() {
            let index = self.index(ordinal);
            return index..=index;
        }
        let group = self.group_of(ordinal);
        let start = if group == 1 {
            0
        } else {
            self.index(parent[group - 1])
        };
        let end = match parent.get(group) {
            Some(&next) => self.index(next) - 1,
            None => self.ordinals.len() - 1,
        };
        start..=end
    }

    /// The number of messages in the group of the session's own message
    /// `number`.
    fn group_len(&self, number: usize) -> usize {
        let parent = self.thread_messages(Thread::Parent);
        self.group_indices(parent[number - 1]).count()
    }
}

/// The messages one of [`MessageAddresses::refs`]'s refs reads, by number.
#[derive(Clone, Copy)]
enum RefRange {
    /// Every message of the groups of the session's own messages `first` to
    /// `last`.
    Groups(usize, usize),
    /// Messages `first` to `last` of one thread alone.
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

    /// Add message `ordinal`, at `place` in a group with messages left out:
    /// the session's own as a range of its own, a sub-agent's to its thread's
    /// run when it follows that run's last message.
    fn add_message(&mut self, ordinal: usize, place: Place) {
        let Place { thread, number } = place;
        if thread == Thread::Parent {
            self.ranges
                .push((ordinal, RefRange::Run(thread, number, number)));
            return;
        }
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

/// A search hit's `focus=` and the range its `read ref=` recipe reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HitAddresses {
    pub focus: ThreadRange,
    pub read: ThreadRange,
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

    /// The addresses of a hit on the messages of `focus`, ordinals: the
    /// focus messages' thread range, and the read of the message before and
    /// after it in that thread. A focus over several threads names the groups
    /// that hold it. `None` when a focus message is not placed.
    pub fn hit_addresses(&self, focus: MessageRange) -> Option<HitAddresses> {
        let places = (focus.start..=focus.end)
            .map(|ordinal| self.messages.get(&ordinal).copied())
            .collect::<Option<Vec<_>>>()?;
        let (first, last) = (*places.first()?, *places.last()?);
        let (thread, start, end) = if places.iter().all(|place| place.thread == first.thread) {
            (first.thread, first.number, last.number)
        } else {
            (Thread::Parent, first.group, last.group)
        };
        let length = self.lengths.get(&thread).copied().unwrap_or(0);
        Some(HitAddresses {
            focus: ThreadRange::closed(thread, start, end),
            read: ThreadRange::closed(
                thread,
                start.saturating_sub(1).max(1),
                (end + 1).min(length),
            ),
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
    fn range_of_own_messages_holds_subagent_messages_written_during_it() {
        for (reference, expected) in [
            ("m1..m1", ordinals(1, 3, None)),
            ("m2..m2", ordinals(4, 6, None)),
            ("m2..m3", ordinals(4, 8, None)),
            ("m4..m4", ordinals(9, 9, None)),
        ] {
            assert_eq!(resolved(reference).unwrap(), expected, "{reference}");
        }
    }

    #[test]
    fn single_own_message_and_subagent_range_name_their_messages_alone() {
        for (reference, expected) in [
            ("m1", ordinals(2, 2, None)),
            ("m3", ordinals(7, 7, None)),
            ("s1:m2..m3", ordinals(3, 6, Some(1))),
            ("s2:m2", ordinals(8, 8, Some(2))),
        ] {
            assert_eq!(resolved(reference).unwrap(), expected, "{reference}");
        }
    }

    #[test]
    fn ref_past_its_threads_end_names_threads_length() {
        for (reference, detail) in [
            (
                "m5",
                "message range m5 exceeds transcript length m4; sub-agent messages are numbered sN:mK",
            ),
            (
                "s1:m3..m9",
                "message range s1:m3..m9 exceeds s1's length s1:m3",
            ),
        ] {
            let error = resolved(reference).unwrap_err();

            assert!(
                error.to_string().contains(detail),
                "{reference} names its thread's length: {error}"
            );
        }
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

    /// The `focus=` and `read ref=` addresses, as printed, of a hit on the
    /// messages from ordinal `start` to `end`.
    fn hit(places: &HitPlaces, start: usize, end: usize) -> (String, String) {
        let addresses = places
            .hit_addresses(MessageRange { start, end })
            .expect("the focus messages are placed");
        (addresses.focus.to_string(), addresses.read.to_string())
    }

    #[test]
    fn hit_reads_message_before_and_after_its_focus_in_its_thread() {
        let addresses = addresses();
        let places = addresses.hit_places(1..=9);

        for (start, end, focus, read) in [
            (3, 3, "s1:m2..m2", "s1:m1..m3"),
            (6, 6, "s1:m3..m3", "s1:m2..m3"),
            (2, 2, "m1..m1", "m1..m2"),
            (7, 7, "m3..m3", "m2..m4"),
            (9, 9, "m4..m4", "m3..m4"),
        ] {
            assert_eq!(
                hit(&places, start, end),
                (focus.to_owned(), read.to_owned()),
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
        assert_eq!(addresses.refs(&[(4, 6)]), ["m2..m2"]);
        assert_eq!(addresses.refs(&[(7, 9)]), ["m3..m4"]);
        assert_eq!(addresses.refs(&[(9, 9)]), ["m4"]);
    }

    #[test]
    fn refs_name_messages_of_group_partly_named_by_thread() {
        let addresses = addresses();

        assert_eq!(addresses.refs(&[(7, 7)]), ["m3"]);
        assert_eq!(
            addresses.refs(&[(5, 6), (8, 8)]),
            ["s2:m1..m2", "s1:m3"],
            "beta's m1 and m2 are one run beside alpha's m3"
        );
        assert_eq!(
            addresses.refs(&[(3, 7)]),
            ["s1:m2", "m2..m2", "m3"],
            "alpha's m2 is in m1's group, and m3 without beta's m2"
        );
    }
}
