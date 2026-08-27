use std::collections::HashMap;

use phux_protocol::TerminalId;

use crate::layout::{LayoutNode, NodePath, NodeStep, Rect, SplitDir};

/// One cell of the divider grid, with its resolved box-drawing glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DividerCell {
    /// Column in outer-viewport coordinates.
    pub x: u16,
    /// Row in outer-viewport coordinates.
    pub y: u16,
    /// The pre-resolved box-drawing character.
    pub ch: char,
}

/// A grab target: the divider cells of one interior split, plus the
/// identity of the [`LayoutNode::Split`] they control.
///
/// Surfaced out of the layout walk so a press on a divider cell resolves
/// to the split whose `ratio` a drag should adjust. The `axis` is the
/// split's `dir`: a `Horizontal` split paints a *vertical* line whose
/// cells move left/right under a drag, a `Vertical` split a *horizontal*
/// line whose cells move up/down. `cells` are the outer-viewport cell
/// coordinates the line occupies (the same cells the rasterizer paints a
/// glyph into), so the hit-test is an exact set-membership check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DividerHit {
    /// Path from the layout root to the controlling [`LayoutNode::Split`].
    pub node_path: NodePath,
    /// The split's axis (`dir`). Drives whether a drag reads the
    /// pointer's x (Horizontal) or y (Vertical).
    pub axis: SplitDir,
    /// Outer-viewport cells the divider line occupies, in long-axis order.
    pub cells: Vec<(u16, u16)>,
}

// -----------------------------------------------------------------------------
// Internals — segment collection, divider counting, rasterization
// -----------------------------------------------------------------------------

/// One divider segment: either a vertical line (from a Horizontal
/// split) or a horizontal line (from a Vertical split). Lives in
/// outer-viewport cell coordinates and remembers which two subtrees
/// it separates so we can resolve "is the focused pane adjacent?".
#[derive(Debug, Clone)]
pub(super) struct DividerSegment {
    /// Direction of the *line itself*: a Horizontal split produces a
    /// vertical line; we tag the segment with the split's dir so the
    /// rasterizer knows which axis to walk.
    split: SplitDir,
    /// Inclusive cell range along the segment's long axis.
    a0: u16,
    /// Inclusive cell range along the segment's long axis.
    a1: u16,
    /// Cell index on the perpendicular (cross) axis.
    cross: u16,
    /// Leaves of the subtree on the "low" side of the segment (left for
    /// Horizontal, top for Vertical). Used to resolve focus adjacency.
    low_leaves: Vec<TerminalId>,
    /// Leaves of the subtree on the "high" side.
    high_leaves: Vec<TerminalId>,
    /// Path from the layout root to the [`LayoutNode::Split`] this
    /// segment is the divider for. Carried so the divider→split identity
    /// survives into [`DividerHit`] (a press on this line resolves to
    /// this split).
    node_path: NodePath,
}

/// Wildcard handler for `#[non_exhaustive]` matches over [`LayoutNode`] /
/// [`SplitDir`]. v0.1 only knows the documented variants; a newer-server
/// forward-compat decode reaching this module is a protocol violation
/// already caught upstream.
#[cold]
#[inline(never)]
#[allow(clippy::panic)]
fn unknown_variant() -> ! {
    panic!("multi_pane: unknown wire-protocol variant (newer than this client)")
}

