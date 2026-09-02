//! The monotone repaint accumulator (ADR-0029 §2, phux agent-detector work).
//!
//! ADR-0029 was Accepted with a `RepaintLevel` accumulator specified and never
//! implemented: the driver's repaint triggers each painted inline, so two in
//! one `select!` iteration double-painted. That was tolerable while the
//! triggers were rare. It stops being tolerable the moment a server-side agent
//! detector starts writing `phux.agent/v1` records, because every
//! `METADATA_CHANGED` broadcast routes to `paint_full_frame` — an `ESC[2J`
//! full-screen clear plus a forced re-render of every pane. A burst of twenty
//! coalesced metadata frames would strobe the whole screen twenty times.
//!
//! The fix has two halves. This module is the scheduler half: triggers RAISE a
//! level during the iteration and the loop DRAINS it exactly once, so N
//! triggers collapse into one paint at the highest requested level. The other
//! half is [`super::paint::paint_chrome_in_place`], the cheap level's painter —
//! sidebar strip + status bar, no `ED2`, no pane-interior re-render.
//!
//! [`RepaintLevel`] derives `Ord` in DECLARATION order, which is what makes
//! [`RepaintAccumulator::raise_chrome`] / [`RepaintAccumulator::raise_full`] a
//! monotone `max`: idempotent, order-independent, and impossible to lower.
//!
//! [`PaintPacer`] is the same idea one level up: the accumulator collapses the
//! triggers WITHIN one loop iteration, the pacer collapses paints ACROSS
//! iterations that land inside one frame interval.

use std::time::Duration;

/// How much of the frame a drained repaint must redraw. Ordered least- to
/// most-expensive; `Ord` follows declaration order so a raise is a `max`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RepaintLevel {
    /// Nothing to do — no trigger fired this iteration.
    #[default]
    None,
    /// In-place chrome only: the sidebar strip and the status bar. No `ED2`,
    /// no pane-interior render, and the painters' own content caches make an
    /// unchanged strip/bar a zero-byte no-op.
    Chrome,
    /// The full viewport: `ED2` + every pane + dividers + chrome. Required
    /// whenever the layout (and therefore the pane rects) moved under us.
    Full,
}

/// One iteration's accumulated repaint intent (ADR-0029 §2).
///
/// Triggers raise; the loop drains once. Because the level is a `max` and the
/// drain is a single site, "two triggers in one iteration paint twice" is not
/// representable rather than merely fixed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RepaintAccumulator {
    level: RepaintLevel,
    /// `true` once a [`RepaintLevel::Full`] was raised: that path physically
    /// clears the viewport (`ED2`), so a painter's content cache must be
    /// bypassed for the cells it wiped. Reported out of [`Self::drain`] so the
    /// caller can pass the force-full flag on.
    viewport_was_cleared: bool,
    /// `true` once a frame changed something the agent-fleet dashboard projects.
    /// Carried here, and not as a loop-local `bool`, because the fleet refresh
    /// is a REPAINT trigger like any other: while the dashboard is open it
    /// repaints the overlay layer (today, via `paint_full_frame` under the
    /// modal), so a per-frame call is a per-frame full-screen clear. It must
    /// obey the same raise-then-drain-once discipline as [`Self::level`].
    fleet_dirty: bool,
}

/// One iteration's drained repaint work: the level to paint, whether the
/// viewport was physically cleared, and whether a live fleet dashboard must be
/// re-projected. Returned by [`RepaintAccumulator::drain`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Repaint {
    /// The highest level any trigger raised this iteration.
    pub(super) level: RepaintLevel,
    /// Whether a raised level physically cleared the viewport (`ED2`).
    pub(super) viewport_was_cleared: bool,
    /// Whether an open agent-fleet dashboard needs its rows rebuilt + repainted.
    pub(super) fleet_dirty: bool,
}

