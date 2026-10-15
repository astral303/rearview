use crate::error::{AppError, Result};

/// A provider's declaration for one operation that differs between agents.
/// The app runs the operation through `T`; a marker capability holds `()`.
pub enum Capability<T = ()> {
    Capable(T),
    /// `None` unless the user needs guidance when the app refuses a request
    /// for the operation.
    NotCapable(Option<DenialDetail>),
}

impl<T> Capability<T> {
    pub fn is_capable(&self) -> bool {
        matches!(self, Self::Capable(_))
    }

    /// The carried operation, or [`AppError::NotCapable`] for `operation`
    /// asked of `agent`, the agent's display name.
    pub fn or_refuse(self, agent: &'static str, operation: Operation) -> Result<T> {
        match self {
            Self::Capable(carried) => Ok(carried),
            Self::NotCapable(detail) => Err(AppError::NotCapable {
                agent,
                operation,
                detail,
            }),
        }
    }
}

/// Guidance shown with a refusal. Agents that need the same guidance share
/// one variant, and the message names the agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DenialDetail {
    /// The agent forks only through `/fork` inside a running session.
    ResumeThenForkInside,
}

/// An operation the user asks of an agent's session, displayed as its verb.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Resume,
    Fork,
    Rename,
    Delete,
}

impl std::fmt::Display for Operation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
            Self::Rename => "rename",
            Self::Delete => "delete",
        })
    }
}

/// The refusal of `operation` asked of `agent`, its display name.
pub(crate) fn refusal_message(
    agent: &str,
    operation: &Operation,
    detail: &Option<DenialDetail>,
) -> String {
    match detail {
        Some(DenialDetail::ResumeThenForkInside) => {
            format!("{agent} forks only from inside a session; resume it, then use /fork")
        }
        None => format!("{agent} cannot {operation} a session"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_with_a_detail_names_the_agent_in_its_guidance() {
        let refusal = Capability::<()>::NotCapable(Some(DenialDetail::ResumeThenForkInside))
            .or_refuse("Alpha", Operation::Fork)
            .unwrap_err();

        assert_eq!(
            refusal.to_string(),
            "Alpha forks only from inside a session; resume it, then use /fork"
        );
    }

    #[test]
    fn a_refusal_without_a_detail_names_the_agent_and_the_operation() {
        let refusal = Capability::<()>::NotCapable(None)
            .or_refuse("Alpha", Operation::Delete)
            .unwrap_err();

        assert_eq!(refusal.to_string(), "Alpha cannot delete a session");
    }
}
