// Grid data structures: the renderer-side snapshot of the terminal state.
//
// The application layer converts alacritty_terminal's internal grid into a
// `RenderGrid` each frame.  The renderer consumes it and produces pixels.

use mechanic_config::theme::Rgb;

// ── Cell flags ────────────────────────────────────────────────────────────────

bitflags::bitflags! {
    /// Text-decoration and rendering flags for a single terminal cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CellFlags: u8 {
        /// Bold text weight.
        const BOLD      = 1 << 0;
        /// Italic text style.
        const ITALIC    = 1 << 1;
        /// Draw an underline below the glyph.
        const UNDERLINE = 1 << 2;
        /// Swap foreground and background colors.
        const INVERSE   = 1 << 3;
    }
}

// ── Cursor ────────────────────────────────────────────────────────────────────

/// How the terminal cursor should be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorStyle {
    /// Solid block that covers the full cell (█).
    #[default]
    Block,
    /// Vertical bar (I-beam) on the left edge of the cell.
    Bar,
    /// Horizontal underline at the bottom of the cell.
    Underline,
}

// ── Single cell ───────────────────────────────────────────────────────────────

/// Renderer-side representation of one terminal cell.
#[derive(Debug, Clone, Copy)]
pub struct RenderCell {
    /// The Unicode character occupying this cell, or `' '` for an empty cell.
    pub character: char,
    /// Foreground (glyph) color.
    pub fg: Rgb,
    /// Background color.
    pub bg: Rgb,
    /// Rendering flags (bold, italic, underline, inverse).
    pub flags: CellFlags,
}

impl Default for RenderCell {
    fn default() -> Self {
        use mechanic_config::theme::palette;
        Self {
            character: ' ',
            fg: palette::ELECTRIC,
            bg: palette::BLACK,
            flags: CellFlags::empty(),
        }
    }
}

// ── Full grid ─────────────────────────────────────────────────────────────────

/// A complete snapshot of the visible terminal grid, ready to be handed to the
/// GPU renderer.
///
/// The cells are stored in row-major order: `cells[row * cols + col]`.
#[derive(Debug)]
pub struct RenderGrid {
    /// Cells in row-major order.
    pub cells: Vec<RenderCell>,
    /// Number of columns in the grid.
    pub cols: usize,
    /// Number of rows in the grid.
    pub rows: usize,
    /// `(col, row)` of the text cursor.
    pub cursor_position: (usize, usize),
    /// Visual style of the text cursor.
    pub cursor_style: CursorStyle,
}

impl RenderGrid {
    /// Construct an empty grid of the given dimensions filled with default cells.
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cells: vec![RenderCell::default(); cols * rows],
            cols,
            rows,
            cursor_position: (0, 0),
            cursor_style: CursorStyle::default(),
        }
    }

    /// Return a reference to the cell at `(col, row)`.
    ///
    /// Returns `None` if the coordinates are out of bounds.
    pub fn get(&self, col: usize, row: usize) -> Option<&RenderCell> {
        if col < self.cols && row < self.rows {
            self.cells.get(row * self.cols + col)
        } else {
            None
        }
    }

    /// Return a mutable reference to the cell at `(col, row)`.
    ///
    /// Returns `None` if the coordinates are out of bounds.
    pub fn get_mut(&mut self, col: usize, row: usize) -> Option<&mut RenderCell> {
        if col < self.cols && row < self.rows {
            self.cells.get_mut(row * self.cols + col)
        } else {
            None
        }
    }
}