/// Recursively split `bounds` according to the tree, recording one
/// `DividerSegment` per interior node and the outer-viewport `Rect` of
/// every leaf. Bounds are in outer-viewport cell coordinates; the
/// divider cell is subtracted from the split axis before the ratio is
/// applied.
///
/// Exact-tiling invariant: for *any* `bounds`, the leaf rects plus the
/// divider cells these segments rasterize to cover `bounds` with zero
/// gap and zero overlap. Every leaf in the tree receives a rect — a
/// sub-viable split yields zero-size leaf rects rather than dropping
/// leaves, so the rect a pane is painted into always equals the rect
/// [`crate::multi_pane::pane_rects`] tells the server to size the PTY to.
/// The divider column/row is only reserved when the split axis has at
/// least one cell to spare; at zero width/height the subtree is invisible
/// and emits no divider.
///
/// Min-size freezing (phux-foz.3, TUI doc §6.2): each split's ratio cut
/// is clamped so both subtrees keep their aggregate minimums
/// ([`MIN_LEAF_COLS`] x [`MIN_LEAF_ROWS`] per leaf plus interior
/// dividers). A leaf squeezed to its floor freezes there and the deficit
/// redistributes to the other side — tmux's shrink behavior. When
/// `bounds` cannot fit even the aggregate minimums, the clamp disengages
/// for that split and pure proportional tiling resumes (zero-size
/// sub-viable rects, never a tiling hole), so the exact-tiling invariant
/// holds at every viewport.
pub(super) fn walk_layout(
    node: &LayoutNode,
    bounds: Rect,
    segments: &mut Vec<DividerSegment>,
    rects: &mut HashMap<TerminalId, Rect>,
) {
    walk_layout_at(node, bounds, &mut NodePath::root(), segments, rects, true);
}

/// [`walk_layout`] without min-size freezing: the raw proportional
/// tiling of the tree's ratios.
///
/// This is what the ratios *ask for*, before §6.2 freezing redistributes
/// space. The ADR-0019 decision 5 resize gate
/// (`phux_client::attach::actions`) checks candidate ratios against this
/// view — gating on the frozen rects would never trip on the frozen axis
/// (the floor holds the rect at minimum while the ratio drifts
/// unboundedly past it), so a `resize-pane` could silently bank
/// arbitrary ratio the pane would snap to on the next viewport grow.
pub(super) fn walk_layout_proportional(
    node: &LayoutNode,
    bounds: Rect,
    segments: &mut Vec<DividerSegment>,
    rects: &mut HashMap<TerminalId, Rect>,
) {
    walk_layout_at(node, bounds, &mut NodePath::root(), segments, rects, false);
}

/// [`walk_layout`] with an explicit `path` accumulator (the steps from
/// the root to `node`). `path` is pushed before recursing into each child
/// and popped after, so it always names the node currently under `bounds`.
/// `freeze` selects §6.2 min-size freezing ([`walk_layout`]) or raw
/// proportional tiling ([`walk_layout_proportional`]).
#[allow(
    clippy::too_many_lines,
    reason = "the Horizontal and Vertical arms are near-mirror child-bounds math; splitting them loses the side-by-side readability that makes the divider-reservation symmetry auditable."
)]
fn walk_layout_at(
    node: &LayoutNode,
    bounds: Rect,
    path: &mut NodePath,
    segments: &mut Vec<DividerSegment>,
    rects: &mut HashMap<TerminalId, Rect>,
    freeze: bool,
) {
    match node {
        LayoutNode::Leaf(p) => {
            rects.insert(p.clone(), bounds);
        }
        LayoutNode::Split {
            dir,
            ratio,
            left,
            right,
        } => match dir {
            SplitDir::Horizontal => {
                // Reserve one column for the divider only when there is
                // width to spare. At `bounds.w == 0` the subtree is
                // invisible: no divider, both children get zero width.
                let has_divider = bounds.w >= 1;
                let content_w = bounds.w.saturating_sub(1);
                let left_w = if freeze {
                    freeze_split_dim(content_w, *ratio, min_dims(left).0, min_dims(right).0)
                } else {
                    split_dim(content_w, *ratio)
                };
                let right_w = content_w - left_w;
                let divider_x = bounds.x + left_w;
                if has_divider {
                    segments.push(DividerSegment {
                        split: SplitDir::Horizontal,
                        a0: bounds.y,
                        a1: bounds.y + bounds.h.saturating_sub(1),
                        cross: divider_x,
                        low_leaves: collect_leaves(left),
                        high_leaves: collect_leaves(right),
                        node_path: path.clone(),
                    });
                }
                path.push(NodeStep::Left);
                walk_layout_at(
                    left,
                    Rect {
                        x: bounds.x,
                        y: bounds.y,
                        w: left_w,
                        h: bounds.h,
                    },
                    path,
                    segments,
                    rects,
                    freeze,
                );
                path.pop();
                path.push(NodeStep::Right);
                walk_layout_at(
                    right,
                    Rect {
                        x: if has_divider { divider_x + 1 } else { bounds.x },
                        y: bounds.y,
                        w: right_w,
                        h: bounds.h,
                    },
                    path,
                    segments,
                    rects,
                    freeze,
                );
                path.pop();
            }
            SplitDir::Vertical => {
                let has_divider = bounds.h >= 1;
                let content_h = bounds.h.saturating_sub(1);
                let top_h = if freeze {
                    freeze_split_dim(content_h, *ratio, min_dims(left).1, min_dims(right).1)
                } else {
                    split_dim(content_h, *ratio)
                };
                let bot_h = content_h - top_h;
                let divider_y = bounds.y + top_h;
                if has_divider {
                    segments.push(DividerSegment {
                        split: SplitDir::Vertical,
                        a0: bounds.x,
                        a1: bounds.x + bounds.w.saturating_sub(1),
                        cross: divider_y,
                        low_leaves: collect_leaves(left),
                        high_leaves: collect_leaves(right),
                        node_path: path.clone(),
                    });
                }
                path.push(NodeStep::Left);
                walk_layout_at(
                    left,
                    Rect {
                        x: bounds.x,
                        y: bounds.y,
                        w: bounds.w,
                        h: top_h,
                    },
                    path,
                    segments,
                    rects,
                    freeze,
                );
                path.pop();
                path.push(NodeStep::Right);
                walk_layout_at(
                    right,
                    Rect {
                        x: bounds.x,
                        y: if has_divider { divider_y + 1 } else { bounds.y },
                        w: bounds.w,
                        h: bot_h,
                    },
                    path,
                    segments,
                    rects,
                    freeze,
                );
                path.pop();
            }
            _ => unknown_variant(),
        },
        _ => unknown_variant(),
    }
}

