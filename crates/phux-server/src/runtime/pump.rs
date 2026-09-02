//! Shared per-generation state for the pane output pumps.
//!
//! Two pumps forward one pane's broadcast output to one consumer: the ATTACH
//! pump in [`crate::runtime::attach`] and the `ATTACH_TERMINAL` pump in
//! [`crate::runtime::commands`]. They differ in how they publish a bootstrap
//! and in what they do when they fail, but the rules for *what may go on the
//! wire right now* are identical — and when they were written twice, only one
//! copy got the gap fence (phux-l96p.10), so the same client-killing sequence
//! gap stayed live on the `rec` / `play` / headless / FFI path after it was
//! fixed for interactive attach.
//!
//! [`PumpGeneration`] is that shared rule set, and it keeps `generation_active`
//! and `gap_pending` **private**: a pump cannot put a live delta on the wire
//! without asking [`PumpGeneration::forwards`], so a third pump cannot
//! reintroduce the bug by forgetting a flag it never sees.

use std::time::Duration;

use phux_protocol::ids::BootstrapId;

use crate::terminal_actor::PaneOutput;

/// How long a fenced pump waits for the replacement generation before asking
/// for it again.
///
/// An order of magnitude above the actor's `RESIZE_RESYNC_DEBOUNCE`, so a
/// resync that is merely coalescing is never mistaken for one that was lost.
pub(super) const GAP_RESYNC_RETRY: Duration = Duration::from_millis(500);

/// Where one pump has got to inside the generation it is publishing, and
/// whether that generation may still carry live output.
#[derive(Debug)]
pub(super) struct PumpGeneration {
    /// Highest raw sequence already covered by the published bootstrap.
    published_cut: u64,
    /// Highest raw sequence actually forwarded to the consumer.
    last_forwarded_seq: u64,
    /// Generation every frame this pump emits is labelled with.
    bootstrap_id: BootstrapId,
    /// Cleared by a tombstone and set again once a replacement bootstrap is
    /// published; nothing may be forwarded in between.
    generation_active: bool,
    /// Set the moment the broadcast drops a window under this pump, cleared
    /// when the replacement generation is published.
    ///
    /// While it is set the pump forwards nothing. Two things depend on that.
    /// First, the consumer's mirror is exactly sequenced: a `TERMINAL_OUTPUT`
    /// whose `seq` skips the dropped window is a `SequenceGap`, which the
    /// client kernel treats as a protocol error and detaches on — so
    /// forwarding "the rest" after a gap does not degrade the session, it ends
    /// it. Second, a pump that keeps awaiting mailbox capacity for frames the
    /// consumer cannot use drains the broadcast at the *consumer's* speed, and
    /// the in-band resync it just asked for is delivered on that same
    /// broadcast: at PTY speed the resync is overwritten before the pump
    /// reaches it, and the next lag re-arms the same trap. Dropping instead of
    /// queueing lets the pump drain at memory speed, so the resync always
    /// arrives. This is tmux's rule — a consumer far enough behind gets one
    /// fresh screen, not a replay of everything it missed.
    gap_pending: bool,
}

impl PumpGeneration {
    /// Start at the cut the publication gate handed over.
    pub(super) const fn opened_at(published_cut: u64, bootstrap_id: BootstrapId) -> Self {
        Self {
            published_cut,
            last_forwarded_seq: published_cut,
            bootstrap_id,
            generation_active: true,
            gap_pending: false,
        }
    }

    /// The generation label every frame this pump emits carries.
    pub(super) const fn bootstrap_id(&self) -> BootstrapId {
        self.bootstrap_id
    }

    /// Adopt the next generation label ahead of republishing.
    pub(super) const fn set_bootstrap_id(&mut self, bootstrap_id: BootstrapId) {
        self.bootstrap_id = bootstrap_id;
    }

    /// Highest raw sequence actually forwarded, for a tombstone's
    /// `last_valid_seq`.
    pub(super) const fn last_forwarded_seq(&self) -> u64 {
        self.last_forwarded_seq
    }

    /// Is a generation currently published and unretired?
    pub(super) const fn is_active(&self) -> bool {
        self.generation_active
    }

    /// Is this pump waiting on a replacement generation after a gap?
    pub(super) const fn is_fenced(&self) -> bool {
        self.gap_pending
    }

