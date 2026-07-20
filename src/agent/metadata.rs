use crate::agent::protocol::escape_atom;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SEARCH_VERSION: u16 = 5;
pub const WITHIN_VERSION: u16 = 5;
pub const READ_VERSION: u16 = 4;
pub const OUTLINE_VERSION: u16 = 4;
pub const CAPABILITIES_VERSION: u16 = 1;
pub const WARNING_VERSION: u16 = 1;
pub const ERROR_VERSION: u16 = 1;
pub const CURSOR_VERSION: u16 = 1;
pub const JSONL_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AgentOutputFormat {
    #[default]
    Compact,
    Jsonl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolFamily {
    Search,
    Within,
    Read,
    Outline,
}

impl ProtocolFamily {
    pub fn name(self) -> &'static str {
        match self {
            Self::Search => "agent-search",
            Self::Within => "agent-within",
            Self::Read => "agent-read",
            Self::Outline => "agent-outline",
        }
    }

    pub fn version(self) -> u16 {
        match self {
            Self::Search => SEARCH_VERSION,
            Self::Within => WITHIN_VERSION,
            Self::Read => READ_VERSION,
            Self::Outline => OUTLINE_VERSION,
        }
    }
}

#[derive(Debug, Serialize)]
struct CapabilityCommand {
    command: &'static str,
    family: &'static str,
    version: u16,
    formats: [&'static str; 2],
    continuation: &'static str,
}

fn commands() -> [CapabilityCommand; 4] {
    [
        CapabilityCommand {
            command: "search",
            family: "agent-search",
            version: SEARCH_VERSION,
            formats: ["compact", "jsonl"],
            continuation: "cursor",
        },
        CapabilityCommand {
            command: "within",
            family: "agent-within",
            version: WITHIN_VERSION,
            formats: ["compact", "jsonl"],
            continuation: "cursor+revision",
        },
        CapabilityCommand {
            command: "outline",
            family: "agent-outline",
            version: OUTLINE_VERSION,
            formats: ["compact", "jsonl"],
            continuation: "revision-guarded-read-range",
        },
        CapabilityCommand {
            command: "read",
            family: "agent-read",
            version: READ_VERSION,
            formats: ["compact", "jsonl"],
            continuation: "revision-guarded-read-range+line-slice",
        },
    ]
}

pub fn format_capabilities() -> String {
    let mut output = format!(
        "protocol agent-capabilities v={CAPABILITIES_VERSION} compatibility=same-major formats=compact,jsonl default=compact jsonl-schema={JSONL_SCHEMA_VERSION}\n"
    );
    output.push_str("budget unit=unicode-scalar hard=true jsonl=whole-records\n");
    output.push_str(
        "policy fields=tools,tool-results,thinking,subagents defaults=false,false,false,false\n",
    );
    output.push_str(
        "identity conversation=ch_ read=ch_:mN..mN focus=mN..mN project=pr_ uuid=uuid revision=rv_ anchor=ma_ guards=revision,anchor\n",
    );
    output.push_str(&format!(
        "diagnostics warning-v={WARNING_VERSION} error-v={ERROR_VERSION} warnings=stdout errors=stderr formats=compact,jsonl\n"
    ));
    output.push_str(&format!(
        "continuation token=cu_ version={CURSOR_VERSION} stale=stale-cursor local=true deterministic=true\n"
    ));
    for command in commands() {
        output.push_str(&format!(
            "command name={} family={} v={} formats={} continuation={}\n",
            command.command,
            command.family,
            command.version,
            command.formats.join(","),
            escape_atom(command.continuation)
        ));
    }
    output.push_str(
        "grammar compact=records-lines atoms=percent-encoded ordering=header,metadata,data,continuation,warnings jsonl=tagged-object-per-line\n",
    );
    output
}

pub fn capabilities_json() -> serde_json::Value {
    json!({
        "type": "capabilities",
        "schema": JSONL_SCHEMA_VERSION,
        "protocol": {"family": "agent-capabilities", "version": CAPABILITIES_VERSION, "compatibility": "same-major"},
        "formats": ["compact", "jsonl"],
        "default_format": "compact",
        "budget": {"unit": "unicode-scalar", "hard": true, "jsonl": "whole-records"},
        "content_policy": {"fields": ["tools", "tool-results", "thinking", "subagents"], "defaults": [false, false, false, false]},
        "references": {"conversation": "ch_", "read": "ch_:mN..mN", "focus": "mN..mN", "project": "pr_", "revision": "rv_", "anchor": "ma_", "uuid": "uuid"},
        "guards": ["revision", "anchor"],
        "diagnostics": {"warning_version": WARNING_VERSION, "error_version": ERROR_VERSION, "warnings": "stdout", "errors": "stderr", "formats": ["compact", "jsonl"]},
        "continuation": {"token": "cu_", "version": CURSOR_VERSION, "stale_error": "stale-cursor", "local": true, "deterministic": true},
        "commands": commands(),
        "ordering": ["header", "metadata", "data", "continuation", "warnings"]
    })
}
