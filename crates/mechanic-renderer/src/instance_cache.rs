use std::{ops::Range, sync::Arc};

use bytemuck::Zeroable;

use super::GpuInstance;
use crate::{
    grid::{CursorStyle, RenderCell, RenderGrid},
    text::ShapedRow,
};
use mechanic_config::theme::Rgb;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Appearance {
    fg: Rgb,
    bg: Rgb,
    flags: crate::grid::CellFlags,
    underline_color: Option<Rgb>,
}

impl From<&RenderCell> for Appearance {
    fn from(cell: &RenderCell) -> Self {
        Self { fg: cell.fg, bg: cell.bg, flags: cell.flags, underline_color: cell.underline_color }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CursorKey {
    col: usize,
    row: usize,
    width: usize,
    style: CursorStyle,
    color: Rgb,
    focused: bool,
    visual_col: usize,
    rtl: bool,
}

impl CursorKey {
    fn for_row(
        grid: &RenderGrid,
        shaped: &[Arc<ShapedRow>],
        row: usize,
        focused: bool,
    ) -> Option<Self> {
        // The overlay follows all row glyphs, so the final row owns its cache key.
        let shape = shaped.get(grid.cursor_position.1)?;
        let visual_col = *shape.visual_cols.get(grid.cursor_position.0)?;
        let rtl = *shape.rtl.get(grid.cursor_position.0)?;
        (grid.cursor_visible && row + 1 == grid.rows).then_some(Self {
            col: grid.cursor_position.0,
            row: grid.cursor_position.1,
            width: grid.cursor_width,
            style: grid.cursor_style,
            color: grid.cursor_color,
            focused,
            visual_col,
            rtl,
        })
    }
}

struct Row {
    cells: Vec<Appearance>,
    shaped: Arc<ShapedRow>,
    cursor: Option<CursorKey>,
    instances: Vec<GpuInstance>,
}

#[derive(Default)]
pub(super) struct InstanceCache {
    rows: Vec<Option<Row>>,
    cols: usize,
    epoch: Option<(u64, (f32, f32))>,
    foreground_offsets: Vec<usize>,
    pub instances: Vec<GpuInstance>,
    pub background_count: u32,
}

impl InstanceCache {
    pub fn invalidate(&mut self) {
        self.epoch = None;
    }

    /// Rebuild changed rows, then collect contiguous GPU upload ranges.
    pub fn update(
        &mut self,
        grid: &RenderGrid,
        shaped: &[Arc<ShapedRow>],
        epoch: (u64, (f32, f32)),
        focused: bool,
        mut build: impl FnMut(usize, Vec<GpuInstance>) -> Vec<GpuInstance>,
    ) -> Vec<Range<usize>> {
        assert_eq!(shaped.len(), grid.rows);
        if self.epoch != Some(epoch) || self.cols != grid.cols || self.rows.len() != grid.rows {
            self.rows.clear();
            self.foreground_offsets.clear();
        }
        self.epoch = Some(epoch);
        self.cols = grid.cols;
        self.rows.resize_with(grid.rows, || None);
        let mut dirty = Vec::with_capacity(grid.rows);
        for (row, shape) in shaped.iter().enumerate() {
            let cells = &grid.cells[row * grid.cols..(row + 1) * grid.cols];
            let cursor = CursorKey::for_row(grid, shaped, row, focused);
            // Shaped-row identity covers characters, marks, font and paragraph direction.
            let changed = self.rows[row].as_ref().is_none_or(|cached| {
                !Arc::ptr_eq(&cached.shaped, shape)
                    || !cached.cells.iter().copied().eq(cells.iter().map(Appearance::from))
                    || cached.cursor != cursor
            });
            if changed {
                let reusable = self.rows[row]
                    .as_mut()
                    .map(|row| std::mem::take(&mut row.instances))
                    .unwrap_or_default();
                let instances = build(row, reusable);
                assert!(instances.len() >= grid.cols);
                if let Some(cached) = self.rows[row].as_mut() {
                    for (cached, cell) in cached.cells.iter_mut().zip(cells) {
                        *cached = Appearance::from(cell);
                    }
                    cached.shaped = Arc::clone(shape);
                    cached.cursor = cursor;
                    cached.instances = instances;
                } else {
                    self.rows[row] = Some(Row {
                        cells: cells.iter().map(Appearance::from).collect(),
                        shaped: Arc::clone(shape),
                        cursor,
                        instances,
                    });
                }
            }
            dirty.push(changed);
        }

        let background_count = grid.cols * grid.rows;
        self.background_count = background_count as u32;
        let total = self.rows.iter().flatten().map(|row| row.instances.len()).sum();
        self.instances.resize(total, GpuInstance::zeroed());
        let mut uploads = Vec::new();
        // Backgrounds precede all glyphs, preserving overhangs between rows.
        for (row, changed) in dirty.iter().copied().enumerate() {
            if changed {
                let range = row * grid.cols..(row + 1) * grid.cols;
                self.instances[range.clone()]
                    .copy_from_slice(&self.rows[row].as_ref().unwrap().instances[..grid.cols]);
                add_range(&mut uploads, range);
            }
        }
        let mut offset = background_count;
        for (row, changed) in dirty.into_iter().enumerate() {
            let foreground = &self.rows[row].as_ref().unwrap().instances[grid.cols..];
            if changed || self.foreground_offsets.get(row) != Some(&offset) {
                let range = offset..offset + foreground.len();
                self.instances[range.clone()].copy_from_slice(foreground);
                add_range(&mut uploads, range);
            }
            if let Some(old) = self.foreground_offsets.get_mut(row) {
                *old = offset;
            } else {
                self.foreground_offsets.push(offset);
            }
            offset += foreground.len();
        }
        uploads
    }
}

fn add_range(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    if let Some(last) = ranges.last_mut()
        && last.end == range.start
    {
        last.end = range.end;
    } else {
        ranges.push(range);
    }
}
