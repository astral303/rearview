//! Starting an agent on a session the browser selected.

use crate::error::Result;
use std::path::Path;
use std::process::Command;

/// What a launcher needs in order to hand a session back to its agent.
pub struct SessionLaunch<'a> {
    pub path: &'a Path,
    /// The directory the session ran in, when it still exists.
    pub project_path: Option<&'a Path>,
    /// Extra arguments from `[resume].default_args`, documented as arguments for
    /// the `claude` CLI. Only Claude's launcher appends them; the others would be
    /// passing Claude flags to a different program.
    pub configured_args: &'a [String],
}

/// Builds the command that resumes a session. Building it can have effects of its
/// own — Claude copies a transcript into the current project when it has to — so a
/// launcher returns a command to run rather than running one.
pub trait SessionLauncher: Sync {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<Command>;

    /// Resume `launch` as a new session branching from the original.
    fn fork_command(&self, launch: &SessionLaunch) -> Result<Command>;
}

/// An agent that takes the transcript path on its command line.
pub struct PathResumeLauncher {
    pub program: &'static str,
    pub resume_flag: &'static str,
    pub fork_flag: &'static str,
}

impl SessionLauncher for PathResumeLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<Command> {
        let mut command = Command::new(self.program);
        command.arg(self.resume_flag).arg(launch.path);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }

    /// A fork runs where the user is now, not where the original session ran: the
    /// branch belongs to the current project.
    fn fork_command(&self, launch: &SessionLaunch) -> Result<Command> {
        let mut command = Command::new(self.program);
        command.arg(self.fork_flag).arg(launch.path);
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Source;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn launch<'a>(
        path: &'a Path,
        project_path: &'a Path,
        configured_args: &'a [String],
    ) -> SessionLaunch<'a> {
        SessionLaunch {
            path,
            project_path: Some(project_path),
            configured_args,
        }
    }

    fn arguments(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn path_resuming_agents_are_invoked_with_the_transcript_path() {
        let path = PathBuf::from("/tmp/session.jsonl");
        let project = PathBuf::from("/tmp/project");
        let claude_args = ["--dangerously-skip-permissions".to_owned()];

        for (source, program, resume_flag) in [
            (Source::Pi, "pi", "--session"),
            (Source::Omp, "omp", "--resume"),
        ] {
            let launcher = source.provider().launcher();
            let resume = launcher
                .resume_command(&launch(&path, &project, &claude_args))
                .unwrap();
            assert_eq!(resume.get_program(), OsStr::new(program));
            assert_eq!(
                arguments(&resume),
                vec![OsStr::new(resume_flag), path.as_os_str()],
                "{program} resume must not inherit Claude's configured arguments"
            );
            assert_eq!(resume.get_current_dir(), Some(project.as_path()));

            let fork = launcher
                .fork_command(&launch(&path, &project, &claude_args))
                .unwrap();
            assert_eq!(
                arguments(&fork),
                vec![OsStr::new("--fork"), path.as_os_str()]
            );
            assert_eq!(
                fork.get_current_dir(),
                None,
                "{program} fork runs in the current directory"
            );
        }
    }
}
