//! The proof a transcript parse may run: every parse entry point takes a
//! [`ParsePermit`], and only the refresh owner of the session cache, the
//! terminal printout of a file the user named (`--render`) and the read of a
//! direct file outside every session root can build one. The TUI's drawing
//! thread can build none, and [`ParsePermit::admit`] refuses it.

use std::marker::PhantomData;

/// Permission to parse transcripts, for as long as the owner token or the
/// bare file it was built from lives.
#[derive(Clone, Copy)]
pub struct ParsePermit<'a> {
    _grant: PhantomData<&'a ()>,
}

/// One parse a [`ParsePermit`] admitted. Only [`ParsePermit::admit`] makes
/// one, so a format's own reading of a transcript cannot run without a
/// permit.
#[derive(Clone, Copy)]
pub struct Admitted {
    _proof: (),
}

impl<'a> ParsePermit<'a> {
    /// The permit of the refresh owner whose token lives for `'a`.
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

    /// Admit one parse. Panics in debug builds on the TUI's drawing thread.
    pub(crate) fn admit(self) -> Admitted {
        super::refuse_the_drawing_thread("parse a transcript");
        Admitted { _proof: () }
    }
}

/// The permit tests parse their fixture transcripts with. A test that
/// asserts no parse never uses it.
#[cfg(test)]
pub(crate) const FOR_FIXTURES: ParsePermit<'static> = ParsePermit {
    _grant: PhantomData,
};