impl RepaintAccumulator {
    /// Request an in-place chrome repaint (sidebar strip + status bar).
    ///
    /// The cheap level: it never clears the viewport and never touches a pane
    /// interior, so it is safe to raise on every agent-state change — which,
    /// with a live detector, is the highest-frequency chrome trigger there is.
    pub(super) const fn raise_chrome(&mut self) {
        if matches!(self.level, RepaintLevel::None) {
            self.level = RepaintLevel::Chrome;
        }
    }

    /// Request a full-viewport repaint (`ED2` + every pane + chrome).
    ///
    /// Monotone: this always wins over a same-iteration `raise_chrome`, and
    /// records that the viewport was cleared.
    pub(super) const fn raise_full(&mut self) {
        self.level = RepaintLevel::Full;
        self.viewport_was_cleared = true;
    }

    /// Request a re-projection of the agent-fleet dashboard.
    ///
    /// Raised by every frame that changed fleet-projected state (an agent
    /// record, an ADR-0035 `Asked`, a pane spawn/close, a layout or
    /// session-graph change). Idempotent: nine panes publishing a state
    /// transition in one coalesced batch rebuild the fleet ONCE.
    pub(super) const fn raise_fleet(&mut self) {
        self.fleet_dirty = true;
    }

    /// The accumulated repaint work, resetting to [`Default`]. Called EXACTLY
    /// ONCE per loop iteration.
    pub(super) fn drain(&mut self) -> Repaint {
        let taken = std::mem::take(self);
        Repaint {
            level: taken.level,
            viewport_was_cleared: taken.viewport_was_cleared,
            fleet_dirty: taken.fleet_dirty,
        }
    }
}

/// The default cap on composited frames per second, as a minimum interval
/// between them.
///
/// 16ms is one frame at 60Hz: fast enough that no human perceives the delay,
/// slow enough that a producer emitting a thousand lines a second stops
/// costing a thousand full repaints. It is a FLOOR on the gap between paints,
/// never a delay added to a quiet screen — the first frame after any lull
/// paints immediately (see [`PaintPacer::admit`]), so first-byte latency for
/// a keystroke echo is unchanged.
const DEFAULT_FRAME_INTERVAL_MS: u64 = 16;

/// `PHUX_FRAME_INTERVAL_MS` overrides the pacing floor; `0` disables pacing
/// entirely (every burst paints, the pre-pacer behaviour).
fn frame_interval() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let ms = std::env::var("PHUX_FRAME_INTERVAL_MS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_FRAME_INTERVAL_MS);
        Duration::from_millis(ms)
    })
}

