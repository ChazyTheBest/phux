//! Pooled libghostty render scaffolding, shared by both ends of the wire.
//!
//! Under [ADR-0013] a libghostty `Terminal` runs on the server *and* on the
//! client, and each end walks its grid through the same three libghostty
//! objects: a [`RenderState`], a [`RowIterator`], and a [`CellIterator`].
//! Allocating that trio is not free, so every walker pools it for the life of
//! the pane it serves.
//!
//! Pooling has one non-obvious hazard, and it is the reason this type exists
//! rather than four private copies of the same three fields:
//!
//! > A pooled [`RenderState`] caches what it last walked. libghostty's per-row
//! > dirty bits live on the `Terminal` and are drained by whichever
//! > `RenderState` reads a row first, so after a geometry change a pooled state
//! > can report the *new* dimensions while still serving *pre-resize* row
//! > bodies (`phux-5pyx`). A freshly allocated state has no prior cache, so its
//! > first walk observes every row as it is now.
//!
//! [`RenderPool::begin`] therefore rebuilds the trio whenever the terminal's
//! `(cols, rows)` differ from the last walk, and hands back the three objects
//! as disjoint borrows so a caller can drive the row/cell walk exactly as it
//! did with three private fields.
//!
//! # What this type deliberately does NOT own
//!
//! **Dirty policy.** `RenderState::update` *consumes* the terminal's dirty
//! bits; when and whether to clear [`Snapshot::set_dirty`] and each row's
//! `set_dirty` is a per-consumer decision, and phux's consumers legitimately
//! disagree: the server's `mark_synced` clears both, its
//! `synthesize_incremental` clears neither (an unacked diff must stay
//! re-emittable, ADR-0018), its per-consumer reference diff bypasses the dirty
//! bits entirely, and the client's renderer clears only the rows it drew.
//! Folding those into one type would erase four deliberate policies, so the
//! pool owns allocation and geometry only and leaves every dirty decision at
//! the call site.
//!
//! **The `Terminal`.** The terminal is passed to [`RenderPool::begin`] per
//! walk rather than owned here. On the server one `Terminal` is walked by
//! several pools (one per consumer); on the client the pool outlives
//! individual replica generations. Owning it would be wrong at both ends.
//!
//! This module carries no wire types and does not participate in protocol
//! versioning. It lives in `phux-protocol` behind the `server` feature for the
//! same reason [`crate::sgr`] and [`crate::kitty_replay`] do: it is a
//! libghostty-backed render helper that both `phux-server` and `phux-client`
//! need, and `phux-core` (the only other crate both could import) deliberately
//! carries no `libghostty-vt` dependency. See [ADR-0086].
//!
//! [ADR-0013]: https://github.com/phall1/phux/blob/main/ADR/0013-libghostty-bytes-on-wire.md
//! [ADR-0018]: https://github.com/phall1/phux/blob/main/ADR/0018-lazy-state-synchronization.md
//! [ADR-0086]: https://github.com/phall1/phux/blob/main/ADR/0086-shared-render-pool.md

use libghostty_vt::{
    RenderState, Terminal as GhosttyTerminal,
    render::{CellIterator, RowIterator, Snapshot},
};

/// One pooled walk of a terminal's grid.
///
/// Returned by [`RenderPool::begin`]. The three members are disjoint borrows
/// of the pool, so the usual walk still type-checks:
///
/// ```ignore
/// let RenderWalk { snapshot, rows, cells } = pool.begin(terminal)?;
/// let mut row_iter = rows.update(&snapshot)?;
/// while let Some(row) = row_iter.next() {
///     let mut cell_iter = cells.update(row)?;
///     // ...
/// }
/// ```
#[derive(Debug)]
pub struct RenderWalk<'alloc, 's> {
    /// The snapshot produced by this walk's `RenderState::update`.
    ///
    /// Reading [`Snapshot::dirty`] drains nothing further; the drain already
    /// happened inside `update`. Clearing it is the caller's decision.
    pub snapshot: Snapshot<'alloc, 's>,
    /// The pool's row iterator, borrowed for the duration of the walk.
    pub rows: &'s mut RowIterator<'alloc>,
    /// The pool's cell iterator, borrowed for the duration of the walk.
    pub cells: &'s mut CellIterator<'alloc>,
}

/// A pooled [`RenderState`] + [`RowIterator`] + [`CellIterator`], rebuilt when
/// the terminal it walks changes geometry.
///
/// Allocate one per walker (per pane, per consumer) and keep it warm across
/// frames; see the module docs for the pooling hazard it exists to close.
#[derive(Debug)]
pub struct RenderPool<'alloc> {
    state: RenderState<'alloc>,
    rows: RowIterator<'alloc>,
    cells: CellIterator<'alloc>,
    /// The `(cols, rows)` this pool last walked, or `None` before the first
    /// walk. A change rebuilds the trio.
    last_dims: Option<(u16, u16)>,
}

impl<'alloc> RenderPool<'alloc> {
    /// Allocate a fresh pool. Do this once per walker, not once per frame.
    pub fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            last_dims: None,
        })
    }

    /// The `(cols, rows)` this pool last walked, or `None` before the first
    /// [`Self::begin`].
    #[must_use]
    pub const fn last_dims(&self) -> Option<(u16, u16)> {
        self.last_dims
    }

    /// Start a walk of `terminal`, rebuilding the pooled trio first if the
    /// terminal's geometry changed since the last walk.
    ///
    /// This performs the `RenderState::update` that **drains `terminal`'s
    /// dirty bits into the pooled state**; what the caller then does with
    /// [`Snapshot::dirty`] and the per-row bits is entirely the caller's
    /// policy (see the module docs).
    pub fn begin<'s, 'cb>(
        &'s mut self,
        terminal: &GhosttyTerminal<'alloc, 'cb>,
    ) -> Result<RenderWalk<'alloc, 's>, libghostty_vt::Error> {
        self.rebuild_on_geometry_change(terminal)?;
        // Destructure so the snapshot (which borrows `state`) and the two
        // iterators are disjoint borrows rather than three overlapping
        // borrows of `self`.
        let Self {
            state, rows, cells, ..
        } = self;
        let snapshot = state.update(terminal)?;
        Ok(RenderWalk {
            snapshot,
            rows,
            cells,
        })
    }

    /// Discard and reallocate the pooled trio when `terminal`'s dimensions
    /// differ from the last walk (`phux-5pyx`).
    ///
    /// Scoped to the rare resize tick rather than every call, so the pooled
    /// allocation win survives on the steady-state hot path.
    fn rebuild_on_geometry_change<'cb>(
        &mut self,
        terminal: &GhosttyTerminal<'alloc, 'cb>,
    ) -> Result<(), libghostty_vt::Error> {
        let live = (terminal.cols()?, terminal.rows()?);
        if self.last_dims != Some(live) {
            self.state = RenderState::new()?;
            self.rows = RowIterator::new()?;
            self.cells = CellIterator::new()?;
            self.last_dims = Some(live);
        }
        Ok(())
    }
}
