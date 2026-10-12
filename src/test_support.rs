//! Lists from the provider registry for the integration tests in `tests/`.
//! Not part of the library interface.

use crate::history::provider::providers;

/// Every environment variable any agent's sessions are found through, each
/// once.
pub fn storage_env_vars() -> Vec<&'static str> {
    let mut variables: Vec<&'static str> = providers()
        .iter()
        .flat_map(|provider| provider.storage_env_vars().iter().copied())
        .collect();
    variables.sort_unstable();
    variables.dedup();
    variables
}

/// Every environment variable an agent sets in the shells it runs, naming
/// the session the shell belongs to.
pub fn current_session_env_vars() -> Vec<&'static str> {
    providers()
        .iter()
        .filter_map(|provider| provider.current_session_env_var())
        .collect()
}
