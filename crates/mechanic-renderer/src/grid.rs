use mechanic_config::theme::Rgb;

bitflags::bitflags! {
    /// Text-decoration and rendering flags for a single terminal cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CellFlags: u16 {
        /// Bold text weight.
        const BOLD      = 1 << 0;
        /// Italic text style.
        const ITALIC    = 1 << 1;
        /// Underline requested; single unless a style flag is set.
        const UNDERLINE = 1 << 2;
        /// Swap foreground and background colors.
        const INVERSE   = 1 << 3;
        /// Leading cell of a two-column character.
        const WIDE_CHAR = 1 << 4;
        /// Trailing cell of a two-column character; draw its background only.
        const WIDE_CHAR_SPACER = 1 << 5;
        /// Concealed text; draw its background only.
        const HIDDEN = 1 << 6;
        /// Padding before a wide character wrapped to the next row.
        const LEADING_WIDE_CHAR_SPACER = 1 << 7;
        const DOUBLE_UNDERLINE = 1 << 8;
        const UNDERCURL = 1 << 9;
        const DOTTED_UNDERLINE = 1 << 10;
        const DASHED_UNDERLINE = 1 << 11;
        const STRIKEOUT = 1 << 12;
    }
}

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
    /// Outline of the cursor cell, used when the window is unfocused.
    HollowBlock,
}

/// Renderer-side representation of one terminal cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCell {
    /// The Unicode character occupying this cell, or `' '` for an empty cell.
    pub character: char,
    /// Combining characters attached to the base character, in source order.
    pub zerowidth: String,
    /// Foreground (glyph) color.
    pub fg: Rgb,
    /// Background color.
    pub bg: Rgb,
    /// Rendering flags (bold, italic, underline, inverse).
    pub flags: CellFlags,
    pub underline_color: Option<Rgb>,
}

impl Default for RenderCell {
    fn default() -> Self {
        use mechanic_config::theme::palette;
        Self {
            character: ' ',
            zerowidth: String::new(),
            fg: palette::ELECTRIC,
            bg: palette::BLACK,
            flags: CellFlags::empty(),
            underline_color: None,
        }
    }
}

/// Visible cells in row-major order: cells[row * cols + col].
#[derive(Debug)]
pub struct RenderGrid {
    /// Cells in row-major order.
    pub cells: Vec<RenderCell>,
    /// Number of columns in the grid.
    pub cols: usize,
    /// Number of rows in the grid.
    pub rows: usize,
    /// True when a row continues into the next row through terminal soft wrapping.
    pub wrapped: Vec<bool>,
    /// Logical text preceding the visible first row in its soft-wrapped paragraph.
    pub bidi_prefix: String,
    /// Logical text following the visible last row in its soft-wrapped paragraph.
    pub bidi_suffix: String,
    /// Context exceeded the bounded byte/cell budget; paragraph resolution is partial.
    pub bidi_context_truncated: bool,
    /// `(col, row)` of the text cursor.
    pub cursor_position: (usize, usize),
    /// Visual style of the text cursor.
    pub cursor_style: CursorStyle,
    /// False for hidden cursors and cursor positions outside the viewport.
    pub cursor_visible: bool,
    /// Cursor span in terminal columns, including a wide character's spacer.
    pub cursor_width: usize,
    /// Resolved cursor outline/bar/underline color.
    pub cursor_color: Rgb,
}

impl RenderGrid {
    /// Construct an empty grid of the given dimensions filled with default cells.
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cells: vec![RenderCell::default(); cols * rows],
            cols,
            rows,
            wrapped: vec![false; rows],
            bidi_prefix: String::new(),
            bidi_suffix: String::new(),
            bidi_context_truncated: false,
            cursor_position: (0, 0),
            cursor_style: CursorStyle::default(),
            cursor_visible: true,
            cursor_width: 1,
            cursor_color: mechanic_config::theme::palette::CELESTE,
        }
    }

    /// Return a reference to the cell at `(col, row)`.
    pub fn get(&self, col: usize, row: usize) -> Option<&RenderCell> {
        if col < self.cols && row < self.rows {
            self.cells.get(row * self.cols + col)
        } else {
            None
        }
    }

    /// Return a mutable reference to the cell at `(col, row)`.
    pub fn get_mut(&mut self, col: usize, row: usize) -> Option<&mut RenderCell> {
        if col < self.cols && row < self.rows {
            self.cells.get_mut(row * self.cols + col)
        } else {
            None
        }
    }
}
