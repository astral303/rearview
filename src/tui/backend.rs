//! The terminal backend adapter that shows the cursor only after placing it.

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use std::io::{self, Write};

/// Emits the terminal's `Show` after the next `MoveTo` instead of before it.
///
/// ratatui ends every draw with show-then-move. ConPTY paints as soon as the
/// cursor is shown but sits on a move-only update, so after a redraw the cursor
/// is painted on the last cell written and only later jumps back to the prompt:
/// a white block flashing over the status line while it counts. A show that no
/// move follows still goes out on the next flush.
pub struct ShowAfterMove<B> {
    inner: B,
    show_pending: bool,
}

impl<B> ShowAfterMove<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            show_pending: false,
        }
    }
}

impl<B: Backend> ShowAfterMove<B> {
    fn flush_pending_show(&mut self) -> Result<(), B::Error> {
        if self.show_pending {
            self.show_pending = false;
            self.inner.show_cursor()?;
        }
        Ok(())
    }
}

impl<B: Backend> Backend for ShowAfterMove<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), B::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), B::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), B::Error> {
        self.show_pending = false;
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), B::Error> {
        self.show_pending = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, B::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), B::Error> {
        self.inner.set_cursor_position(position)?;
        self.flush_pending_show()
    }

    fn clear(&mut self) -> Result<(), B::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), B::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, B::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, B::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), B::Error> {
        self.flush_pending_show()?;
        self.inner.flush()
    }
}

/// `execute!` on the backend, as the terminal guard does to leave the
/// alternate screen, needs the adapter to write through to the terminal.
impl<B: Write> Write for ShowAfterMove<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::CrosstermBackend;
    use std::cell::RefCell;
    use std::rc::Rc;

    const SHOW: &str = "\x1b[?25h";
    const HIDE: &str = "\x1b[?25l";
    /// `MoveTo(3, 1)`: rows and columns are 1-based on the wire.
    const MOVE_TO_3_1: &str = "\x1b[2;4H";

    /// What the backend wrote, readable while the backend still owns its
    /// writer.
    #[derive(Clone, Default)]
    struct SharedOutput(Rc<RefCell<Vec<u8>>>);

    impl SharedOutput {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    impl Write for SharedOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn backend() -> (ShowAfterMove<CrosstermBackend<SharedOutput>>, SharedOutput) {
        let output = SharedOutput::default();
        let backend = ShowAfterMove::new(CrosstermBackend::new(output.clone()));
        (backend, output)
    }

    #[test]
    fn the_cursor_is_shown_only_after_it_has_been_moved() {
        let (mut backend, output) = backend();

        backend.show_cursor().unwrap();
        backend.set_cursor_position(Position::new(3, 1)).unwrap();
        Backend::flush(&mut backend).unwrap();

        let written = output.text();
        let moved = written.find(MOVE_TO_3_1).expect("the move goes out");
        let shown = written.find(SHOW).expect("the show goes out");
        assert!(moved < shown, "show came before move in {written:?}");
    }

    #[test]
    fn a_show_that_no_move_follows_goes_out_on_flush() {
        let (mut backend, output) = backend();

        backend.show_cursor().unwrap();
        assert!(!output.text().contains(SHOW), "held until a move or flush");

        Backend::flush(&mut backend).unwrap();
        assert!(output.text().contains(SHOW));
    }

    #[test]
    fn hiding_the_cursor_drops_a_pending_show() {
        let (mut backend, output) = backend();

        backend.show_cursor().unwrap();
        backend.hide_cursor().unwrap();
        backend.set_cursor_position(Position::new(3, 1)).unwrap();
        Backend::flush(&mut backend).unwrap();

        let written = output.text();
        assert!(written.contains(HIDE));
        assert!(!written.contains(SHOW), "a hidden cursor stays hidden");
    }
}