/// Per-cell record of the divider grid: which edges are present at one
/// coordinate, and which of them are heavy? Order of bits is [N, E, S,
/// W]; weight is parallel.
#[derive(Default, Clone, Copy)]
struct DividerEdges {
    north: Option<bool>, // Some(heavy?) when an edge points north
    east: Option<bool>,
    south: Option<bool>,
    west: Option<bool>,
}

/// The four cardinal neighbours of one grid coordinate. A direction is
/// `Some` only when that neighbour is itself a divider cell.
#[derive(Default, Clone, Copy)]
struct Neighbours {
    north: Option<DividerEdges>,
    east: Option<DividerEdges>,
    south: Option<DividerEdges>,
    west: Option<DividerEdges>,
}

impl Neighbours {
    /// Read the divider cells cardinally adjacent to `(x, y)`.
    fn around(grid: &HashMap<(u16, u16), DividerEdges>, x: u16, y: u16) -> Self {
        Self {
            north: if y > 0 {
                grid.get(&(x, y - 1)).copied()
            } else {
                None
            },
            east: grid.get(&(x + 1, y)).copied(),
            south: grid.get(&(x, y + 1)).copied(),
            west: if x > 0 {
                grid.get(&(x - 1, y)).copied()
            } else {
                None
            },
        }
    }
}

impl DividerEdges {
    /// Inherit a junction edge toward each cardinal neighbour that is a
    /// divider cell and that we don't already have an edge toward. The
    /// inherited weight matches the neighbour's touching edge.
    fn inherit_junctions(&mut self, neighbours: &Neighbours) {
        if self.north.is_none() {
            // The neighbour's south edge is what touches us.
            self.north = neighbours.north.and_then(|n| n.south.or(n.north));
        }
        if self.south.is_none() {
            self.south = neighbours.south.and_then(|n| n.north.or(n.south));
        }
        if self.east.is_none() {
            self.east = neighbours.east.and_then(|n| n.west.or(n.east));
        }
        if self.west.is_none() {
            self.west = neighbours.west.and_then(|n| n.east.or(n.west));
        }
    }
}