/// The frame-rate governor for pane output (phux-l96p.3).
///
/// The coalescing drain that preceded this collapses frames that are ALREADY
/// QUEUED on one socket read. It cannot help a producer whose lines arrive one
/// wake-up apart — a program printing progress a thousand times a second hands
/// the driver a thousand separate bursts, and every one of them used to paint.
/// On a scrolling viewport each of those paints is a full-screen repaint, so
/// the client burned tens of megabytes a second redrawing frames no eye could
/// resolve.
///
/// The policy has two halves, and the first is what keeps it honest:
///
/// * **The first frame after a lull paints immediately.** Pacing never delays
///   the byte a user is waiting for. Typing into a quiet shell, a keystroke's
///   echo is on the glass exactly as fast as before.
/// * **Frames inside the window accumulate.** Their bytes still reach the
///   libghostty mirror (the mirror is the truth; only the paint is withheld),
///   the panes they touched are remembered, and the driver settles all of them
///   in ONE composited frame when the window expires.
///
/// The accumulated set is per-pane and unordered-but-deduplicated: settling
/// twice for the same pane in one frame would paint the same rows twice.
#[derive(Debug, Default)]
pub(super) struct PaintPacer {
    /// Earliest instant the next composited frame may be emitted. `None` ⇒
    /// nothing has painted yet, so the next request paints.
    next_allowed: Option<tokio::time::Instant>,
    /// Panes whose paint was withheld and still owe a settle. Small by
    /// construction (one entry per visible pane), so a linear dedup beats a
    /// hash set.
    pending: Vec<phux_protocol::ids::TerminalId>,
    /// While set and unexpired, output counts as a REPLY to the user and is
    /// not paced.
    ///
    /// Without this the pacer taxes exactly the latency that matters most.
    /// The measured shape: p50 echo improved (222us -> 176us) while p99 went
    /// 711us -> 17.4ms and the max to 19.1ms, with the slow keystrokes
    /// clustered at one frame interval. The mechanism is a keystroke landing
    /// just after some other paint opened a window — its echo is unsolicited
    /// output as far as the pacer can tell, so it waited up to 16ms for a
    /// deadline it should never have been subject to.
    ///
    /// Pacing exists to stop a program that floods the screen from repainting
    /// faster than anyone can see. It was never meant to govern the echo of a
    /// keypress, and this deadline is the distinction: unsolicited output is
    /// paced, a reply to the user is not.
    ///
    /// A DEADLINE rather than a one-shot flag, because a reply is not always
    /// one frame — a shell echo and the readline redraw that follows it
    /// arrive as two, and a flag consumed by the first leaves the second (the
    /// one carrying the glyph the user is waiting for) to wait out the window.
    /// That is measurable: the one-shot version left the 8-22ms band at 4.3%
    /// of keystrokes against 1.2% with pacing disabled outright.
    input_until: Option<tokio::time::Instant>,
}

/// How long after an input batch output still counts as a reply to it.
///
/// Sized to cover a reply fragmented across several frames over the local
/// transport — a UDS round trip is tens of microseconds — without handing a
/// sustained flood a standing exemption. At 10 keystrokes a second this lifts
/// pacing for a fifth of the time at most, and only for the pane the user is
/// actually looking at.
const INPUT_GRACE: Duration = Duration::from_millis(20);

impl PaintPacer {
    /// Whether a composited frame may be emitted at `now`, arming the next
    /// window if so.
    ///
    /// The single decision point: callers that are admitted paint inline,
    /// callers that are refused [`Self::withhold`] every pane they touched.
    pub(super) fn admit(&mut self, now: tokio::time::Instant) -> bool {
        if frame_interval().is_zero() {
            return true;
        }
        // A frame that answers the user bypasses the window entirely, and the
        // window then restarts from THIS paint rather than from whatever
        // unsolicited output happened to open the last one.
        //
        // The grace is deliberately NOT consumed here: the rest of a
        // fragmented reply must bypass too, or the frame carrying the glyph
        // the user is waiting for is exactly the one that waits.
        if self.input_until.is_some_and(|until| now < until) {
            super::render_prof::note_paced_replies(1);
            self.next_allowed = Some(now + frame_interval());
            return true;
        }
        self.input_until = None;
        if self.next_allowed.is_some_and(|at| now < at) {
            super::render_prof::note_paced_waits(1);
            return false;
        }
        self.next_allowed = Some(now + frame_interval());
        true
    }

    /// Note that the user just sent input, so the next output frame is a
    /// reply to them and must paint immediately.
    ///
    /// Set from the one place every input batch funnels through, before the
    /// events are dispatched — a key consumed by a local keybinding counts
    /// too, because the repaint it triggers is just as much a response to the
    /// user as a pane echo is.
    pub(super) fn note_input(&mut self, now: tokio::time::Instant) {
        self.input_until = Some(now + INPUT_GRACE);
    }

    /// Start the pacing window at `now`.
    ///
    /// Called from every path that actually emits a composited frame, so the
    /// cadence is measured from paints rather than from arrivals. It does not
    /// touch the input grace: only time expires that.
    pub(super) fn rearm(&mut self, now: tokio::time::Instant) {
        self.next_allowed = Some(now + frame_interval());
    }