    /// May this live delta go on the wire?
    ///
    /// The single gate every pump's live-forward path must pass through. A
    /// retired generation and a fenced one both answer `false`, as does a
    /// sequence the published bootstrap already covers.
    pub(super) const fn forwards(&self, seq: u64) -> bool {
        self.generation_active && !self.gap_pending && seq > self.published_cut
    }

    /// Record a delta that reached the consumer.
    pub(super) const fn note_forwarded(&mut self, seq: u64) {
        self.last_forwarded_seq = seq;
    }

    /// A tombstone retired the published generation; nothing may be forwarded
    /// until a replacement is published.
    pub(super) const fn retire(&mut self) {
        self.generation_active = false;
    }

    /// The broadcast dropped a window under this pump: fence the generation
    /// before asking for a resync.
    ///
    /// Returns whether a resync was *already* in flight, which distinguishes a
    /// fresh gap (worth a `WARN`) from a repeat while fenced.
    pub(super) const fn fence_for_gap(&mut self) -> bool {
        let already_pending = self.gap_pending;
        self.gap_pending = true;
        already_pending
    }

    /// A replacement generation is published at `base_seq`: unfence, reactivate
    /// and re-anchor the sequence expectation.
    pub(super) const fn republished_at(&mut self, base_seq: u64) {
        self.published_cut = base_seq;
        self.last_forwarded_seq = base_seq;
        self.generation_active = true;
        self.gap_pending = false;
    }
}

/// The next broadcast event for a pump, or `None` when a fenced pump has
/// waited [`GAP_RESYNC_RETRY`] without one.
///
/// A pump that is not fenced simply awaits the broadcast. A fenced one bounds
/// that wait: it is forwarding nothing until the replacement generation lands,
/// so without a bound a resync that never arrived would be indistinguishable
/// from a pane with nothing to say. `None` is the caller's cue to ask again.
///
/// # Cancel safety
///
/// `broadcast::Receiver::recv` is cancel-safe, so the bounded wait cannot drop
/// a message it had already taken.
pub(super) async fn next_event(
    generation: &PumpGeneration,
    output_rx: &mut tokio::sync::broadcast::Receiver<PaneOutput>,
) -> Option<Result<PaneOutput, tokio::sync::broadcast::error::RecvError>> {
    if !generation.is_fenced() {
        return Some(output_rx.recv().await);
    }
    (tokio::time::timeout(GAP_RESYNC_RETRY, output_rx.recv()).await).ok()
}

#[cfg(test)]
mod tests {
    use super::{GAP_RESYNC_RETRY, PumpGeneration};

    fn bootstrap(raw: u64) -> phux_protocol::ids::BootstrapId {
        phux_protocol::ids::BootstrapId::new(raw).expect("non-zero bootstrap id")
    }

    fn opened() -> PumpGeneration {
        PumpGeneration::opened_at(41, bootstrap(7))
    }

    #[test]
    fn a_fresh_generation_forwards_only_past_its_cut() {
        let generation = opened();
        assert!(
            !generation.forwards(41),
            "the cut itself is already covered"
        );
        assert!(generation.forwards(42));
        assert!(!generation.is_fenced());
    }

    #[test]
    fn a_gap_fences_every_live_delta_until_the_replacement_lands() {
        let mut generation = opened();
        assert!(!generation.fence_for_gap(), "first gap is not a repeat");
        assert!(generation.is_fenced());
        // This is the frame that used to detach the client.
        assert!(!generation.forwards(20_533));
        assert!(generation.fence_for_gap(), "second gap is a repeat");

        generation.republished_at(9_000);
        assert!(!generation.is_fenced());
        assert!(!generation.forwards(9_000));
        assert!(generation.forwards(9_001));
    }

    #[test]
    fn a_retired_generation_forwards_nothing_even_unfenced() {
        let mut generation = opened();
        generation.retire();
        assert!(!generation.is_active());
        assert!(!generation.forwards(42));
        generation.republished_at(100);
        assert!(generation.is_active());
        assert!(generation.forwards(101));
    }

    #[test]
    fn the_retry_window_sits_well_above_the_actor_resync_debounce() {
        assert!(
            GAP_RESYNC_RETRY >= crate::terminal_actor::RESIZE_RESYNC_DEBOUNCE * 4,
            "a resync that is merely coalescing must not look like one that was lost",
        );
    }
}