/// Does the focused pane's rect share an edge with a vertical line at
/// column `seg.cross`? Adjacent iff the focused rect's left edge ==
/// cross+1 OR right edge == cross, AND it overlaps the segment's y
/// range.
const fn rect_borders_vertical_line(rect: Rect, seg: &DividerSegment) -> bool {
    let adjacent_x =
        rect.x == seg.cross.saturating_add(1) || rect.x.saturating_add(rect.w) == seg.cross;
    let overlaps_y = rect.y <= seg.a1 && rect.y.saturating_add(rect.h).saturating_sub(1) >= seg.a0;
    adjacent_x && overlaps_y
}

/// Mirror of [`rect_borders_vertical_line`] for a horizontal line at row
/// `seg.cross`.
const fn rect_borders_horizontal_line(rect: Rect, seg: &DividerSegment) -> bool {
    let adjacent_y =
        rect.y == seg.cross.saturating_add(1) || rect.y.saturating_add(rect.h) == seg.cross;
    let overlaps_x = rect.x <= seg.a1 && rect.x.saturating_add(rect.w).saturating_sub(1) >= seg.a0;
    adjacent_y && overlaps_x
}

/// Does the segment touch the focused pane? Focused pane is adjacent
/// iff its `TerminalId` is in either `low_leaves` or `high_leaves` AND
/// its rect borders the segment along the cross axis. We use membership
/// only — the rasterizer doesn't need the exact rect contact test
/// because every leaf in a subtree is adjacent to *its* outer divider
/// (binary-split-tree invariant).
fn segment_heavy(
    seg: &DividerSegment,
    focus: Option<&TerminalId>,
    rects: &HashMap<TerminalId, Rect>,
) -> bool {
    let Some(fid) = focus else {
        return false;
    };
    let touches = seg.low_leaves.contains(fid) || seg.high_leaves.contains(fid);
    if !touches {
        return false;
    }
    // Confirm the focused pane's rect actually shares an edge with
    // this segment. Otherwise we'd mark non-adjacent dividers heavy
    // when the focused pane is deep in one subtree.
    let Some(fr) = rects.get(fid) else {
        return false;
    };
    match seg.split {
        SplitDir::Horizontal => rect_borders_vertical_line(*fr, seg),
        SplitDir::Vertical => rect_borders_horizontal_line(*fr, seg),
        _ => false,
    }
}

/// Lay down a vertical line at column `cross` from row a0..=a1.
fn lay_down_vertical_line(
    grid: &mut HashMap<(u16, u16), DividerEdges>,
    seg: &DividerSegment,
    heavy: bool,
    vcols: u16,
    vrows: u16,
) {
    let x = seg.cross;
    if x >= vcols {
        return;
    }
    for y in seg.a0..=seg.a1.min(vrows.saturating_sub(1)) {
        let cell = grid.entry((x, y)).or_default();
        if y > seg.a0 {
            cell.north = Some(heavy);
        }
        if y < seg.a1 {
            cell.south = Some(heavy);
        }
    }
}

/// Lay down a horizontal line at row `cross` from col a0..=a1.
fn lay_down_horizontal_line(
    grid: &mut HashMap<(u16, u16), DividerEdges>,
    seg: &DividerSegment,
    heavy: bool,
    vcols: u16,
    vrows: u16,
) {
    let y = seg.cross;
    if y >= vrows {
        return;
    }
    for x in seg.a0..=seg.a1.min(vcols.saturating_sub(1)) {
        let cell = grid.entry((x, y)).or_default();
        if x > seg.a0 {
            cell.west = Some(heavy);
        }
        if x < seg.a1 {
            cell.east = Some(heavy);
        }
    }
}

/// Lay down one segment's cells, recording incident edges. A
/// `Horizontal` split paints a vertical line, a `Vertical` split a
/// horizontal one.
fn lay_down_segment(
    grid: &mut HashMap<(u16, u16), DividerEdges>,
    seg: &DividerSegment,
    heavy: bool,
    viewport: (u16, u16),
) {
    let (vcols, vrows) = viewport;
    match seg.split {
        SplitDir::Horizontal => lay_down_vertical_line(grid, seg, heavy, vcols, vrows),
        SplitDir::Vertical => lay_down_horizontal_line(grid, seg, heavy, vcols, vrows),
        _ => {}
    }
}

