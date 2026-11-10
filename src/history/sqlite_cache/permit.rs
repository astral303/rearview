//! The proof a transcript parse may run: every parse entry point takes a
//! [`ParsePermit`], and only the refresh owner of the session cache, the
//! terminal printout of a file the user named (`--render`) and the read of a
//! direct file outside every session root can build one. The TUI's drawing
//! thread can build none, and [`ParsePermit::admit`] refuses it.

use std::marker::PhantomData;

/// Permission to parse transcripts during `'a`: one write batch's work (see
/// [`RefreshToken::parse_batch`](super::refresh::RefreshToken::parse_batch)),
/// or the life of the file it was built from.
#[derive(Clone, Copy)]
pub struct ParsePermit<'a> {
    _grant: PhantomData<&'a ()>,
}

/// One transcript file's read a [`ParsePermit`] admitted. Only
/// [`ParsePermit::admit`] and [`Admitted::admit_linked`] make one, so a
/// format's own reading of a transcript cannot run without a permit, and
/// each file read takes its own admission.
pub struct Admitted {
    _proof: (),
}

impl Admitted {
    /// Admit the read of `transcript`, another file the admitted
    /// transcript's read reads, such as an earlier rollout it continues.
    pub(crate) fn admit_linked(&self, transcript: &std::path::Path) -> Admitted {
        record_admission(transcript)
    }
}

/// Test builds count the admission of `transcript` in [`admitted_reads`].
fn record_admission(transcript: &std::path::Path) -> Admitted {
    #[cfg(test)]
    admitted_reads::record(transcript);
    #[cfg(not(test))]
    let _ = transcript;
    Admitted { _proof: () }
}

impl<'a> ParsePermit<'a> {
    /// The permit of a refresh owner's write batch, for
    /// [`RefreshToken::parse_batch`](super::refresh::RefreshToken::parse_batch)
    /// to hand to the batch's work. Its lifetime is unbound: that method's
    /// closure limits it to the work.
    pub(super) fn owner() -> Self {
        Self {
            _grant: PhantomData,
        }
    }

    /// The permit of the terminal printout of the file `--render` names.
    pub(crate) fn rendered_file(_file: &'a crate::display::RenderedFile) -> Self {
        Self {
            _grant: PhantomData,
        }
    }

    /// The permit of a direct file found outside every provider's session
    /// roots, on the TUI's background worker.
    pub(crate) fn outside_every_root(
        _file: &'a crate::history::provider::OutsideEveryRoot,
    ) -> Self {
        Self {
            _grant: PhantomData,
        }
    }

    /// Admit one parse of the transcript at `transcript`. Panics in debug
    /// builds on the TUI's drawing thread. Test builds count the admission
    /// in [`admitted_reads`].
    pub(crate) fn admit(self, transcript: &std::path::Path) -> Admitted {
        super::refuse_the_drawing_thread("parse a transcript");
        record_admission(transcript)
    }
}

/// The permit tests parse their fixture transcripts with. A test that
/// asserts no parse never uses it.
#[cfg(test)]
pub(crate) const FOR_FIXTURES: ParsePermit<'static> = ParsePermit {
    _grant: PhantomData,
};

/// Every transcript file read a [`ParsePermit`] admitted in this test
/// process, by path: the file a parse names, each sub-agent transcript it
/// reads and each earlier rollout a Codex thread continues. A test counts
/// the parses of its own files here, whichever route reads them. Reads of
/// a file's opening lines only, as discovery and Codex's search for the
/// rollouts a thread continues make them, are not counted. Temporary
/// directories keep parallel tests' paths apart.
#[cfg(test)]
pub(crate) mod admitted_reads {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};

    static READS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn record(transcript: &Path) {
        READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(transcript.to_path_buf());
    }

    /// The reads of the transcript at `transcript` admitted so far.
    pub(crate) fn count(transcript: &Path) -> usize {
        READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|read| *read == transcript)
            .count()
    }
}
