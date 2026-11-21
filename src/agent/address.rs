//! The addresses `agent read` and `agent outline` print for a transcript's
//! messages and calls, and the messages a requested range names. Records,
//! links, legends and recipes print every address through
//! [`MessageAddresses`], so a ref copied from any of them names the same
//! message.

use crate::agent::call_links::CallAddress;
use crate::agent::diagnostic::AgentError;
use crate::agent::refs::MessageRange;
use crate::agent::transcript::AgentTranscript;
use crate::error::Result;

/// The addresses of one transcript's messages: each message's number in the
/// session, `m7`.
#[derive(Debug)]
pub struct MessageAddresses {
    /// The number of the transcript's last message.
    last: usize,
}

impl MessageAddresses {
    pub fn of(transcript: &AgentTranscript) -> Self {
        Self {
            last: transcript.messages.len(),
        }
    }

    /// `m7`: message `ordinal`.
    pub fn message(&self, ordinal: usize) -> String {
        format!("m{ordinal}")
    }

    /// `m7`, or `m7..m9`: messages `first` to `last`.
    pub fn range(&self, first: usize, last: usize) -> String {
        if first == last {
            self.message(first)
        } else {
            format!("{}..m{last}", self.message(first))
        }
    }

    /// `m7.2`: the call at `call`.
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

    /// The messages `range` names. Fails as `out-of-range` when it ends past
    /// the transcript's last message.
    pub fn messages_in(&self, range: MessageRange) -> Result<MessageRange> {
        if range.end > self.last {
            return Err(AgentError::out_of_range(
                None,
                format!(
                    "message range {} exceeds transcript length {}",
                    self.closed_range(range.start, range.end),
                    self.message(self.last)
                ),
            )
            .into());
        }
        Ok(range)
    }

    /// `m7..m9`, and `m7..m7` for one message: the form of an outline's
    /// segment, a `focus=` and a requested range.
    pub fn closed_range(&self, first: usize, last: usize) -> String {
        format!("{}..{}", self.message(first), self.message(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{text_message, transcript};
    use crate::agent::transcript::AgentMessageRole;

    fn addresses(messages: usize) -> MessageAddresses {
        MessageAddresses::of(&transcript(
            (1..=messages)
                .map(|ordinal| text_message(ordinal, AgentMessageRole::User, "text"))
                .collect(),
            "session.jsonl",
        ))
    }

    #[test]
    fn single_message_range_prints_as_message() {
        assert_eq!(addresses(9).range(7, 7), "m7");
        assert_eq!(addresses(9).range(7, 9), "m7..m9");
    }

    #[test]
    fn consecutive_calls_print_as_one_range() {
        assert_eq!(addresses(9).calls(7, 2, 2), "m7.2");
        assert_eq!(addresses(9).calls(7, 2, 4), "m7.2..4");
    }

    #[test]
    fn range_past_last_message_is_out_of_range() {
        let error = addresses(9)
            .messages_in(MessageRange { start: 8, end: 10 })
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("message range m8..m10 exceeds transcript length m9"),
            "the error names the range and the last message: {error}"
        );
    }
}