    /// Remember that `terminal_id` owes a settle paint.
    pub(super) fn withhold(&mut self, terminal_id: &phux_protocol::ids::TerminalId) {
        if !self.pending.iter().any(|id| id == terminal_id) {
            self.pending.push(terminal_id.clone());
        }
    }

    /// When the driver must wake to settle withheld panes, or `None` when
    /// nothing is owed.
    ///
    /// `None` on an idle screen is what keeps the pacer free: the driver arms
    /// no timer, so a quiet attach parks on its wake-up sources exactly as it
    /// did before.
    pub(super) const fn deadline(&self) -> Option<tokio::time::Instant> {
        if self.pending.is_empty() {
            None
        } else {
            self.next_allowed
        }
    }

    /// Take the panes owed a settle paint, clearing the debt.
    pub(super) fn take_pending(&mut self) -> Vec<phux_protocol::ids::TerminalId> {
        std::mem::take(&mut self.pending)
    }

    /// Forget every withheld pane: a full-viewport repaint has just redrawn
    /// them all, so settling them again would repaint what is already correct.
    pub(super) fn clear_pending(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use phux_protocol::ids::TerminalId;

    fn pane(id: u32) -> TerminalId {
        TerminalId::Local { id }
    }

    /// The pacer's central promise: latency is never added to a quiet screen.
    /// The first frame after any lull paints immediately, so a keystroke's
    /// echo reaches the glass exactly as fast as it did before pacing.
    #[test]
    fn the_first_frame_after_a_lull_paints_immediately() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        assert!(pacer.admit(t0), "nothing has painted; paint now");
        // Well past the window: another lull, another immediate paint.
        assert!(
            pacer.admit(t0 + Duration::from_secs(1)),
            "a frame after the window paints immediately"
        );
    }