/// Post-pass: T-piece junctions where one segment terminates at
/// another. A cell whose neighbour is itself a divider cell gets an
/// edge pointing toward that neighbour (inheriting the neighbour's
/// weight on the touching edge). Without this pass an inner segment
/// ending against an outer segment paints as a straight line + a
/// straight perpendicular at the same coordinates, with no junction
/// glyph — visually a "broken cross."
fn inherit_junction_edges(grid: &mut HashMap<(u16, u16), DividerEdges>) {
    let cell_coords: Vec<(u16, u16)> = grid.keys().copied().collect();
    for (x, y) in cell_coords {
        let neighbours = Neighbours::around(grid, x, y);
        let Some(cell) = grid.get_mut(&(x, y)) else {
            continue;
        };
        cell.inherit_junctions(&neighbours);
    }
}

/// Resolve every grid cell to its box-drawing glyph, in stable
/// row-major output order.
fn into_divider_cells(grid: &HashMap<(u16, u16), DividerEdges>) -> Vec<DividerCell> {
    let mut keys: Vec<(u16, u16)> = grid.keys().copied().collect();
    keys.sort_by_key(|(x, y)| (*y, *x));
    keys.into_iter()
        .map(|(x, y)| {
            let cell = grid[&(x, y)];
            DividerCell {
                x,
                y,
                ch: pick_box_char(cell.north, cell.east, cell.south, cell.west),
            }
        })
        .collect()
}

/// Final pass: convert `DividerSegment`s into the per-cell
/// `DividerCell`s the painter consumes. Resolves heavy/light per
/// segment edge by checking focus adjacency, and picks junction
/// characters when two segments cross.
pub(super) fn rasterize(
    segments: &[DividerSegment],
    focus: Option<&TerminalId>,
    rects: &HashMap<TerminalId, Rect>,
    viewport: (u16, u16),
) -> Vec<DividerCell> {
    let mut grid: HashMap<(u16, u16), DividerEdges> = HashMap::new();
    for seg in segments {
        lay_down_segment(&mut grid, seg, segment_heavy(seg, focus, rects), viewport);
    }
    inherit_junction_edges(&mut grid);
    into_divider_cells(&grid)
}

/// Build the per-split grab map from the same segments [`rasterize`]
/// paints. Each [`DividerSegment`] becomes one [`DividerHit`] carrying
/// the controlling split's path + axis and the exact cells the divider
/// line occupies, clamped to `viewport` identically to [`rasterize`] so
/// the hit set and the painted glyph cells are the same cells.
///
/// Cells of an off-screen segment (its `cross` axis past the viewport,
/// per the same guards [`rasterize`] uses) are dropped; a segment that
/// clamps to zero on-screen cells still yields a `DividerHit` with an
/// empty `cells` vec, which the hit-test simply never matches.
pub(super) fn divider_hits(segments: &[DividerSegment], viewport: (u16, u16)) -> Vec<DividerHit> {
    let (vcols, vrows) = viewport;
    segments
        .iter()
        .map(|seg| {
            let mut cells = Vec::new();
            match seg.split {
                SplitDir::Horizontal => {
                    // Vertical line at column `cross` from row a0..=a1.
                    let x = seg.cross;
                    if x < vcols {
                        for y in seg.a0..=seg.a1.min(vrows.saturating_sub(1)) {
                            cells.push((x, y));
                        }
                    }
                }
                SplitDir::Vertical => {
                    // Horizontal line at row `cross` from col a0..=a1.
                    let y = seg.cross;
                    if y < vrows {
                        for x in seg.a0..=seg.a1.min(vcols.saturating_sub(1)) {
                            cells.push((x, y));
                        }
                    }
                }
                _ => {}
            }
            DividerHit {
                node_path: seg.node_path.clone(),
                axis: seg.split,
                cells,
            }
        })
        .collect()
}

