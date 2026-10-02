//! Native pane geometry, independent of terminal sessions and window APIs.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub type PaneId = u64;
pub type SplitId = u64;

pub const MAX_PANES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// A vertical divider, with panes side by side.
    Vertical,
    /// A horizontal divider, with panes stacked top to bottom.
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl DropEdge {
    fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Vertical,
            Self::Top | Self::Bottom => Axis::Horizontal,
        }
    }

    fn new_pane_first(self) -> bool {
        matches!(self, Self::Left | Self::Top)
    }
}

/// Only geometry and focus are persisted; terminal contents are never captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneTreeSnapshot {
    pub root: PaneNodeSnapshot,
    pub active: PaneId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaneNodeSnapshot {
    Leaf { id: PaneId },
    Split { id: SplitId, axis: Axis, ratio: f64, first: Box<Self>, second: Box<Self> },
}

impl PaneTreeSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        let (panes, _) = self.validated_ids()?;
        if !panes.contains(&self.active) {
            return Err("active pane is absent from the layout".into());
        }
        Ok(())
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        fn visit(node: &PaneNodeSnapshot, ids: &mut Vec<PaneId>) {
            match node {
                PaneNodeSnapshot::Leaf { id } => ids.push(*id),
                PaneNodeSnapshot::Split { first, second, .. } => {
                    visit(first, ids);
                    visit(second, ids);
                }
            }
        }
        let mut ids = Vec::new();
        visit(&self.root, &mut ids);
        ids
    }

    fn validated_ids(&self) -> Result<(HashSet<PaneId>, HashSet<SplitId>), String> {
        fn visit(
            node: &PaneNodeSnapshot,
            depth: usize,
            nodes: &mut usize,
            panes: &mut HashSet<PaneId>,
            splits: &mut HashSet<SplitId>,
        ) -> Result<(), String> {
            *nodes += 1;
            if depth > MAX_PANES || *nodes > 2 * MAX_PANES - 1 {
                return Err("pane layout exceeds the node or depth limit".into());
            }
            match node {
                PaneNodeSnapshot::Leaf { id } => {
                    if !panes.insert(*id) {
                        return Err("duplicate pane ID".into());
                    }
                    if panes.len() > MAX_PANES {
                        return Err("pane layout exceeds the pane limit".into());
                    }
                }
                PaneNodeSnapshot::Split { id, ratio, first, second, .. } => {
                    if !splits.insert(*id) {
                        return Err("duplicate split ID".into());
                    }
                    if !ratio.is_finite() || !(0.0..=1.0).contains(ratio) {
                        return Err("split ratio must be finite and between zero and one".into());
                    }
                    visit(first, depth + 1, nodes, panes, splits)?;
                    visit(second, depth + 1, nodes, panes, splits)?;
                }
            }
            Ok(())
        }
        let mut panes = HashSet::new();
        let mut splits = HashSet::new();
        visit(&self.root, 1, &mut 0, &mut panes, &mut splits)?;
        Ok((panes, splits))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Physical pixels. Right and bottom edges are excluded from hit testing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x.saturating_add(self.width))
            && y < f64::from(self.y.saturating_add(self.height))
    }

    fn extent(self, axis: Axis) -> u32 {
        match axis {
            Axis::Vertical => self.width,
            Axis::Horizontal => self.height,
        }
    }

    fn origin(self, axis: Axis) -> u32 {
        match axis {
            Axis::Vertical => self.x,
            Axis::Horizontal => self.y,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRect {
    pub id: PaneId,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DividerRect {
    pub id: SplitId,
    pub axis: Axis,
    pub rect: Rect,
    region: Rect,
    first_min: u32,
    second_min: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Pane(PaneId),
    Divider(SplitId),
}

#[derive(Debug, Clone)]
pub struct Layout {
    /// Leaves in stable depth-first order, also used for next/previous focus.
    pub panes: Vec<PaneRect>,
    pub dividers: Vec<DividerRect>,
    minimum: Size,
    divider_px: u32,
    bounds: Rect,
}

impl Layout {
    pub fn pane(&self, id: PaneId) -> Option<Rect> {
        self.panes.iter().find(|pane| pane.id == id).map(|pane| pane.rect)
    }

    pub fn hit_test(&self, x: f64, y: f64) -> Option<Hit> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        self.dividers
            .iter()
            .find(|divider| divider.rect.contains(x, y))
            .map(|divider| Hit::Divider(divider.id))
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|pane| pane.rect.contains(x, y))
                    .map(|pane| Hit::Pane(pane.id))
            })
    }

    /// Pick the closest edge in normalized pane coordinates. Divider and
    /// out-of-window hits are never pane drop destinations.
    pub fn drop_target(&self, x: f64, y: f64) -> Option<(PaneId, DropEdge)> {
        let Hit::Pane(id) = self.hit_test(x, y)? else {
            return None;
        };
        let rect = self.pane(id)?;
        let horizontal = (x - f64::from(rect.x)) / f64::from(rect.width);
        let vertical = (y - f64::from(rect.y)) / f64::from(rect.height);
        let distances = [
            (horizontal, DropEdge::Left),
            (1.0 - horizontal, DropEdge::Right),
            (vertical, DropEdge::Top),
            (1.0 - vertical, DropEdge::Bottom),
        ];
        let (_, edge) = distances.into_iter().min_by(|a, b| a.0.total_cmp(&b.0))?;
        Some((id, edge))
    }
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(PaneId),
    Split { id: SplitId, axis: Axis, ratio: f64, first: Box<Node>, second: Box<Node> },
}

