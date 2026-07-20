use crate::agent::transcript::{AgentMessage, AgentMessagePart};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContentVisibility {
    pub tools: bool,
    pub tool_results: bool,
    pub thinking: bool,
    pub subagents: bool,
}

impl ContentVisibility {
    pub const SEARCH: Self = Self {
        tools: true,
        tool_results: true,
        thinking: true,
        subagents: true,
    };

    pub fn merge(&mut self, other: Self) {
        self.tools |= other.tools;
        self.tool_results |= other.tool_results;
        self.thinking |= other.thinking;
        self.subagents |= other.subagents;
    }

    pub fn message_is_visible(self, message: &AgentMessage) -> bool {
        self.subagents || message.parent_tool_use_id.is_none()
    }

    pub fn part_is_visible(self, part: &AgentMessagePart) -> bool {
        match part {
            AgentMessagePart::Text { .. } => true,
            AgentMessagePart::ToolUse { .. } => self.tools,
            AgentMessagePart::ToolResult { .. } => self.tool_results,
            AgentMessagePart::Thinking { .. } => self.thinking,
        }
    }

    pub fn atom(self) -> String {
        let mut parts = vec!["text"];
        if self.tools {
            parts.push("tools");
        }
        if self.tool_results {
            parts.push("tool-results");
        }
        if self.thinking {
            parts.push("thinking");
        }
        if self.subagents {
            parts.push("subagents");
        }
        parts.join("+")
    }
}
