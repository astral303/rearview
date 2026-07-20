use crate::agent::diagnostic::{AgentError, AgentErrorKind};
use crate::agent::metadata::CURSOR_VERSION;
use crate::error::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContinuationCursor {
    pub version: u16,
    pub family: String,
    pub position: usize,
    pub fingerprint: String,
}

impl ContinuationCursor {
    pub fn new(family: &str, position: usize, fingerprint: String) -> Self {
        Self {
            version: CURSOR_VERSION,
            family: family.to_string(),
            position,
            fingerprint,
        }
    }

    pub fn encode(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("cursor serialization is infallible");
        format!("cu_{CURSOR_VERSION}_{}", encode_hex(&bytes))
    }

    pub fn decode(value: &str) -> Result<Self> {
        let prefix = format!("cu_{CURSOR_VERSION}_");
        let encoded = value.strip_prefix(&prefix).ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::InvalidCursor,
                Some(value),
                "cursor has an unsupported prefix or version",
            )
        })?;
        let bytes = decode_hex(encoded).ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::InvalidCursor,
                Some(value),
                "cursor payload is not valid hexadecimal",
            )
        })?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidCursor,
                Some(value),
                "cursor payload is malformed",
            )
        })?;
        if cursor.version != CURSOR_VERSION {
            return Err(AgentError::new(
                AgentErrorKind::InvalidCursor,
                Some(value),
                "cursor payload version is unsupported",
            )
            .into());
        }
        Ok(cursor)
    }

    pub fn validate(&self, family: &str, fingerprint: &str, len: usize) -> Result<()> {
        if self.family != family || self.fingerprint != fingerprint || self.position > len {
            return Err(AgentError::new(
                AgentErrorKind::StaleCursor,
                None,
                "cursor does not match the current command result revision",
            )
            .into());
        }
        Ok(())
    }
}

pub fn fingerprint(parts: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.as_ref().as_bytes().iter().copied().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_opaque_payload() {
        let cursor = ContinuationCursor::new("agent-search", 4, "abcdef".to_string());
        assert_eq!(
            ContinuationCursor::decode(&cursor.encode()).unwrap(),
            cursor
        );
    }

    #[test]
    fn fingerprint_is_order_sensitive_and_deterministic() {
        assert_eq!(fingerprint(["a", "b"]), fingerprint(["a", "b"]));
        assert_ne!(fingerprint(["a", "b"]), fingerprint(["b", "a"]));
    }
}