impl Node {
    fn len(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { first, second, .. } => first.len() + second.len(),
        }
    }

    fn pane_ids(&self, ids: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => ids.push(*id),
            Self::Split { first, second, .. } => {
                first.pane_ids(ids);
                second.pane_ids(ids);
            }
        }
    }

    fn split(&mut self, target: PaneId, new: PaneId, split_id: SplitId, axis: Axis) -> bool {
        let edge = match axis {
            Axis::Vertical => DropEdge::Right,
            Axis::Horizontal => DropEdge::Bottom,
        };
        self.insert_at(target, new, split_id, edge)
    }

    fn insert_at(
        &mut self,
        target: PaneId,
        new: PaneId,
        split_id: SplitId,
        edge: DropEdge,
    ) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    id: split_id,
                    axis: edge.axis(),
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(if edge.new_pane_first() { new } else { target })),
                    second: Box::new(Self::Leaf(if edge.new_pane_first() { target } else { new })),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.insert_at(target, new, split_id, edge)
                    || second.insert_at(target, new, split_id, edge)
            }
        }
    }

    fn remove(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(id) => (id != target).then_some(Self::Leaf(id)),
            Self::Split { id, axis, ratio, first, second } => {
                match (first.remove(target), second.remove(target)) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        id,
                        axis,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(survivor), None) | (None, Some(survivor)) => Some(survivor),
                    (None, None) => None,
                }
            }
        }
    }

    fn minimum(&self, minimum: Size, divider_px: u32) -> Size {
        match self {
            Self::Leaf(_) => minimum,
            Self::Split { axis, first, second, .. } => {
                let first = first.minimum(minimum, divider_px);
                let second = second.minimum(minimum, divider_px);
                match axis {
                    Axis::Vertical => Size {
                        width: first.width.saturating_add(second.width).saturating_add(divider_px),
                        height: first.height.max(second.height),
                    },
                    Axis::Horizontal => Size {
                        width: first.width.max(second.width),
                        height: first
                            .height
                            .saturating_add(second.height)
                            .saturating_add(divider_px),
                    },
                }
            }
        }
    }

    fn layout(&self, rect: Rect, layout: &mut Layout) {
        match self {
            Self::Leaf(id) => layout.panes.push(PaneRect { id: *id, rect }),
            Self::Split { id, axis, ratio, first, second } => {
                let first_min = first.minimum(layout.minimum, layout.divider_px);
                let second_min = second.minimum(layout.minimum, layout.divider_px);
                let (first_min, second_min) = match axis {
                    Axis::Vertical => (first_min.width, second_min.width),
                    Axis::Horizontal => (first_min.height, second_min.height),
                };
                let extent = rect.extent(*axis);
                // Retain a pixel for each child whenever the window permits it.
                let divider_px = layout.divider_px.min(extent.saturating_sub(2));
                let available = extent - divider_px;
                let first_px = constrained_extent(available, *ratio, first_min, second_min);
                let second_px = available - first_px;
                let (first_rect, divider_rect, second_rect) = match axis {
                    Axis::Vertical => (
                        Rect { width: first_px, ..rect },
                        Rect { x: rect.x + first_px, width: divider_px, ..rect },
                        Rect { x: rect.x + first_px + divider_px, width: second_px, ..rect },
                    ),
                    Axis::Horizontal => (
                        Rect { height: first_px, ..rect },
                        Rect { y: rect.y + first_px, height: divider_px, ..rect },
                        Rect { y: rect.y + first_px + divider_px, height: second_px, ..rect },
                    ),
                };
                layout.dividers.push(DividerRect {
                    id: *id,
                    axis: *axis,
                    rect: divider_rect,
                    region: rect,
                    first_min,
                    second_min,
                });
                first.layout(first_rect, layout);
                second.layout(second_rect, layout);
            }
        }
    }

    fn set_ratio(&mut self, split_id: SplitId, new_ratio: f64) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { id, ratio, first, second, .. } => {
                if *id == split_id {
                    let changed = *ratio != new_ratio;
                    *ratio = new_ratio;
                    changed
                } else {
                    first.set_ratio(split_id, new_ratio) || second.set_ratio(split_id, new_ratio)
                }
            }
        }
    }
}

/// Clamp to subtree minima when possible. In undersized windows, share the
/// available pixels in proportion to those minima, preserving exact coverage.
fn constrained_extent(available: u32, ratio: f64, first_min: u32, second_min: u32) -> u32 {
    let first_min = first_min.max(1);
    let second_min = second_min.max(1);
    let total_min = u64::from(first_min) + u64::from(second_min);
    if total_min <= u64::from(available) {
        ((f64::from(available) * ratio).round() as u32).clamp(first_min, available - second_min)
    } else {
        let scaled =
            ((u64::from(available) * u64::from(first_min) + total_min / 2) / total_min) as u32;
        if available >= 2 { scaled.clamp(1, available - 1) } else { scaled }
    }
}

