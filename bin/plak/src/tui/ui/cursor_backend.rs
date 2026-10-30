//! Drops repeated native-cursor commands.
//!
//! Ratatui shows or hides the cursor and moves it after every frame. The
//! Crossterm backend's draw leaves the cursor on the last cell it wrote, so
//! the move back to the caret has to happen whenever that frame drew cells.
//! A repeated hide or show is dropped. A move to the same cell is dropped
//! only when the frame drew nothing. A move still restarts the terminal
//! blink timer; the terminal owner uses a steady cursor shape so that
//! restart is not visible.

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Remembers the cursor command last sent to `inner`.
pub(super) struct SteadyCursor<B> {
    inner: B,
    visible: Option<bool>,
    position: Option<Position>,
    columns: u16,
    rows: u16,
    sized: bool,
}

impl<B> SteadyCursor<B> {
    pub(super) fn new(inner: B) -> Self {
        Self {
            inner,
            visible: None,
            position: None,
            columns: 0,
            rows: 0,
            sized: false,
        }
    }

    fn note_size(&mut self, columns: u16, rows: u16) {
        if self.sized && self.columns == columns && self.rows == rows {
            return;
        }
        self.columns = columns;
        self.rows = rows;
        self.sized = true;
        self.visible = None;
        self.position = None;
    }
}

impl<B: Backend> Backend for SteadyCursor<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut drew = false;
        let result = self.inner.draw(content.inspect(|_| drew = true));
        if drew {
            // The cell writer left the cursor on the last cell. Forget the
            // caret so the next move puts it back.
            self.position = None;
        }
        result
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        let Size { width, height } = self.inner.size()?;
        self.note_size(width, height);
        if self.visible == Some(false) {
            return Ok(());
        }
        self.inner.hide_cursor()?;
        self.visible = Some(false);
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        let Size { width, height } = self.inner.size()?;
        self.note_size(width, height);
        if self.visible == Some(true) {
            return Ok(());
        }
        self.inner.show_cursor()?;
        self.visible = Some(true);
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        if let Some(position) = self.position {
            return Ok(position);
        }
        let position = self.inner.get_cursor_position()?;
        self.position = Some(position);
        Ok(position)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        let Size { width, height } = self.inner.size()?;
        self.note_size(width, height);
        if self.position == Some(position) {
            return Ok(());
        }
        self.inner.set_cursor_position(position)?;
        self.position = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::{Backend, ClearType, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    use super::SteadyCursor;

    #[derive(Default)]
    struct CountingBackend {
        width: u16,
        height: u16,
        shows: usize,
        hides: usize,
        moves: usize,
        visible: bool,
        position: Position,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                width: 20,
                height: 8,
                ..Self::default()
            }
        }
    }

    impl Backend for CountingBackend {
        type Error = std::io::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            // Crossterm queues MoveTo and Print per cell, leaving the cursor
            // on the last one written.
            for (x, y, cell) in content {
                let width = u16::try_from(unicode_width::UnicodeWidthStr::width(cell.symbol()))
                    .unwrap_or(1)
                    .max(1);
                self.position = Position {
                    x: x.saturating_add(width),
                    y,
                };
            }
            Ok(())
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.hides += 1;
            self.visible = false;
            Ok(())
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.shows += 1;
            self.visible = true;
            Ok(())
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            Ok(self.position)
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.moves += 1;
            self.position = position.into();
            Ok(())
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn clear_region(&mut self, _clear_type: ClearType) -> Result<(), Self::Error> {
            Ok(())
        }

        fn size(&self) -> Result<Size, Self::Error> {
            Ok(Size {
                width: self.width,
                height: self.height,
            })
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            Ok(WindowSize {
                columns_rows: self.size()?,
                pixels: Size {
                    width: 0,
                    height: 0,
                },
            })
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn an_unchanged_caret_is_not_shown_or_moved_again() {
        let mut backend = SteadyCursor::new(CountingBackend::new());
        let at = Position { x: 3, y: 4 };
        backend.show_cursor().unwrap();
        backend.set_cursor_position(at).unwrap();
        backend.show_cursor().unwrap();
        backend.set_cursor_position(at).unwrap();
        backend.hide_cursor().unwrap();
        backend.hide_cursor().unwrap();
        assert_eq!(backend.inner.shows, 1);
        assert_eq!(backend.inner.moves, 1);
        assert_eq!(backend.inner.hides, 1);
        assert!(!backend.inner.visible);
        assert_eq!(backend.get_cursor_position().unwrap(), at);
    }

    #[test]
    fn a_move_and_a_resize_reach_the_backend() {
        let mut backend = SteadyCursor::new(CountingBackend::new());
        backend.show_cursor().unwrap();
        backend
            .set_cursor_position(Position { x: 1, y: 1 })
            .unwrap();
        backend
            .set_cursor_position(Position { x: 2, y: 3 })
            .unwrap();
        backend.inner.width = 30;
        backend.inner.height = 12;
        backend.show_cursor().unwrap();
        backend
            .set_cursor_position(Position { x: 2, y: 3 })
            .unwrap();
        assert_eq!(backend.inner.shows, 2, "a resize must show the caret again");
        assert_eq!(backend.inner.moves, 3, "a resize must move the caret again");
        assert_eq!(
            backend.get_cursor_position().unwrap(),
            Position { x: 2, y: 3 }
        );
    }

    #[test]
    fn a_drawn_frame_moves_the_caret_back_onto_its_cell() {
        let mut backend = SteadyCursor::new(CountingBackend::new());
        let caret = Position { x: 3, y: 4 };
        let cell = Cell::default();
        backend.draw([(1, 1, &cell)].into_iter()).unwrap();
        backend.show_cursor().unwrap();
        backend.set_cursor_position(caret).unwrap();
        assert_eq!(backend.inner.position, caret);
        let moves = backend.inner.moves;
        let shows = backend.inner.shows;
        backend.draw([(8, 2, &cell)].into_iter()).unwrap();
        assert_ne!(
            backend.inner.position, caret,
            "draw leaves the cursor on the cell it wrote"
        );
        backend.show_cursor().unwrap();
        backend.set_cursor_position(caret).unwrap();
        assert_eq!(backend.inner.shows, shows, "show stays deduped");
        assert!(
            backend.inner.moves > moves,
            "a drawn frame must move the caret back"
        );
        assert_eq!(backend.inner.position, caret);
        backend.draw(std::iter::empty()).unwrap();
        backend.show_cursor().unwrap();
        backend.set_cursor_position(caret).unwrap();
        assert_eq!(backend.inner.moves, moves + 1);
        assert_eq!(backend.inner.shows, shows);
    }
}
