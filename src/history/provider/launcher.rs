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

/// Builds the command that resumes a session, for a provider declaring
/// [`resume`](super::SessionProvider::resume) Capable. Building it can have
/// effects of its own — Claude copies a transcript into the current project
/// when it has to — so a launcher returns a command to run rather than running
/// one.
pub trait SessionResumer: Sync {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<Command>;
}

/// Builds the command that resumes `launch` as a new session branching from
/// the original, for a provider declaring
/// [`fork`](super::SessionProvider::fork) Capable. Building it can copy a
/// transcript, as Claude's fork from another project does.
pub trait SessionForker: Sync {
    fn fork_command(&self, launch: &SessionLaunch) -> Result<Command>;
}

/// An agent that takes the transcript path on its command line.
pub struct PathResumeLauncher {
    pub program: &'static str,
    pub resume_flag: &'static str,
    pub fork_flag: &'static str,
}

impl SessionResumer for PathResumeLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<Command> {
        let mut command = Command::new(self.program);
        command.arg(self.resume_flag).arg(launch.path);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }
}

impl SessionForker for PathResumeLauncher {
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
    use crate::history::provider::Capability;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn path_resuming_agents_resume_with_transcript_path() {
        let path = PathBuf::from("/tmp/session.jsonl");
        let project = PathBuf::from("/tmp/project");
        let launch = SessionLaunch {
            path: &path,
            project_path: Some(&project),
            configured_args: &["--dangerously-skip-permissions".to_owned()],
        };

        for (source, program, resume_flag) in [
            (Source::Pi, "pi", "--session"),
            (Source::Omp, "omp", "--resume"),
        ] {
            let Capability::Capable(resumer) = source.provider().resume() else {
                panic!("{program} resumes");
            };
            let resume = resumer.resume_command(&launch).unwrap();
            assert_eq!(resume.get_program(), OsStr::new(program));
            assert_eq!(
                resume.get_args().collect::<Vec<_>>(),
                vec![OsStr::new(resume_flag), path.as_os_str()],
                "{program} resume must not inherit Claude's configured arguments"
            );
            assert_eq!(resume.get_current_dir(), Some(project.as_path()));
        }
    }
}
