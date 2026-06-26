use crate::error::{AppError, Result};
use crate::history::Conversation;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const REF_NAMESPACE: &str = "agent-v1";
const CANONICAL_DIGEST_HEX_LEN: usize = 12;
const MIN_PREFIX_HEX_LEN: usize = 8;
const DIGEST_HEX_LEN: usize = 32;
const UUID_HEX_LEN: usize = 32;
const UUID_LEN: usize = 36;
const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentConversationRef {
    uuid: String,
    digest_hex: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConversationRefInput {
    digest_prefix_hex: String,
}

impl AgentConversationRef {
    pub fn from_parts(project_dir_name: &str, session_filename: &str) -> Self {
        let digest = digest_parts([REF_NAMESPACE, project_dir_name, session_filename]);
        Self {
            uuid: session_uuid(session_filename)
                .filter(|uuid| is_uuid(uuid))
                .unwrap_or("none")
                .to_ascii_lowercase(),
            digest_hex: format!("{digest:032x}"),
        }
    }

    pub fn canonical(&self) -> String {
        format!("ch_{}", &self.digest_hex[..CANONICAL_DIGEST_HEX_LEN])
    }

    pub fn full_ref(&self) -> String {
        format!("ch_{}", self.digest_hex)
    }

    pub fn uuid(&self) -> String {
        self.uuid.clone()
    }

    fn matches_input(&self, input: &ConversationRefInput) -> bool {
        self.digest_hex.starts_with(&input.digest_prefix_hex)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentConversationKey {
    pub project_dir_name: String,
    pub session_filename: String,
    pub path: PathBuf,
}

impl AgentConversationKey {
    pub fn new(
        project_dir_name: impl Into<String>,
        session_filename: impl Into<String>,
        path: PathBuf,
    ) -> Self {
        Self {
            project_dir_name: project_dir_name.into(),
            session_filename: session_filename.into(),
            path,
        }
    }

    pub fn from_conversation(conversation: &Conversation) -> Result<Self> {
        let project_dir_name = conversation
            .path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                AppError::ConfigError("conversation path has no project directory".to_string())
            })?
            .to_string();
        let session_filename = conversation
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                AppError::ConfigError("conversation path has no session filename".to_string())
            })?
            .to_string();
        Ok(Self::new(
            project_dir_name,
            session_filename,
            conversation.path.clone(),
        ))
    }

    pub fn conversation_ref(&self) -> AgentConversationRef {
        AgentConversationRef::from_parts(&self.project_dir_name, &self.session_filename)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConversation {
    pub key: AgentConversationKey,
    pub reference: AgentConversationRef,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageRange {
    pub start: usize,
    pub end: usize,
}

impl MessageRange {
    pub fn single(message: usize) -> Self {
        Self {
            start: message,
            end: message,
        }
    }

    pub fn contains(&self, other: &MessageRange) -> bool {
        self.start <= other.start && self.end >= other.end
    }

    pub fn union(&self, other: &MessageRange) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRef {
    pub conversation: String,
    pub range: Option<MessageRange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusRef {
    pub conversation: Option<String>,
    pub range: MessageRange,
}

pub fn parse_read_ref(input: &str) -> Result<ReadRef> {
    let (conversation, range) = match input.split_once(':') {
        Some((conversation, range)) => (conversation, Some(parse_message_range(range)?)),
        None => (input, None),
    };
    validate_conversation_ref(conversation)?;
    Ok(ReadRef {
        conversation: conversation.to_string(),
        range,
    })
}

pub fn parse_focus_ref(input: &str) -> Result<FocusRef> {
    if let Some((conversation, range)) = input.split_once(':') {
        validate_conversation_ref(conversation)?;
        Ok(FocusRef {
            conversation: Some(conversation.to_string()),
            range: parse_message_range(range)?,
        })
    } else {
        Ok(FocusRef {
            conversation: None,
            range: parse_message_range(input)?,
        })
    }
}

pub fn validate_resolved_focus_in_ranges(
    read_refs: &[(ReadRef, ResolvedConversation)],
    focus: &FocusRef,
    focus_conversation: Option<&ResolvedConversation>,
) -> Result<()> {
    let target_ref = if let Some(focus_conversation) = focus_conversation {
        focus_conversation.reference.full_ref()
    } else {
        let Some((_, first)) = read_refs.first() else {
            return Err(AppError::ConfigError(
                "focus requires at least one read ref".to_string(),
            ));
        };
        let first_ref = first.reference.full_ref();
        if read_refs
            .iter()
            .any(|(_, resolved)| resolved.reference.full_ref() != first_ref)
        {
            return Err(AppError::ConfigError(
                "bare focus is ambiguous for multiple conversations; use ch_...:mN".to_string(),
            ));
        }
        first_ref
    };

    let contained = read_refs.iter().any(|(read_ref, resolved)| {
        resolved.reference.full_ref() == target_ref
            && read_ref
                .range
                .unwrap_or(MessageRange {
                    start: 1,
                    end: usize::MAX,
                })
                .contains(&focus.range)
    });

    if contained {
        Ok(())
    } else {
        Err(AppError::ConfigError(format!(
            "focus m{}..m{} is outside the requested read range",
            focus.range.start, focus.range.end
        )))
    }
}

pub fn resolve_conversation_ref(
    keys: &[AgentConversationKey],
    reference: &str,
) -> Result<ResolvedConversation> {
    let input = validate_conversation_ref(reference)?;
    let matches: Vec<ResolvedConversation> = keys
        .iter()
        .filter_map(|key| {
            let conversation_ref = key.conversation_ref();
            conversation_ref
                .matches_input(&input)
                .then(|| ResolvedConversation {
                    key: key.clone(),
                    reference: conversation_ref,
                })
        })
        .collect();

    finish_resolution(reference, matches)
}

fn finish_resolution(
    reference: &str,
    matches: Vec<ResolvedConversation>,
) -> Result<ResolvedConversation> {
    match matches.as_slice() {
        [resolved] => Ok(resolved.clone()),
        [] => Err(AppError::SessionNotFound(reference.to_string())),
        _ if matches
            .iter()
            .all(|resolved| resolved.reference.full_ref() == matches[0].reference.full_ref()) =>
        {
            Ok(matches[0].clone())
        }
        _ => {
            let candidates = matches
                .iter()
                .map(|m| format!("{} {}", m.reference.canonical(), m.key.session_filename))
                .collect::<Vec<_>>()
                .join("\n  ");
            Err(AppError::ConfigError(format!(
                "ambiguous conversation ref {reference}; candidates:\n  {candidates}"
            )))
        }
    }
}

pub fn conversation_keys_from_conversations(
    conversations: &[Conversation],
) -> Result<Vec<AgentConversationKey>> {
    conversations
        .iter()
        .map(AgentConversationKey::from_conversation)
        .collect()
}

fn validate_conversation_ref(reference: &str) -> Result<ConversationRefInput> {
    if let Some(hex) = reference.strip_prefix("ch_") {
        if hex.len() < MIN_PREFIX_HEX_LEN {
            return Err(AppError::ConfigError(format!(
                "conversation ref {reference} is too short; use at least {MIN_PREFIX_HEX_LEN} hex characters"
            )));
        }
        if hex.len() > DIGEST_HEX_LEN {
            return Err(AppError::ConfigError(format!(
                "conversation ref {reference} is too long; use at most {DIGEST_HEX_LEN} hex characters"
            )));
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(AppError::ConfigError(format!(
                "invalid conversation ref {reference}; expected hexadecimal digits"
            )));
        }
        return Ok(ConversationRefInput {
            digest_prefix_hex: hex.to_ascii_lowercase(),
        });
    }

    Err(AppError::ConfigError(format!(
        "invalid conversation ref {reference}; use ref=ch_... from agent search output"
    )))
}

fn session_uuid(session_filename: &str) -> Option<&str> {
    session_filename.strip_suffix(".jsonl")
}

fn is_uuid(value: &str) -> bool {
    value.len() == UUID_LEN
        && value.chars().enumerate().all(|(index, c)| match index {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
        && value.chars().filter(|c| c.is_ascii_hexdigit()).count() == UUID_HEX_LEN
}

fn parse_message_range(input: &str) -> Result<MessageRange> {
    if input.contains("...") {
        return Err(AppError::ConfigError(format!(
            "invalid message range {input}; use mN or mN..mM"
        )));
    }
    if let Some((start, end)) = input.split_once("..") {
        if start.is_empty() || end.is_empty() {
            return Err(AppError::ConfigError(format!(
                "open-ended message range {input} is not supported"
            )));
        }
        let start = parse_message_number(start)?;
        let end = parse_message_number(end)?;
        if start > end {
            return Err(AppError::ConfigError(format!(
                "invalid message range {input}; start must be before end"
            )));
        }
        Ok(MessageRange { start, end })
    } else {
        Ok(MessageRange::single(parse_message_number(input)?))
    }
}

fn parse_message_number(input: &str) -> Result<usize> {
    let Some(number) = input.strip_prefix('m') else {
        return Err(AppError::ConfigError(format!(
            "invalid message ref {input}; expected mN"
        )));
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::ConfigError(format!(
            "invalid message ref {input}; expected mN"
        )));
    }
    let parsed = number
        .parse::<usize>()
        .map_err(|_| AppError::ConfigError(format!("invalid message ref {input}; expected mN")))?;
    if parsed == 0 {
        return Err(AppError::ConfigError(
            "message refs are 1-based".to_string(),
        ));
    }
    Ok(parsed)
}

fn digest_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> u128 {
    let mut hash = FNV_OFFSET;
    for part in parts {
        for byte in (part.len() as u64).to_le_bytes() {
            hash = (hash ^ byte as u128).wrapping_mul(FNV_PRIME);
        }
        for byte in part.as_bytes() {
            hash = (hash ^ *byte as u128).wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(project: &str, filename: &str) -> AgentConversationKey {
        AgentConversationKey::new(
            project,
            filename,
            PathBuf::from(format!("/{project}/{filename}")),
        )
    }

    #[test]
    fn canonical_ref_is_internal_digest_ref() {
        let reference = AgentConversationRef::from_parts(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
        );
        assert_eq!(reference.canonical(), "ch_98bd40760a01");
        assert_eq!(reference.full_ref().len(), "ch_".len() + DIGEST_HEX_LEN);
        assert_eq!(reference.uuid(), "12345678-1234-4234-9234-123456789abc");
        assert_eq!(
            reference,
            AgentConversationRef::from_parts(
                "project-a",
                "12345678-1234-4234-9234-123456789abc.jsonl"
            )
        );
    }

    #[test]
    fn same_uuid_across_projects_has_distinct_internal_refs() {
        let first =
            key("project-a", "12345678-1234-4234-9234-123456789abc.jsonl").conversation_ref();
        let second =
            key("project-b", "12345678-1234-4234-9234-123456789abc.jsonl").conversation_ref();
        assert_ne!(first.full_ref(), second.full_ref());
        assert_eq!(first.uuid(), second.uuid());
    }

    #[test]
    fn rejects_uuid_refs_for_command_args() {
        let keys = vec![key(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
        )];
        let err =
            resolve_conversation_ref(&keys, "12345678-1234-4234-9234-123456789abc").unwrap_err();
        assert!(err.to_string().contains("use ref=ch_..."));
    }

    #[test]
    fn resolves_internal_digest_prefix() {
        let keys = vec![
            key("project-a", "12345678-1234-4234-9234-123456789abc.jsonl"),
            key("project-b", "87654321-1234-4234-9234-123456789abc.jsonl"),
        ];
        let internal = AgentConversationRef::from_parts(
            "project-a",
            "12345678-1234-4234-9234-123456789abc.jsonl",
        )
        .digest_hex;
        let resolved = resolve_conversation_ref(&keys, &format!("ch_{}", &internal[..8])).unwrap();
        assert_eq!(
            resolved.key.session_filename,
            "12345678-1234-4234-9234-123456789abc.jsonl"
        );
    }

    #[test]
    fn ambiguous_prefix_reports_canonical_candidates() {
        let first = key("project-a", "12345678-1234-4234-9234-123456789abc.jsonl");
        let second = key("project-c", "12345678-ffff-4234-9234-123456789abc.jsonl");
        let first_ref = first.conversation_ref();
        let second_ref = second.conversation_ref();
        let err = finish_resolution(
            &first_ref.canonical(),
            vec![
                ResolvedConversation {
                    key: first,
                    reference: first_ref.clone(),
                },
                ResolvedConversation {
                    key: second,
                    reference: second_ref,
                },
            ],
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("ambiguous conversation ref"));
        assert!(message.contains(&first_ref.canonical()));
        assert!(message.contains("12345678-ffff-4234-9234-123456789abc.jsonl"));
    }

    #[test]
    fn read_ref_rejects_invalid_forms() {
        assert!(
            parse_read_ref("ch_1234567")
                .unwrap_err()
                .to_string()
                .contains("too short")
        );
        assert!(
            parse_read_ref("ch_12345678:m1..")
                .unwrap_err()
                .to_string()
                .contains("open-ended")
        );
        assert!(
            parse_read_ref("ch_12345678:m..m2")
                .unwrap_err()
                .to_string()
                .contains("invalid message ref")
        );
        assert!(
            parse_read_ref("ch_12345678:m3..m2")
                .unwrap_err()
                .to_string()
                .contains("start must be before end")
        );
        assert!(
            parse_read_ref("ch_12345678:1")
                .unwrap_err()
                .to_string()
                .contains("expected mN")
        );
    }

    #[test]
    fn validates_focus_inside_read_ranges() {
        let reads = vec![parse_read_ref("ch_12345678:m2..m5").unwrap()];
        let resolved = ResolvedConversation {
            key: key("project-a", "one.jsonl"),
            reference: AgentConversationRef::from_parts("project-a", "one.jsonl"),
        };
        let resolved_reads = vec![(reads[0].clone(), resolved)];
        validate_resolved_focus_in_ranges(&resolved_reads, &parse_focus_ref("m3").unwrap(), None)
            .unwrap();
        let err = validate_resolved_focus_in_ranges(
            &resolved_reads,
            &parse_focus_ref("m6").unwrap(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside"));
    }

    #[test]
    fn bare_focus_is_rejected_for_multiple_conversations() {
        let reads = vec![
            parse_read_ref("ch_12345678:m1..m5").unwrap(),
            parse_read_ref("ch_87654321:m1..m5").unwrap(),
        ];
        let first = ResolvedConversation {
            key: key("project-a", "one.jsonl"),
            reference: AgentConversationRef::from_parts("project-a", "one.jsonl"),
        };
        let second = ResolvedConversation {
            key: key("project-b", "two.jsonl"),
            reference: AgentConversationRef::from_parts("project-b", "two.jsonl"),
        };
        let resolved_reads = vec![(reads[0].clone(), first), (reads[1].clone(), second)];
        let err = validate_resolved_focus_in_ranges(
            &resolved_reads,
            &parse_focus_ref("m2").unwrap(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bare focus is ambiguous"));
    }
}
