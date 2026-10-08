use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// A config file name that exists nowhere, so mise reads no mise.toml.
const NO_CONFIG_FILE: &str = "__no_such_mise_config__.toml";

/// Each script in mise-tasks/ is the command its task runs: a `[tasks.<name>]`
/// entry in mise.toml may add metadata or dependencies, but `run`,
/// `run_windows` or `file` would replace the script. Tasks included through
/// `task_config.includes` are not checked.
#[test]
fn every_mise_tasks_script_is_the_command_its_task_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scripts = canonical_or_given(&root.join("mise-tasks"));
    let file_tasks: BTreeMap<String, PathBuf> = listed_tasks(root, Some(NO_CONFIG_FILE))
        .into_iter()
        .filter_map(|(name, file)| Some((name, canonical_or_given(&file?))))
        .filter(|(_, file)| file.starts_with(&scripts))
        .collect();
    assert!(
        !file_tasks.is_empty(),
        "mise listed no task from mise-tasks/"
    );
    let resolved = listed_tasks(root, None);

    let replaced: Vec<String> = file_tasks
        .iter()
        .filter_map(|(name, script)| match resolved.get(name) {
            Some(Some(file)) if canonical_or_given(file) == *script => None,
            Some(Some(file)) => Some(format!("{name}: runs {}", file.display())),
            Some(None) => Some(format!("{name}: runs a command from mise.toml")),
            None => Some(format!("{name}: not listed with mise.toml read")),
        })
        .collect();
    assert!(
        replaced.is_empty(),
        "mise.toml replaces these mise-tasks/ scripts:\n{}",
        replaced.join("\n")
    );
}

/// Every task mise lists in `root`, by name, with the script file it runs.
/// `config_file` replaces the config file names mise looks for.
fn listed_tasks(root: &Path, config_file: Option<&str>) -> BTreeMap<String, Option<PathBuf>> {
    let mut command = Command::new("mise");
    command
        .args(["tasks", "ls", "--json", "--hidden"])
        .current_dir(root)
        // A prompt reads end of input and fails instead of waiting.
        .stdin(Stdio::null());
    match config_file {
        Some(name) => command.env("MISE_OVERRIDE_CONFIG_FILENAMES", name),
        None => command.env_remove("MISE_OVERRIDE_CONFIG_FILENAMES"),
    };
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run `mise tasks ls` (needs mise on PATH): {error}"));
    assert!(
        output.status.success(),
        "`mise tasks ls` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tasks: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("parse `mise tasks ls --json`");
    tasks
        .into_iter()
        .map(|task| {
            let name = task["name"].as_str().expect("task name").to_owned();
            (name, task["file"].as_str().map(PathBuf::from))
        })
        .collect()
}

fn canonical_or_given(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
