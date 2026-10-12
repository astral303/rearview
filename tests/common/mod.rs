use std::path::Path;
use std::process::Command;

/// The binary with every agent's session location pointed at a missing path
/// under `config`, its cache under `config`, and no current session. A test
/// that writes one agent's sessions sets that agent's variable after.
pub fn isolated_rearview(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rearview"));
    let missing = config.join("no-agent-sessions");
    for variable in rearview::test_support::storage_env_vars() {
        command.env(variable, missing.join(variable));
    }
    for variable in rearview::test_support::current_session_env_vars() {
        command.env_remove(variable);
    }
    // The spawned binary would otherwise write session caches for this
    // test's throwaway roots into the user's real cache directory.
    command.env("REARVIEW_CACHE_DIR", config.join("cache"));
    command
}