/// Pick the box-drawing character for a cell given which of its four
/// edges are present, and (per edge) whether that edge is heavy.
///
/// Falls back to the all-light glyph when a cell has incident heavy
/// edges in a configuration Unicode 16 doesn't define a mixed
/// pictograph for (very rare; v0.1 only generates 2-edge crosses and
/// 4-edge plusses).
#[allow(
    clippy::match_same_arms,
    reason = "single-edge stubs share the glyph of the longer line they continue (`│` / `─` / `┃` / `━`); merging them via `|` defeats the `unnested_or_patterns` lint. Keep the 1-edge-per-arm form for documentation."
)]
const fn pick_box_char(
    north: Option<bool>,
    east: Option<bool>,
    south: Option<bool>,
    west: Option<bool>,
) -> char {
    use EdgeKind::{Absent, Heavy, Light};
    // Compact tag: (present, heavy) per side. 256-entry decision table
    // at most, but we only hit a small subset; an explicit match is
    // more readable than a numeric lookup.
    let n = edge_kind(north);
    let e = edge_kind(east);
    let s = edge_kind(south);
    let w = edge_kind(west);

    match (n, e, s, w) {
        // Pure straight pieces (and single-edge stubs treated as straight)
        (Absent, Absent, Absent, Absent) => ' ',
        (Light, Absent, Light, Absent) => '\u{2502}', // │
        (Light, Absent, Absent, Absent) => '\u{2502}',
        (Absent, Absent, Light, Absent) => '\u{2502}',
        (Heavy, Absent, Heavy, Absent) => '\u{2503}', // ┃
        (Heavy, Absent, Absent, Absent) => '\u{2503}',
        (Absent, Absent, Heavy, Absent) => '\u{2503}',
        (Absent, Light, Absent, Light) => '\u{2500}', // ─
        (Absent, Light, Absent, Absent) => '\u{2500}',
        (Absent, Absent, Absent, Light) => '\u{2500}',
        (Absent, Heavy, Absent, Heavy) => '\u{2501}', // ━
        (Absent, Heavy, Absent, Absent) => '\u{2501}',
        (Absent, Absent, Absent, Heavy) => '\u{2501}',

        // Corner pieces (light-only and heavy-only)
        (Absent, Light, Light, Absent) => '\u{250C}', // ┌
        (Absent, Heavy, Heavy, Absent) => '\u{250F}', // ┏
        (Absent, Absent, Light, Light) => '\u{2510}', // ┐
        (Absent, Absent, Heavy, Heavy) => '\u{2513}', // ┓
        (Light, Light, Absent, Absent) => '\u{2514}', // └
        (Heavy, Heavy, Absent, Absent) => '\u{2517}', // ┗
        (Light, Absent, Absent, Light) => '\u{2518}', // ┘
        (Heavy, Absent, Absent, Heavy) => '\u{251B}', // ┛

        // T-pieces (light backbone + light/heavy stems).
        (Light, Light, Light, Absent) => '\u{251C}', // ├
        (Heavy, Heavy, Heavy, Absent) => '\u{2523}', // ┣
        (Heavy, Light, Heavy, Absent) => '\u{2520}', // ┠
        (Light, Heavy, Light, Absent) => '\u{251D}', // ┝
        (Light, Absent, Light, Light) => '\u{2524}', // ┤
        (Heavy, Absent, Heavy, Heavy) => '\u{252B}', // ┫
        (Heavy, Absent, Heavy, Light) => '\u{2528}', // ┨
        (Light, Absent, Light, Heavy) => '\u{2525}', // ┥
        (Absent, Light, Light, Light) => '\u{252C}', // ┬
        (Absent, Heavy, Heavy, Heavy) => '\u{2533}', // ┳
        (Absent, Heavy, Light, Heavy) => '\u{252F}', // ┯
        (Absent, Light, Heavy, Light) => '\u{2530}', // ┰
        (Light, Light, Absent, Light) => '\u{2534}', // ┴
        (Heavy, Heavy, Absent, Heavy) => '\u{253B}', // ┻
        (Light, Heavy, Absent, Heavy) => '\u{2537}', // ┷
        (Heavy, Light, Absent, Light) => '\u{2538}', // ┸

        // Four-way crosses (pure)
        (Light, Light, Light, Light) => '\u{253C}', // ┼
        (Heavy, Heavy, Heavy, Heavy) => '\u{254B}', // ╋
        // Mixed-weight crosses (heavy on one axis)
        (Heavy, Light, Heavy, Light) => '\u{2542}', // ╂
        (Light, Heavy, Light, Heavy) => '\u{253F}', // ┿
        // Mixed-weight crosses (heavy on adjacent edges, approximated)
        (Light, Heavy, Heavy, Light) => '\u{2545}', // ╅
        (Heavy, Heavy, Light, Light) => '\u{2546}', // ╆
        (Heavy, Light, Light, Heavy) => '\u{2548}', // ╈
        (Light, Light, Heavy, Heavy) => '\u{2549}', // ╉

        // Any remaining shape (very rare; would require non-binary
        // junctions). Fall back to a hollow space so it visually
        // signals "missing glyph" instead of pretending to be a cross.
        _ => ' ',
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    Absent,
    Light,
    Heavy,
}

const fn edge_kind(e: Option<bool>) -> EdgeKind {
    match e {
        None => EdgeKind::Absent,
        Some(false) => EdgeKind::Light,
        Some(true) => EdgeKind::Heavy,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(super) fn split_dim(total: u16, ratio: f32) -> u16 {
    // Mirror crate::layout::split_dim (private there) for divider math.
    let raw = (f32::from(total) * ratio).round();
    if raw < 0.0 {
        0
    } else if raw > f32::from(total) {
        total
    } else {
        raw as u16
    }
}

/// Minimum inner-content width of a leaf pane, in cells (TUI doc §6.2).
pub(super) const MIN_LEAF_COLS: u16 = 2;

/// Minimum inner-content height of a leaf pane, in cells (TUI doc §6.2).
pub(super) const MIN_LEAF_ROWS: u16 = 1;

/// The smallest `(cols, rows)` bounds under which every leaf of `node`
/// keeps its §6.2 floor ([`MIN_LEAF_COLS`] x [`MIN_LEAF_ROWS`]).
///
/// A split adds its one-cell divider along its own axis and takes the
/// max across the perpendicular axis, so the aggregate is exactly what
/// the divider-reservation walk needs to hand every leaf its minimum.
pub(super) fn min_dims(node: &LayoutNode) -> (u16, u16) {
    match node {
        LayoutNode::Leaf(_) => (MIN_LEAF_COLS, MIN_LEAF_ROWS),
        LayoutNode::Split {
            dir, left, right, ..
        } => {
            let (lw, lh) = min_dims(left);
            let (rw, rh) = min_dims(right);
            match dir {
                SplitDir::Horizontal => (lw.saturating_add(rw).saturating_add(1), lh.max(rh)),
                SplitDir::Vertical => (lw.max(rw), lh.saturating_add(rh).saturating_add(1)),
                _ => unknown_variant(),
            }
        }
        _ => unknown_variant(),
    }
}

/// [`split_dim`] with §6.2 min-size freezing: the low side's share of
/// `content`, clamped so the low subtree keeps `min_low` cells and the
/// high subtree keeps `min_high` (their [`min_dims`] aggregates along
/// the split axis).
///
/// When `content` cannot cover both minimums the clamp disengages and
/// the raw proportional cut is returned — the degenerate-viewport
/// fallback that preserves exact tiling (see [`walk_layout`]).
pub(super) fn freeze_split_dim(content: u16, ratio: f32, min_low: u16, min_high: u16) -> u16 {
    let low = split_dim(content, ratio);
    match min_low.checked_add(min_high) {
        Some(needed) if content >= needed => low.clamp(min_low, content - min_high),
        _ => low,
    }
}

fn collect_leaves(node: &LayoutNode) -> Vec<TerminalId> {
    let mut out = Vec::new();
    collect_leaves_into(node, &mut out);
    out
}

fn collect_leaves_into(node: &LayoutNode, out: &mut Vec<TerminalId>) {
    match node {
        LayoutNode::Leaf(p) => out.push(p.clone()),
        LayoutNode::Split { left, right, .. } => {
            collect_leaves_into(left, out);
            collect_leaves_into(right, out);
        }
        _ => {}
    }
}
