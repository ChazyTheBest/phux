//! `PHUX_RENDER_PROF=1`: per-second render-cost counters for the attach loop.
//!
//! The client's paint cost is invisible from the outside: a frame that paints
//! and a frame that is coalesced away look identical on the wire and on the
//! glass. This module makes the difference countable, so a change to the paint
//! scheduler can be argued from numbers rather than from a screen recording.
//!
//! Every counter is a relaxed atomic behind one cached `bool`, so the disabled
//! path (the default) is a predictable-branch load per call site and nothing
//! else. Enabled, the driver's per-iteration [`tick`] emits at most one
//! `tracing::info!` per second into the ordinary client log:
//!
//! ```text
//! render_prof: frames=812 paints=61 skipped=751 bar_composes=10
//!              layouts=3 flushes=61 bytes=48213 window_ms=1000
//! ```
//!
//! * `frames` — inbound `TERMINAL_OUTPUT` frames applied to a mirror.
//! * `paints` — composited frames actually emitted to the sink.
//! * `skipped` — frames whose paint was withheld (coalesced or paced).
//! * `bar_composes` — runs of the status-bar widget pipeline.
//! * `layouts` — `compute_layout_in` calls that missed the layout cache.
//! * `flushes` / `bytes` — what reached the off-loop stdout writer.
//! * `paced_replies` — frames admitted because they answer the user's input.
//! * `paced_waits` — frames the pacer held back for its window.

#![allow(
    clippy::redundant_pub_crate,
    reason = "the module is `pub(crate)`, so `pub(crate)` items are what actually name their reach; `pub` here trips `unreachable_pub` instead"
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Relaxed everywhere: these are diagnostic counters, never a synchronization
/// edge. A torn read at a window boundary costs a slightly-off log line.
const REL: Ordering = Ordering::Relaxed;

static ENABLED: AtomicBool = AtomicBool::new(false);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

static FRAMES: AtomicU64 = AtomicU64::new(0);
static PAINTS: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static BAR_COMPOSES: AtomicU64 = AtomicU64::new(0);
static LAYOUTS: AtomicU64 = AtomicU64::new(0);
static FLUSHES: AtomicU64 = AtomicU64::new(0);
static PACED_REPLIES: AtomicU64 = AtomicU64::new(0);
static PACED_WAITS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Whether `PHUX_RENDER_PROF` asked for counters, read from the environment
/// once per process.
///
/// The one-shot latch is deliberate: the attach loop calls this on every
/// counted event, and `std::env::var` walks the environment block.
pub(crate) fn enabled() -> bool {
    if INITIALIZED.load(REL) {
        return ENABLED.load(REL);
    }
    let on = std::env::var_os("PHUX_RENDER_PROF").is_some_and(|v| v != "0" && !v.is_empty());
    ENABLED.store(on, REL);
    INITIALIZED.store(true, REL);
    on
}

macro_rules! counter {
    ($(#[$doc:meta])* $name:ident, $cell:ident) => {
        $(#[$doc])*
        pub(crate) fn $name(n: u64) {
            if enabled() {
                $cell.fetch_add(n, REL);
            }
        }
    };
}

counter!(
    /// One inbound `TERMINAL_OUTPUT` frame applied to a pane mirror.
    note_frames,
    FRAMES
);
counter!(
    /// One composited frame emitted to the sink (one DEC 2026 block).
    note_paints,
    PAINTS
);
counter!(
    /// One frame whose paint was withheld — coalesced behind a later frame
    /// for the same pane, or held back by the frame pacer.
    note_skipped,
    SKIPPED
);
counter!(
    /// One run of the status-bar widget pipeline.
    note_bar_composes,
    BAR_COMPOSES
);
counter!(
    /// One `compute_layout_in` that missed the per-frame layout cache.
    note_layouts,
    LAYOUTS
);
counter!(
    /// One buffer shipped to the off-loop stdout writer.
    note_flushes,
    FLUSHES
);
counter!(
    /// Bytes handed to the off-loop stdout writer.
    note_bytes,
    BYTES
);
counter!(
    /// One frame admitted because it answers the user's input rather than
    /// arriving unsolicited — the pacer's felt-latency exemption.
    note_paced_replies,
    PACED_REPLIES
);
counter!(
    /// One frame the pacer made wait for its window. The pair
    /// `paced_replies` / `paced_waits` is how a latency regression in the
    /// scheduler is told apart from load on the box.
    note_paced_waits,
    PACED_WAITS
);

/// The reporting window. One line per second keeps the log readable while a
/// 300k-line `seq` runs, and matches the cadence a human reads `ps` at.
const WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// Per-iteration hook: emit a counter line when the window has elapsed.
///
/// Cheap enough for the driver's settle path — one atomic load when disabled,
/// one extra `Instant::now()` comparison when enabled.
pub(crate) fn tick() {
    if !enabled() {
        return;
    }
    tick_at(std::time::Instant::now());
}

thread_local! {
    static WINDOW_START: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

fn tick_at(now: std::time::Instant) {
    let start = WINDOW_START.with(|cell| {
        let start = cell.get().unwrap_or(now);
        cell.set(Some(start));
        start
    });
    let elapsed = now.saturating_duration_since(start);
    if elapsed < WINDOW {
        return;
    }
    WINDOW_START.with(|cell| cell.set(Some(now)));
    tracing::info!(
        frames = FRAMES.swap(0, REL),
        paints = PAINTS.swap(0, REL),
        skipped = SKIPPED.swap(0, REL),
        bar_composes = BAR_COMPOSES.swap(0, REL),
        layouts = LAYOUTS.swap(0, REL),
        flushes = FLUSHES.swap(0, REL),
        paced_replies = PACED_REPLIES.swap(0, REL),
        paced_waits = PACED_WAITS.swap(0, REL),
        bytes = BYTES.swap(0, REL),
        window_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "render_prof",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The disabled default must not accumulate: every `note_*` is a branch
    /// that returns before touching its cell.
    #[test]
    fn disabled_counters_stay_at_zero() {
        // `enabled()` latches from the environment; the test process does not
        // set `PHUX_RENDER_PROF`, so this asserts the default path.
        if enabled() {
            return;
        }
        let before = FRAMES.load(REL);
        note_frames(5);
        assert_eq!(FRAMES.load(REL), before, "disabled counters must not move");
    }

    /// A window shorter than [`WINDOW`] emits nothing and keeps accumulating,
    /// so the per-second line aggregates rather than sampling.
    #[test]
    fn a_sub_window_tick_does_not_reset_counters() {
        if !enabled() {
            return;
        }
        let now = std::time::Instant::now();
        WINDOW_START.with(|cell| cell.set(Some(now)));
        note_frames(3);
        tick_at(now);
        assert!(FRAMES.load(REL) >= 3, "sub-window tick must not drain");
    }
}