fn rect_can_split(rect: Rect, axis: Axis, minimum: Size, divider_px: u32) -> bool {
    match axis {
        Axis::Vertical => {
            u64::from(rect.width) >= 2 * u64::from(minimum.width.max(1)) + u64::from(divider_px)
                && rect.height >= minimum.height.max(1)
        }
        Axis::Horizontal => {
            u64::from(rect.height) >= 2 * u64::from(minimum.height.max(1)) + u64::from(divider_px)
                && rect.width >= minimum.width.max(1)
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaneTree {
    root: Node,
    active: PaneId,
    next_pane_id: Option<PaneId>,
    next_split_id: Option<SplitId>,
}

impl PaneTree {
    pub fn snapshot(&self) -> PaneTreeSnapshot {
        fn snapshot(node: &Node) -> PaneNodeSnapshot {
            match node {
                Node::Leaf(id) => PaneNodeSnapshot::Leaf { id: *id },
                Node::Split { id, axis, ratio, first, second } => PaneNodeSnapshot::Split {
                    id: *id,
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(snapshot(first)),
                    second: Box::new(snapshot(second)),
                },
            }
        }
        PaneTreeSnapshot { root: snapshot(&self.root), active: self.active }
    }

    /// Rebuild an exact validated layout and derive safe allocation counters.
    pub fn from_snapshot(snapshot: PaneTreeSnapshot) -> Result<Self, String> {
        snapshot.validate()?;
        let (panes, splits) = snapshot.validated_ids()?;
        fn import(node: PaneNodeSnapshot) -> Node {
            match node {
                PaneNodeSnapshot::Leaf { id } => Node::Leaf(id),
                PaneNodeSnapshot::Split { id, axis, ratio, first, second } => Node::Split {
                    id,
                    axis,
                    ratio,
                    first: Box::new(import(*first)),
                    second: Box::new(import(*second)),
                },
            }
        }
        Ok(Self {
            root: import(snapshot.root),
            active: snapshot.active,
            next_pane_id: panes.into_iter().max().and_then(|id| id.checked_add(1)),
            next_split_id: splits.into_iter().max().unwrap_or(0).checked_add(1),
        })
    }

    pub fn new(initial_id: PaneId) -> Self {
        Self {
            root: Node::Leaf(initial_id),
            active: initial_id,
            next_pane_id: initial_id.checked_add(1),
            next_split_id: Some(1),
        }
    }

    pub fn active(&self) -> PaneId {
        self.active
    }

    pub fn len(&self) -> usize {
        self.root.len()
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::with_capacity(MAX_PANES);
        self.root.pane_ids(&mut ids);
        ids
    }

    /// Split the active leaf and focus the new pane. Call `can_split_active`
    /// first to enforce the current window's size; this operation only enforces
    /// the pane limit. Closing the returned ID rolls back a failed PTY spawn.
    pub fn split_active(&mut self, axis: Axis) -> Option<PaneId> {
        if self.len() >= MAX_PANES {
            return None;
        }
        let pane_id = self.next_pane_id?;
        let split_id = self.next_split_id?;
        if !self.root.split(self.active, pane_id, split_id, axis) {
            return None;
        }
        self.next_pane_id = pane_id.checked_add(1);
        self.next_split_id = split_id.checked_add(1);
        self.active = pane_id;
        Some(pane_id)
    }

    pub fn can_split_active(&self, axis: Axis, layout: &Layout) -> bool {
        if self.len() >= MAX_PANES || self.next_pane_id.is_none() || self.next_split_id.is_none() {
            return false;
        }
        let Some(rect) = layout.pane(self.active) else { return false };
        rect_can_split(rect, axis, layout.minimum, layout.divider_px)
    }

    /// Remove the source leaf, collapse its parent, then split the target at
    /// the requested edge. Pane IDs, focus, and unrelated split ratios survive.
    /// Require a valid `move_preview` first to enforce physical cell minima.
    pub fn move_pane(&mut self, source: PaneId, target: PaneId, edge: DropEdge) -> bool {
        let Some(candidate) = self.move_candidate(source, target, edge) else {
            return false;
        };
        *self = candidate;
        true
    }

    /// Exact receiving rectangle after the source parent collapses. Return
    /// no preview when any pane in the result would fall below the cell minima.
    pub fn move_preview(
        &self,
        source: PaneId,
        target: PaneId,
        edge: DropEdge,
        layout: &Layout,
    ) -> Option<Rect> {
        let candidate = self.move_candidate(source, target, edge)?;
        candidate.fitting_layout(layout)?.pane(source)
    }

    /// Allocate a local ID for an incoming live pane and insert it at an edge.
    /// Capacity, ID exhaustion, and the complete resulting geometry are checked
    /// before committing, so failed docking leaves this tree unchanged.
    pub fn insert_pane(
        &mut self,
        target: PaneId,
        edge: DropEdge,
        layout: &Layout,
    ) -> Option<PaneId> {
        let (candidate, pane, _) = self.insert_candidate(target, edge, layout)?;
        *self = candidate;
        Some(pane)
    }

    pub fn insert_preview(&self, target: PaneId, edge: DropEdge, layout: &Layout) -> Option<Rect> {
        self.insert_candidate(target, edge, layout).map(|(_, _, rect)| rect)
    }

    fn insert_candidate(
        &self,
        target: PaneId,
        edge: DropEdge,
        layout: &Layout,
    ) -> Option<(Self, PaneId, Rect)> {
        if self.len() >= MAX_PANES {
            return None;
        }
        let pane = self.next_pane_id?;
        let split = self.next_split_id?;
        let mut candidate = self.clone();
        if !candidate.root.insert_at(target, pane, split, edge) {
            return None;
        }
        candidate.active = pane;
        candidate.next_pane_id = pane.checked_add(1);
        candidate.next_split_id = split.checked_add(1);
        let rect = candidate.fitting_layout(layout)?.pane(pane)?;
        Some((candidate, pane, rect))
    }

    fn move_candidate(&self, source: PaneId, target: PaneId, edge: DropEdge) -> Option<Self> {
        let ids = self.pane_ids();
        if source == target || !ids.contains(&source) || !ids.contains(&target) {
            return None;
        }
        let split_id = self.next_split_id?;
        let mut candidate = self.clone();
        candidate.root = candidate.root.remove(source)?;
        if !candidate.root.insert_at(target, source, split_id, edge) {
            return None;
        }
        candidate.next_split_id = split_id.checked_add(1);
        Some(candidate)
    }

    fn fitting_layout(&self, layout: &Layout) -> Option<Layout> {
        let candidate = self.layout(layout.bounds, layout.minimum, layout.divider_px);
        candidate
            .panes
            .iter()
            .all(|pane| {
                pane.rect.width >= layout.minimum.width.max(1)
                    && pane.rect.height >= layout.minimum.height.max(1)
            })
            .then_some(candidate)
    }

    /// Remove a leaf and collapse its parent into the surviving sibling.
    /// The final pane belongs to the window and cannot be removed here.
    pub fn close(&mut self, id: PaneId) -> bool {
        let ids = self.pane_ids();
        let Some(index) = ids.iter().position(|pane| *pane == id) else { return false };
        if ids.len() == 1 {
            return false;
        }
        let root = std::mem::replace(&mut self.root, Node::Leaf(self.active));
        self.root = root.remove(id).expect("closing one leaf retains the other panes");
        if self.active == id {
            let remaining = self.pane_ids();
            self.active = remaining[index.min(remaining.len() - 1)];
        }
        true
    }

    pub fn focus(&mut self, id: PaneId) -> bool {
        if !self.pane_ids().contains(&id) {
            return false;
        }
        self.active = id;
        true
    }

    pub fn focus_next(&mut self) -> PaneId {
        self.focus_offset(1)
    }

    pub fn focus_previous(&mut self) -> PaneId {
        self.focus_offset(-1)
    }

    fn focus_offset(&mut self, offset: isize) -> PaneId {
        let ids = self.pane_ids();
        let index = ids.iter().position(|id| *id == self.active).unwrap_or(0);
        self.active = ids[(index as isize + offset).rem_euclid(ids.len() as isize) as usize];
        self.active
    }

    /// Prefer a pane aligned with the current pane across the requested edge,
    /// then the nearest gap and perpendicular center. At outer edges, stay put.
    pub fn focus_direction(&mut self, direction: Direction, layout: &Layout) -> Option<PaneId> {
        let current = layout.pane(self.active)?;
        if current.width == 0 || current.height == 0 {
            return None;
        }
        let candidates = layout.panes.iter().filter_map(|pane| {
            if pane.id == self.active || pane.rect.width == 0 || pane.rect.height == 0 {
                return None;
            }
            let other = pane.rect;
            let (gap, a_start, a_len, b_start, b_len) = match direction {
                Direction::Left if other.x + other.width <= current.x => (
                    current.x - (other.x + other.width),
                    current.y,
                    current.height,
                    other.y,
                    other.height,
                ),
                Direction::Right if other.x >= current.x + current.width => (
                    other.x - (current.x + current.width),
                    current.y,
                    current.height,
                    other.y,
                    other.height,
                ),
                Direction::Up if other.y + other.height <= current.y => (
                    current.y - (other.y + other.height),
                    current.x,
                    current.width,
                    other.x,
                    other.width,
                ),
                Direction::Down if other.y >= current.y + current.height => (
                    other.y - (current.y + current.height),
                    current.x,
                    current.width,
                    other.x,
                    other.width,
                ),
                _ => return None,
            };
            let aligned = a_start < b_start + b_len && b_start < a_start + a_len;
            let a_center = 2 * u64::from(a_start) + u64::from(a_len);
            let b_center = 2 * u64::from(b_start) + u64::from(b_len);
            Some(((!aligned, gap, a_center.abs_diff(b_center), pane.id), pane.id))
        });
        let (_, id) = candidates.min_by_key(|(score, _)| *score)?;
        self.focus(id).then_some(id)
    }

    pub fn layout(&self, mut bounds: Rect, minimum: Size, divider_px: u32) -> Layout {
        // Keep additions safe even for malformed or extreme physical bounds.
        bounds.width = bounds.width.min(u32::MAX - bounds.x);
        bounds.height = bounds.height.min(u32::MAX - bounds.y);
        let count = self.len();
        let mut layout = Layout {
            panes: Vec::with_capacity(count),
            dividers: Vec::with_capacity(count.saturating_sub(1)),
            minimum,
            divider_px,
            bounds,
        };
        self.root.layout(bounds, &mut layout);
        layout
    }

    /// Zoom changes only the visible layout. The tree, ratios, and pane IDs
    /// remain intact so returning to the split layout restores exact geometry.
    pub fn view_layout(
        &self,
        bounds: Rect,
        minimum: Size,
        divider_px: u32,
        zoomed: bool,
    ) -> Layout {
        let mut layout = self.layout(bounds, minimum, divider_px);
        if zoomed {
            layout.panes = vec![PaneRect { id: self.active, rect: layout.bounds }];
            layout.dividers.clear();
        }
        layout
    }

    /// Move a divider to the pointer's physical coordinates. The pointer
    /// represents the divider center; the ratio respects both subtree minima.
    pub fn drag_divider(&mut self, id: SplitId, x: f64, y: f64, layout: &Layout) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let Some(divider) = layout.dividers.iter().find(|divider| divider.id == id) else {
            return false;
        };
        let divider_px = divider.rect.extent(divider.axis);
        let available = divider.region.extent(divider.axis) - divider_px;
        if available == 0 {
            return false;
        }
        let pointer = match divider.axis {
            Axis::Vertical => x,
            Axis::Horizontal => y,
        };
        let offset =
            pointer - f64::from(divider.region.origin(divider.axis)) - f64::from(divider_px) / 2.0;
        let desired_ratio = (offset / f64::from(available)).clamp(0.0, 1.0);
        let extent =
            constrained_extent(available, desired_ratio, divider.first_min, divider.second_min);
        self.root.set_ratio(id, f64::from(extent) / f64::from(available))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMUM: Size = Size { width: 10, height: 8 };

    #[test]
    fn incoming_panes_allocate_local_ids_and_commit_exact_previews() {
        for edge in [DropEdge::Left, DropEdge::Right, DropEdge::Top, DropEdge::Bottom] {
            let mut tree = grid();
            let original = tree.snapshot();
            let layout = tree.layout(bounds(303, 203), MINIMUM, 3);
            let preview = tree.insert_preview(1, edge, &layout).unwrap();
            assert_eq!(tree.snapshot(), original);
            let incoming = tree.insert_pane(1, edge, &layout).unwrap();
            assert_eq!(incoming, 4);
            assert_eq!(tree.active(), incoming);
            assert_eq!(tree.layout(bounds(303, 203), MINIMUM, 3).pane(incoming), Some(preview));
            assert_eq!(tree.len(), 5);
            assert!(tree.snapshot().validate().is_ok());
            assert_eq!(tree.split_active(Axis::Vertical), Some(5));
        }
    }

    #[test]
    fn rejected_incoming_panes_preserve_tree_focus_and_allocation_counters() {
        let mut tree = PaneTree::new(40);
        let too_small = tree.layout(bounds(22, 8), MINIMUM, 3);
        let original = tree.snapshot();
        let counters = (tree.next_pane_id, tree.next_split_id);
        assert_eq!(tree.insert_preview(40, DropEdge::Left, &too_small), None);
        assert_eq!(tree.insert_pane(40, DropEdge::Left, &too_small), None);
        let enough = tree.layout(bounds(200, 200), MINIMUM, 3);
        assert_eq!(tree.insert_pane(99, DropEdge::Left, &enough), None);
        assert_eq!(tree.snapshot(), original);
        assert_eq!((tree.next_pane_id, tree.next_split_id), counters);
        tree.next_pane_id = None;
        assert_eq!(tree.insert_pane(40, DropEdge::Left, &enough), None);
        tree.next_pane_id = counters.0;
        tree.next_split_id = None;
        assert_eq!(tree.insert_pane(40, DropEdge::Left, &enough), None);
        tree.next_split_id = counters.1;
        for _ in 1..MAX_PANES {
            assert!(tree.split_active(Axis::Vertical).is_some());
        }
        let full = tree.snapshot();
        let counters = (tree.next_pane_id, tree.next_split_id);
        let layout = tree.layout(bounds(2000, 2000), MINIMUM, 3);
        assert_eq!(tree.insert_preview(40, DropEdge::Top, &layout), None);
        assert_eq!(tree.insert_pane(40, DropEdge::Top, &layout), None);
        assert_eq!(tree.snapshot(), full);
        assert_eq!((tree.next_pane_id, tree.next_split_id), counters);
    }

    #[test]
    fn zoom_shows_active_pane_and_restores_tree_geometry_across_focus_and_resize() {
        let mut tree = grid();
        let layout = tree.layout(bounds(303, 203), MINIMUM, 3);
        assert!(tree.drag_divider(1, 110.0, 30.0, &layout));
        let snapshot = tree.snapshot();
        let normal = tree.view_layout(bounds(303, 203), MINIMUM, 3, false);
        let zoomed = tree.view_layout(bounds(303, 203), MINIMUM, 3, true);
        assert_eq!(zoomed.panes, [PaneRect { id: tree.active(), rect: bounds(303, 203) }]);
        assert!(zoomed.dividers.is_empty());
        assert_eq!(tree.snapshot(), snapshot);
        tree.focus(1);
        let zoomed = tree.view_layout(bounds(403, 303), MINIMUM, 3, true);
        assert_eq!(zoomed.panes, [PaneRect { id: 1, rect: bounds(403, 303) }]);
        assert_eq!(tree.snapshot().root, snapshot.root);
        assert_eq!(tree.view_layout(bounds(303, 203), MINIMUM, 3, false).panes, normal.panes);
    }

    fn bounds(width: u32, height: u32) -> Rect {
        Rect { x: 7, y: 11, width, height }
    }

    fn grid() -> PaneTree {
        let mut tree = PaneTree::new(0);
        tree.split_active(Axis::Vertical);
        tree.split_active(Axis::Horizontal);
        tree.focus(0);
        tree.split_active(Axis::Horizontal);
        tree
    }

    #[test]
    fn moving_three_columns_creates_one_large_pane_and_two_stacked_panes() {
        let mut tree = PaneTree::new(0);
        let middle = tree.split_active(Axis::Vertical).unwrap();
        let right = tree.split_active(Axis::Vertical).unwrap();
        let before = tree.layout(bounds(203, 163), MINIMUM, 3);
        let preview = tree.move_preview(right, middle, DropEdge::Bottom, &before).unwrap();
        assert!(tree.move_pane(right, middle, DropEdge::Bottom));
        assert_eq!(tree.pane_ids(), [0, middle, right]);
        assert_eq!(tree.active(), right);
        let after = tree.layout(bounds(203, 163), MINIMUM, 3);
        assert_eq!(after.pane(right), Some(preview));
        let large = after.pane(0).unwrap();
        let top = after.pane(middle).unwrap();
        let bottom = after.pane(right).unwrap();
        assert_eq!(large.height, 163);
        assert_eq!(top.x, bottom.x);
        assert_eq!(top.width, bottom.width);
        assert_eq!(top.y + top.height + 3, bottom.y);
        assert_eq!(top.height + bottom.height + 3, large.height);
    }

    #[test]
    fn nested_move_permutations_preserve_pane_ids_focus_and_preview_geometry() {
        let original = grid();
        let ids = original.pane_ids();
        for &source in &ids {
            for &target in &ids {
                if source == target {
                    continue;
                }
                for edge in [DropEdge::Left, DropEdge::Right, DropEdge::Top, DropEdge::Bottom] {
                    for &focused in &ids {
                        let mut tree = original.clone();
                        tree.focus(focused);
                        let before = tree.layout(bounds(401, 301), MINIMUM, 3);
                        let preview = tree.move_preview(source, target, edge, &before).unwrap();
                        assert!(tree.move_pane(source, target, edge));
                        assert_eq!(tree.active(), focused);
                        let mut actual_ids = tree.pane_ids();
                        actual_ids.sort_unstable();
                        let mut expected_ids = ids.clone();
                        expected_ids.sort_unstable();
                        assert_eq!(actual_ids, expected_ids);
                        assert!(tree.snapshot().validate().is_ok());
                        let layout = tree.layout(bounds(401, 301), MINIMUM, 3);
                        assert_eq!(layout.pane(source), Some(preview));
                        let moved = layout.pane(source).unwrap();
                        let target = layout.pane(target).unwrap();
                        match edge {
                            DropEdge::Left => assert_eq!(moved.x + moved.width + 3, target.x),
                            DropEdge::Right => assert_eq!(target.x + target.width + 3, moved.x),
                            DropEdge::Top => assert_eq!(moved.y + moved.height + 3, target.y),
                            DropEdge::Bottom => assert_eq!(target.y + target.height + 3, moved.y),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn moving_preserves_unaffected_branch_structure_and_ancestor_ratios() {
        let mut tree = grid();
        tree.root.set_ratio(1, 0.3);
        tree.root.set_ratio(2, 0.7);
        tree.root.set_ratio(3, 0.6);
        tree.focus(2);
        let before = tree.snapshot();
        assert!(tree.move_pane(0, 3, DropEdge::Top));
        let after = tree.snapshot();
        match (before.root, after.root) {
            (
                PaneNodeSnapshot::Split { id: old_id, ratio: old_ratio, second: old_right, .. },
                PaneNodeSnapshot::Split { id, ratio, second: right, .. },
            ) => {
                assert_eq!(id, old_id);
                assert_eq!(ratio, old_ratio);
                assert_eq!(right, old_right);
            }
            _ => panic!("outer split must survive the branch-local move"),
        }
        assert_eq!(tree.active(), 2);
    }

    #[test]
    fn drop_hit_edges_are_normalized_and_dividers_are_not_destinations() {
        let mut tree = PaneTree::new(0);
        let other = tree.split_active(Axis::Vertical).unwrap();
        let layout = tree.layout(bounds(203, 83), MINIMUM, 3);
        let rect = layout.pane(other).unwrap();
        let cx = f64::from(rect.x) + f64::from(rect.width) / 2.0;
        let cy = f64::from(rect.y) + f64::from(rect.height) / 2.0;
        for (x, y, edge) in [
            (f64::from(rect.x) + 1.0, cy, DropEdge::Left),
            (f64::from(rect.x + rect.width) - 1.0, cy, DropEdge::Right),
            (cx, f64::from(rect.y) + 1.0, DropEdge::Top),
            (cx, f64::from(rect.y + rect.height) - 1.0, DropEdge::Bottom),
        ] {
            assert_eq!(layout.drop_target(x, y), Some((other, edge)));
        }
        let divider = layout.dividers[0].rect;
        assert_eq!(layout.drop_target(f64::from(divider.x), cy), None);
        for (x, y) in [(f64::NAN, cy), (cx, f64::INFINITY), (6.0, cy), (cx, 94.0)] {
            assert_eq!(layout.drop_target(x, y), None);
        }
    }

    #[test]
    fn move_minima_use_the_final_collapsed_geometry_and_reject_unusable_shapes() {
        let mut tree = PaneTree::new(0);
        let other = tree.split_active(Axis::Vertical).unwrap();
        let layout = tree.layout(bounds(23, 19), MINIMUM, 3);
        assert!(!rect_can_split(layout.pane(other).unwrap(), Axis::Vertical, MINIMUM, 3));
        assert_eq!(tree.move_preview(0, other, DropEdge::Left, &layout), layout.pane(0));
        let tiny = tree.layout(bounds(23, 8), MINIMUM, 3);
        let before = tree.snapshot();
        assert!(tree.move_preview(0, other, DropEdge::Top, &tiny).is_none());
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn invalid_and_exhausted_moves_leave_tree_and_counters_intact() {
        let mut tree = grid();
        let original = tree.snapshot();
        let original_pane_counter = tree.next_pane_id;
        let original_split_counter = tree.next_split_id;
        for (source, target) in [(0, 0), (99, 0), (0, 99)] {
            assert!(!tree.move_pane(source, target, DropEdge::Top));
            assert_eq!(tree.snapshot(), original);
            assert_eq!(tree.next_pane_id, original_pane_counter);
            assert_eq!(tree.next_split_id, original_split_counter);
        }
        tree.next_split_id = None;
        assert!(!tree.move_pane(0, 1, DropEdge::Left));
        assert_eq!(tree.snapshot(), original);
        assert_eq!(tree.next_split_id, None);
        tree.next_split_id = original_split_counter;
        tree.next_pane_id = None;
        // Rearrangement consumes no pane ID, so existing panes remain movable.
        assert!(tree.move_pane(0, 1, DropEdge::Left));
        assert_eq!(tree.next_pane_id, None);
    }

    #[test]
    fn moves_allocate_unique_splits_without_consuming_pane_ids_even_at_the_limit() {
        let mut tree = grid();
        assert!(tree.move_pane(0, 2, DropEdge::Left));
        assert_eq!(tree.next_pane_id, Some(4));
        assert_eq!(tree.next_split_id, Some(5));
        tree.focus(0);
        let inserted = tree.split_active(Axis::Vertical).unwrap();
        assert_eq!(inserted, 4);
        assert_eq!(tree.next_split_id, Some(6));
        assert!(tree.close(inserted));
        assert!(tree.move_pane(0, 1, DropEdge::Top));
        assert_eq!(tree.next_pane_id, Some(5));
        assert_eq!(tree.next_split_id, Some(7));
        assert!(tree.snapshot().validate().is_ok());
        for _ in tree.len()..MAX_PANES {
            assert!(tree.split_active(Axis::Vertical).is_some());
        }
        let full = tree.snapshot();
        let counters = (tree.next_pane_id, tree.next_split_id);
        assert_eq!(tree.split_active(Axis::Vertical), None);
        assert_eq!(tree.snapshot(), full);
        assert_eq!((tree.next_pane_id, tree.next_split_id), counters);
        assert!(tree.move_pane(0, 1, DropEdge::Bottom));
        assert_eq!(tree.len(), MAX_PANES);
    }

    #[test]
    fn snapshot_roundtrip_retains_layout_focus_ratios_and_next_ids() {
        let mut tree = grid();
        tree.focus(1);
        let layout = tree.layout(bounds(303, 203), MINIMUM, 3);
        tree.drag_divider(1, 100.0, 40.0, &layout);
        let snapshot = tree.snapshot();
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let restored: PaneTreeSnapshot = serde_json::from_slice(&encoded).unwrap();
        let mut restored = PaneTree::from_snapshot(restored).unwrap();
        assert_eq!(restored.snapshot(), snapshot);
        assert_eq!(
            restored.layout(bounds(303, 203), MINIMUM, 3).panes,
            tree.layout(bounds(303, 203), MINIMUM, 3).panes
        );
        assert_eq!(restored.split_active(Axis::Horizontal), Some(4));
        assert_eq!(restored.layout(bounds(303, 203), MINIMUM, 3).dividers.last().unwrap().id, 4);
    }

    #[test]
    fn snapshot_rejects_duplicate_ids_bad_ratios_absent_focus_and_excess_depth() {
        let leaf = |id| Box::new(PaneNodeSnapshot::Leaf { id });
        let mut snapshot = PaneTreeSnapshot {
            active: 0,
            root: PaneNodeSnapshot::Split {
                id: 1,
                axis: Axis::Vertical,
                ratio: 0.5,
                first: leaf(0),
                second: leaf(0),
            },
        };
        assert!(snapshot.validate().is_err());
        if let PaneNodeSnapshot::Split { second, .. } = &mut snapshot.root {
            *second = leaf(1);
        }
        assert!(snapshot.validate().is_ok());
        for bad_ratio in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            if let PaneNodeSnapshot::Split { ratio, .. } = &mut snapshot.root {
                *ratio = bad_ratio;
            }
            assert!(snapshot.validate().is_err());
        }
        let mut tree = PaneTree::new(0);
        for _ in 1..MAX_PANES {
            tree.split_active(Axis::Vertical);
        }
        let mut snapshot = tree.snapshot();
        assert!(snapshot.validate().is_ok());
        snapshot.active = 99;
        assert!(snapshot.validate().is_err());
        snapshot.active = 0;
        snapshot.root = PaneNodeSnapshot::Split {
            id: 99,
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: leaf(99),
            second: Box::new(snapshot.root),
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn snapshot_rejects_duplicate_split_ids() {
        let mut snapshot = grid().snapshot();
        if let PaneNodeSnapshot::Split { id, first, .. } = &mut snapshot.root {
            if let PaneNodeSnapshot::Split { id: child_id, .. } = &mut **first {
                *child_id = *id;
            } else {
                panic!("grid's first branch is split");
            }
        }
        assert!(PaneTree::from_snapshot(snapshot).is_err());
    }

    #[test]
    fn snapshot_id_exhaustion_never_wraps_or_reuses_an_id() {
        let snapshot =
            PaneTreeSnapshot { active: u64::MAX, root: PaneNodeSnapshot::Leaf { id: u64::MAX } };
        let mut tree = PaneTree::from_snapshot(snapshot).unwrap();
        assert!(tree.split_active(Axis::Vertical).is_none());
        let mut snapshot = grid().snapshot();
        if let PaneNodeSnapshot::Split { id, .. } = &mut snapshot.root {
            *id = u64::MAX;
        }
        let mut tree = PaneTree::from_snapshot(snapshot).unwrap();
        assert!(tree.split_active(Axis::Vertical).is_none());
    }

    #[test]
    fn splits_keep_ids_and_depth_first_order() {
        let mut tree = grid();
        assert_eq!(tree.pane_ids(), [0, 3, 1, 2]);
        assert_eq!(tree.active(), 3);
        assert_eq!(tree.focus_next(), 1);
        assert_eq!(tree.focus_next(), 2);
        assert_eq!(tree.focus_next(), 0);
        assert_eq!(tree.focus_previous(), 2);
        assert!(!tree.focus(99));
        assert_eq!(tree.active(), 2);
    }

    #[test]
    fn closes_collapse_nested_splits_and_focus_a_survivor() {
        let mut tree = grid();
        assert!(tree.close(3));
        assert_eq!(tree.pane_ids(), [0, 1, 2]);
        assert_eq!(tree.active(), 1);
        let layout = tree.layout(bounds(103, 83), MINIMUM, 3);
        assert_eq!(layout.pane(0), Some(Rect { x: 7, y: 11, width: 50, height: 83 }));
        assert!(tree.close(1));
        assert_eq!(tree.active(), 2);
        assert!(tree.close(0));
        assert_eq!(tree.pane_ids(), [2]);
        assert_eq!(tree.layout(bounds(103, 83), MINIMUM, 3).pane(2), Some(bounds(103, 83)));
        assert!(!tree.close(2));
        assert!(!tree.close(99));
    }

    #[test]
    fn closing_inactive_pane_preserves_focus_and_rollback_restores_geometry() {
        let mut tree = PaneTree::new(40);
        let original = tree.layout(bounds(91, 71), MINIMUM, 3);
        let new_id = tree.split_active(Axis::Vertical).unwrap();
        assert!(tree.close(new_id));
        assert!(tree.focus(40));
        assert_eq!(tree.layout(bounds(91, 71), MINIMUM, 3).panes, original.panes);
        let next_id = tree.split_active(Axis::Horizontal).unwrap();
        assert!(next_id > new_id);
        assert!(tree.close(40));
        assert_eq!(tree.active(), next_id);
    }

    #[test]
    fn pane_limit_and_id_overflow_do_not_mutate_tree() {
        let mut tree = PaneTree::new(0);
        for _ in 1..MAX_PANES {
            assert!(tree.split_active(Axis::Vertical).is_some());
        }
        let ids = tree.pane_ids();
        assert_eq!(tree.split_active(Axis::Horizontal), None);
        assert_eq!(tree.pane_ids(), ids);
        let mut exhausted = PaneTree::new(u64::MAX);
        assert_eq!(exhausted.split_active(Axis::Vertical), None);
        assert_eq!(exhausted.pane_ids(), [u64::MAX]);
    }

    #[test]
    fn split_checks_both_axis_minima_and_divider_space() {
        let tree = PaneTree::new(0);
        assert!(!tree.can_split_active(Axis::Vertical, &tree.layout(bounds(22, 8), MINIMUM, 3)));
        assert!(tree.can_split_active(Axis::Vertical, &tree.layout(bounds(23, 8), MINIMUM, 3)));
        assert!(!tree.can_split_active(Axis::Vertical, &tree.layout(bounds(23, 7), MINIMUM, 3)));
        assert!(!tree.can_split_active(Axis::Horizontal, &tree.layout(bounds(10, 18), MINIMUM, 3)));
        assert!(tree.can_split_active(Axis::Horizontal, &tree.layout(bounds(10, 19), MINIMUM, 3)));
    }

    #[test]
    fn every_pixel_belongs_to_exactly_one_pane_or_divider_even_when_tiny() {
        let tree = grid();
        for width in 0..35 {
            for height in 0..30 {
                let region = bounds(width, height);
                let layout = tree.layout(region, MINIMUM, 3);
                for y in region.y..region.y + height {
                    for x in region.x..region.x + width {
                        let pane_hits = layout
                            .panes
                            .iter()
                            .filter(|pane| pane.rect.contains(f64::from(x), f64::from(y)))
                            .count();
                        let divider_hits = layout
                            .dividers
                            .iter()
                            .filter(|divider| divider.rect.contains(f64::from(x), f64::from(y)))
                            .count();
                        assert_eq!(pane_hits + divider_hits, 1, "size {width}x{height}, ({x},{y})");
                        assert!(layout.hit_test(f64::from(x), f64::from(y)).is_some());
                    }
                }
                for rect in layout
                    .panes
                    .iter()
                    .map(|pane| pane.rect)
                    .chain(layout.dividers.iter().map(|divider| divider.rect))
                {
                    assert!(rect.x >= region.x && rect.y >= region.y);
                    assert!(rect.x + rect.width <= region.x + width);
                    assert!(rect.y + rect.height <= region.y + height);
                }
            }
        }
    }

    #[test]
    fn undersized_asymmetric_branches_keep_a_pixel_for_each_child() {
        let mut tree = PaneTree::new(0);
        for _ in 1..MAX_PANES {
            tree.split_active(Axis::Vertical);
        }
        for width in 2..20 {
            let region = bounds(width, 30);
            let layout = tree.layout(region, MINIMUM, 4);
            assert!(layout.pane(0).unwrap().width >= 1);
            for divider in &layout.dividers {
                let available = divider.region.width - divider.rect.width;
                if available >= 2 {
                    assert!(divider.rect.x > divider.region.x);
                    assert!(
                        divider.rect.x + divider.rect.width
                            < divider.region.x + divider.region.width
                    );
                }
            }
        }
        assert_eq!(constrained_extent(0, 0.5, 1, 100), 0);
        assert_eq!(constrained_extent(2, 0.5, 1, 100), 1);
        assert_eq!(constrained_extent(2, 0.5, 100, 1), 1);
    }

    #[test]
    fn hit_testing_excludes_outer_edges_and_rejects_invalid_coordinates() {
        let tree = grid();
        let layout = tree.layout(bounds(103, 83), MINIMUM, 3);
        assert_eq!(layout.hit_test(7.0, 11.0), Some(Hit::Pane(0)));
        assert_eq!(layout.hit_test(57.5, 11.0), Some(Hit::Divider(1)));
        assert_eq!(layout.hit_test(60.0, 11.0), Some(Hit::Pane(1)));
        assert_eq!(layout.hit_test(7.0, 51.0), Some(Hit::Divider(3)));
        for (x, y) in [
            (6.99, 11.0),
            (110.0, 11.0),
            (7.0, 94.0),
            (f64::NAN, 11.0),
            (f64::INFINITY, 11.0),
            (7.0, f64::NEG_INFINITY),
        ] {
            assert_eq!(layout.hit_test(x, y), None);
        }
    }

    #[test]
    fn directional_focus_uses_adjacent_aligned_panes() {
        let mut tree = grid();
        let layout = tree.layout(bounds(103, 83), MINIMUM, 3);
        tree.focus(0);
        assert_eq!(tree.focus_direction(Direction::Right, &layout), Some(1));
        assert_eq!(tree.focus_direction(Direction::Down, &layout), Some(2));
        assert_eq!(tree.focus_direction(Direction::Left, &layout), Some(3));
        assert_eq!(tree.focus_direction(Direction::Up, &layout), Some(0));
        assert_eq!(tree.focus_direction(Direction::Up, &layout), None);
        assert_eq!(tree.active(), 0);
        assert_eq!(tree.focus_direction(Direction::Left, &layout), None);
    }

    #[test]
    fn dragging_clamps_to_recursive_minima_and_preserves_ratio_on_resize() {
        let mut tree = PaneTree::new(0);
        tree.split_active(Axis::Vertical);
        tree.split_active(Axis::Vertical);
        let layout = tree.layout(bounds(103, 40), MINIMUM, 3);
        assert!(tree.drag_divider(1, 200.0, 20.0, &layout));
        let clamped = tree.layout(bounds(103, 40), MINIMUM, 3);
        assert_eq!(clamped.pane(0).unwrap().width, 77);
        assert_eq!(clamped.pane(1).unwrap().width, 10);
        assert_eq!(clamped.pane(2).unwrap().width, 10);
        assert!(tree.drag_divider(1, 38.5, 20.0, &clamped));
        assert_eq!(tree.layout(bounds(103, 40), MINIMUM, 3).pane(0).unwrap().width, 30);
        assert_eq!(tree.layout(bounds(203, 40), MINIMUM, 3).pane(0).unwrap().width, 60);
        let tiny = tree.layout(bounds(3, 1), MINIMUM, 3);
        assert_eq!(tiny.panes.len(), 3);
        assert_eq!(tree.layout(bounds(103, 40), MINIMUM, 3).pane(0).unwrap().width, 30);
        assert!(!tree.drag_divider(999, 30.0, 20.0, &layout));
        assert!(!tree.drag_divider(1, f64::NAN, 20.0, &layout));
    }

    #[test]
    fn horizontal_drag_uses_y_and_closed_divider_is_inert() {
        let mut tree = PaneTree::new(0);
        tree.split_active(Axis::Horizontal);
        let layout = tree.layout(bounds(40, 83), MINIMUM, 3);
        assert!(tree.drag_divider(1, -500.0, 32.5, &layout));
        let resized = tree.layout(bounds(40, 83), MINIMUM, 3);
        assert_eq!(resized.pane(0).unwrap().height, 20);
        assert!(tree.close(1));
        assert!(!tree.drag_divider(1, 0.0, 40.0, &layout));
    }

    #[test]
    fn extreme_minima_and_bounds_are_safe() {
        let tree = grid();
        let layout = tree.layout(
            Rect { x: u32::MAX - 3, y: u32::MAX - 2, width: u32::MAX, height: u32::MAX },
            Size { width: u32::MAX, height: u32::MAX },
            u32::MAX,
        );
        assert_eq!(layout.panes.len(), 4);
        assert!(layout.hit_test(f64::from(u32::MAX - 1), f64::from(u32::MAX - 1)).is_some());
    }

    #[test]
    fn deep_branches_honor_minima_and_collapse_back_to_one_leaf() {
        let mut tree = PaneTree::new(0);
        for index in 1..MAX_PANES {
            let axis = if index % 2 == 0 { Axis::Horizontal } else { Axis::Vertical };
            assert!(tree.split_active(axis).is_some());
        }
        let needed = tree.root.minimum(MINIMUM, 3);
        let layout = tree.layout(bounds(needed.width, needed.height), MINIMUM, 3);
        for pane in &layout.panes {
            assert!(pane.rect.width >= MINIMUM.width);
            assert!(pane.rect.height >= MINIMUM.height);
        }
        let ids = tree.pane_ids();
        for id in ids.into_iter().skip(1).rev() {
            assert!(tree.close(id));
            assert_eq!(tree.layout(bounds(200, 200), MINIMUM, 3).dividers.len(), tree.len() - 1);
        }
        assert_eq!(tree.pane_ids(), [0]);
        assert_eq!(tree.layout(bounds(200, 200), MINIMUM, 3).pane(0), Some(bounds(200, 200)));
    }

    /// Candidate generation, minimum-size validation, and transactional commit
    /// for a balanced 16-pane layout. Alternating sibling edges keeps the same
    /// balanced geometry; all correctness checks stay outside the timed loop.
    #[test]
    #[ignore = "opt-in release benchmark; run panes::tests::move_preview_commit_benchmark --release --ignored --nocapture"]
    fn move_preview_commit_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        assert!(!black_box(cfg!(debug_assertions)), "use --release for the pane benchmark");
        let mut fixture = PaneTree::new(0);
        for axis in [Axis::Vertical, Axis::Horizontal, Axis::Vertical, Axis::Horizontal] {
            for id in fixture.pane_ids() {
                fixture.focus(id);
                assert!(fixture.split_active(axis).is_some());
            }
        }
        fixture.focus(0);
        assert_eq!(fixture.len(), MAX_PANES);
        let ids = fixture.pane_ids();
        let layout = fixture.layout(bounds(1920, 1080), MINIMUM, 3);
        assert!(
            layout
                .panes
                .iter()
                .all(|pane| pane.rect.width >= MINIMUM.width && pane.rect.height >= MINIMUM.height)
        );
        let mut samples = Vec::with_capacity(5);
        for _ in 0..5 {
            let mut tree = fixture.clone();
            let mut commits = 0;
            let start = Instant::now();
            for index in 0..10_000 {
                let pair = (index / 2) % (MAX_PANES / 2);
                let target = ids[2 * pair];
                let source = ids[2 * pair + 1];
                let edge = if index % 2 == 0 { DropEdge::Top } else { DropEdge::Bottom };
                let preview =
                    black_box(&tree).move_preview(source, target, edge, black_box(&layout));
                let committed =
                    preview.is_some() && black_box(&mut tree).move_pane(source, target, edge);
                commits += usize::from(black_box(committed));
                black_box(preview);
            }
            let elapsed = start.elapsed();
            assert_eq!(commits, 10_000);
            assert_eq!(tree.pane_ids(), ids);
            assert_eq!(tree.active(), fixture.active());
            assert!(tree.snapshot().validate().is_ok());
            assert_eq!(tree.layout(bounds(1920, 1080), MINIMUM, 3).panes, layout.panes);
            samples.push(elapsed.as_nanos() / 10_000);
        }
        samples.sort_unstable();
        eprintln!(
            "pane move benchmark: {MAX_PANES} balanced panes, candidate preview + commit; median {} ns/operation ({}..{}), 5 samples of 10,000",
            samples[2], samples[0], samples[4]
        );
    }

    /// Explicit opt-in only. Run with an optimized test binary and an otherwise
    /// idle machine; timings are descriptive rather than a platform threshold.
    #[test]
    #[ignore = "opt-in release benchmark; run panes::tests::layout_hit_test_benchmark --release --ignored --nocapture"]
    fn layout_hit_test_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        assert!(!black_box(cfg!(debug_assertions)), "use --release for the pane benchmark");
        let mut tree = PaneTree::new(0);
        for index in 1..MAX_PANES {
            tree.focus(tree.pane_ids()[(index * 7) % tree.len()]);
            tree.split_active(if index % 2 == 0 { Axis::Horizontal } else { Axis::Vertical });
        }
        let mut samples = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = Instant::now();
            for index in 0..20_000 {
                let layout = black_box(&tree).layout(
                    bounds(1920 + index % 13, 1080 + index % 11),
                    MINIMUM,
                    3,
                );
                for pane in &layout.panes {
                    black_box(layout.hit_test(
                        f64::from(pane.rect.x) + f64::from(pane.rect.width) / 2.0,
                        f64::from(pane.rect.y) + f64::from(pane.rect.height) / 2.0,
                    ));
                }
                black_box(layout);
            }
            samples.push(start.elapsed().as_nanos() / 20_000);
        }
        samples.sort_unstable();
        eprintln!(
            "pane layout benchmark: {MAX_PANES} panes, layout + {MAX_PANES} hit tests; median {} ns/iteration ({}..{}), 5 samples of 20,000",
            samples[2], samples[0], samples[4],
        );
    }
}