    /// A frame arriving inside the window is refused, and refusing it is what
    /// makes the settle a SINGLE composited frame rather than one per burst.
    #[test]
    fn frames_inside_the_window_are_refused_until_it_expires() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        assert!(pacer.admit(t0));
        let interval = frame_interval();
        assert!(
            !pacer.admit(t0 + interval / 2),
            "half a window in, the paint is withheld"
        );
        assert!(
            pacer.admit(t0 + interval),
            "the window expiring re-admits exactly once"
        );
    }

    /// Withheld panes deduplicate: a burst that touches the same pane forty
    /// times owes ONE settle for it, not forty repaints of the same rows.
    #[test]
    fn withheld_panes_deduplicate_and_drain_once() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        assert!(pacer.admit(t0));
        for _ in 0..40 {
            pacer.withhold(&pane(1));
        }
        pacer.withhold(&pane(2));
        let owed = pacer.take_pending();
        assert_eq!(owed, vec![pane(1), pane(2)]);
        assert!(pacer.take_pending().is_empty(), "taking the debt clears it");
    }

    /// An idle screen arms no timer at all: with nothing owed the driver's
    /// select loop parks on exactly the wake-up sources it did before.
    #[test]
    fn an_idle_pacer_arms_no_deadline() {
        let mut pacer = PaintPacer::default();
        assert_eq!(pacer.deadline(), None, "nothing owed, nothing armed");
        let t0 = tokio::time::Instant::now();
        assert!(pacer.admit(t0));
        assert_eq!(
            pacer.deadline(),
            None,
            "a frame that painted owes no settle"
        );
        pacer.withhold(&pane(1));
        assert_eq!(
            pacer.deadline(),
            Some(t0 + frame_interval()),
            "a withheld pane arms the window's end"
        );
    }

    /// The felt-latency rule: a frame that answers the user is not paced.
    ///
    /// Measured regression this pins down — p50 echo improved (222us ->
    /// 176us) while p99 went 711us -> 17.4ms and max 2.9ms -> 19.1ms, the
    /// slow keystrokes clustered at one frame interval. A keypress whose echo
    /// happened to arrive just after some unrelated paint opened a window was
    /// treated as unsolicited output and made to wait for a deadline it
    /// should never have been subject to.
    #[test]
    fn input_in_flight_bypasses_a_window_that_would_otherwise_refuse() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        // Some unrelated output paints and opens a window.
        assert!(pacer.admit(t0));
        let mid = t0 + frame_interval() / 2;
        assert!(
            !pacer.admit(mid),
            "unsolicited output inside the window is still paced"
        );
        // The user types. The echo that follows is a REPLY, not a flood.
        pacer.note_input(mid);
        assert!(
            pacer.admit(mid),
            "a frame answering the user must not wait for the window"
        );
    }

    /// The grace covers a reply that arrives as SEVERAL frames.
    ///
    /// This is what the one-shot version got wrong, and it was worth 4.3% of
    /// keystrokes landing in the 8-22ms band against 1.2% with pacing off: a
    /// shell echo and the readline redraw behind it are two frames, and a
    /// flag consumed by the first left the second — the one carrying the
    /// glyph the user is waiting for — to wait out the window.
    #[test]
    fn the_grace_covers_a_reply_split_across_frames() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        pacer.note_input(t0);
        assert!(pacer.admit(t0), "the first frame of the reply paints");
        assert!(
            pacer.admit(t0 + Duration::from_millis(1)),
            "and so does the second, 1ms later, still inside the grace"
        );
        assert!(pacer.admit(t0 + Duration::from_millis(5)), "and the third");
    }

    /// The grace EXPIRES, so a flood never gets a standing exemption from one
    /// keystroke: past the window, unsolicited output is paced again.
    #[test]
    fn the_input_grace_expires_and_pacing_resumes() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        pacer.note_input(t0);
        assert!(pacer.admit(t0), "the reply paints");
        // Past the grace, admission is decided by the window again. The
        // first frame out there opens a fresh one...
        let after_grace = t0 + INPUT_GRACE;
        assert!(
            pacer.admit(after_grace),
            "the grace has lapsed and so has the window opened by the reply"
        );
        // ...and the next frame inside it is refused, which is pacing back in
        // force. One keystroke buys a grace, never a standing exemption.
        assert!(
            !pacer.admit(after_grace + frame_interval() / 2),
            "once the grace lapses the flood is paced again"
        );
    }

    /// `rearm` restarts the window but must NOT cancel the grace: a settle
    /// that lands mid-reply would otherwise make the rest of that reply wait,
    /// which is the exact shape of the bug the grace exists to close.
    #[test]
    fn rearm_restarts_the_window_without_cancelling_the_grace() {
        let mut pacer = PaintPacer::default();
        let t0 = tokio::time::Instant::now();
        pacer.note_input(t0);
        pacer.rearm(t0);
        assert!(
            pacer.admit(t0 + Duration::from_millis(1)),
            "the rest of the reply still bypasses the window"
        );
    }

    /// A full-viewport repaint force-redraws every pane, so it discharges the
    /// debt. Settling afterwards would repaint rows that are already correct.
    #[test]
    fn a_full_repaint_discharges_every_withheld_pane() {
        let mut pacer = PaintPacer::default();
        assert!(pacer.admit(tokio::time::Instant::now()));
        pacer.withhold(&pane(1));
        pacer.withhold(&pane(2));
        pacer.clear_pending();
        assert!(pacer.take_pending().is_empty());
        assert_eq!(pacer.deadline(), None);
    }

    /// The drained work for a level-only raise: no fleet re-projection.
    fn painted(level: RepaintLevel, viewport_was_cleared: bool) -> Repaint {
        Repaint {
            level,
            viewport_was_cleared,
            fleet_dirty: false,
        }
    }

    /// Declaration order IS the cost order, and that is what makes `raise` a
    /// monotone max.
    #[test]
    fn levels_order_none_below_chrome_below_full() {
        assert!(RepaintLevel::None < RepaintLevel::Chrome);
        assert!(RepaintLevel::Chrome < RepaintLevel::Full);
        assert_eq!(RepaintLevel::default(), RepaintLevel::None);
    }

    /// A raise never lowers the level, and is order-independent: `full` then
    /// `chrome` and `chrome` then `full` both drain as `Full`.
    #[test]
    fn raise_is_monotone_and_order_independent() {
        let mut a = RepaintAccumulator::default();
        a.raise_full();
        a.raise_chrome();

        let mut b = RepaintAccumulator::default();
        b.raise_chrome();
        b.raise_full();

        assert_eq!(a, b);
        assert_eq!(a.drain(), painted(RepaintLevel::Full, true));
        assert_eq!(b.drain(), painted(RepaintLevel::Full, true));
    }

    /// Raising is idempotent: twenty `MetadataChanged` frames in one coalesced
    /// batch collapse into ONE `Chrome` paint. This is the whole point of the
    /// accumulator with a live agent detector upstream.
    #[test]
    fn twenty_chrome_raises_collapse_into_one_chrome_drain() {
        let mut accum = RepaintAccumulator::default();
        for _ in 0..20 {
            accum.raise_chrome();
        }
        assert_eq!(accum.drain(), painted(RepaintLevel::Chrome, false));
        // Drained: the next iteration starts clean, so an idle loop pass
        // paints nothing at all.
        assert_eq!(accum.drain(), painted(RepaintLevel::None, false));
    }

    /// A chrome-only iteration must NOT report the viewport as cleared — only
    /// the full path emits `ED2`, and only it may bypass a painter's cache.
    #[test]
    fn chrome_never_reports_a_cleared_viewport() {
        let mut accum = RepaintAccumulator::default();
        accum.raise_chrome();
        let drained = accum.drain();
        assert_eq!(drained.level, RepaintLevel::Chrome);
        assert!(
            !drained.viewport_was_cleared,
            "chrome paints in place; it clears nothing"
        );
    }

    /// The default (no trigger) iteration drains to `None` and paints nothing.
    #[test]
    fn untouched_accumulator_drains_to_none() {
        let mut accum = RepaintAccumulator::default();
        assert_eq!(accum.drain(), painted(RepaintLevel::None, false));
    }

    /// The fleet half of the same collapse. Nine panes publishing a state
    /// transition in one coalesced batch must rebuild the dashboard ONCE, not
    /// nine times — while the dashboard is open, each refresh repaints the
    /// overlay layer over a full-frame base (an `ESC[2J` per refresh), which is
    /// the strobe this accumulator exists to prevent, in precisely the view
    /// that exists for watching agents.
    #[test]
    fn nine_fleet_raises_collapse_into_one_fleet_drain() {
        let mut accum = RepaintAccumulator::default();
        for _ in 0..9 {
            accum.raise_fleet();
        }
        let drained = accum.drain();
        assert!(
            drained.fleet_dirty,
            "nine raises must survive as one refresh"
        );
        assert_eq!(
            drained.level,
            RepaintLevel::None,
            "a fleet refresh is not itself a base-frame repaint level"
        );
        // Drained: the next iteration re-projects nothing.
        assert!(
            !accum.drain().fleet_dirty,
            "the fleet flag must not survive its drain"
        );
    }

    /// A fleet raise composes with a level raise: an agent-state batch that
    /// also dirtied the chrome drains to ONE chrome paint AND ONE fleet
    /// refresh, in one iteration.
    #[test]
    fn fleet_and_level_raises_compose_in_one_drain() {
        let mut accum = RepaintAccumulator::default();
        accum.raise_chrome();
        accum.raise_fleet();
        assert_eq!(
            accum.drain(),
            Repaint {
                level: RepaintLevel::Chrome,
                viewport_was_cleared: false,
                fleet_dirty: true,
            }
        );
    }
}
