//! Bidirectional Spilman channel pair + interval netting.
//!
//! Each pair of TollGate peers maintains **two unidirectional Spilman channels**
//! — one per delivery direction. This module implements the orchestration that
//! makes a pair behave as a single bidirectional payment relationship:
//!
//! - [`ChannelPair`] tracks both channel ids (outbound where the local peer is
//!   sender/funder, inbound where the local peer is receiver) and the rolling
//!   metering baseline needed to compute per-interval deltas.
//! - [`compute_net_settlement`] is the **netting engine**: given the cumulative
//!   metering snapshots from the previous and current interval plus the two
//!   peers' prices, it deterministically decides who owes whom and how much.
//!   Only the **net debtor** signs a single [`NetSettlement::LocalOwes`] or
//!   [`NetSettlement::RemoteOwes`] balance update per interval; the other
//!   channel's balance does not move.
//!
//! This is the "mesh-native" mode described in
//! `docs/design/core/tollgate-payment-channels.md` §Netting: traffic flows both
//! ways between adjacent TollGate routers and the payments net out, so channels
//! drain at the difference rate and last far longer before rollover.
//!
//! ## Netting math (from the local peer's perspective)
//!
//! ```text
//! remote_owes_local = Δdelivered × local_price   // local produced, remote consumed
//! local_owes_remote = Δreceived  × remote_price  // local consumed,  remote produced
//! net              = remote_owes_local − local_owes_remote
//! ```
//!
//! `local_price` / `remote_price` are the per-unit **producer prices** — the
//! price each peer charges the consumer of its delivery. Netting uses the
//! *interval delta* of cumulative metering (never the absolute cumulative
//! values), so it is independent of when the channel was opened.
//!
//! The module is intentionally synchronous and free of `cdk_spilman` types so
//! the netting logic can be unit-tested with no mint, no network, and no async
//! runtime. The [`crate::spilman_service::SpilmanService`] is driven by the
//! caller: `ChannelPair` decides *what* to settle, the caller signs it.

use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the netting engine.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NettingError {
    /// A cumulative metering counter decreased between intervals. Cumulative
    /// counters must be monotonic non-decreasing; a regression indicates a bug,
    /// a replay, or a counter reset mid-session.
    #[error("metering went backwards: {field} {prev} -> {cur}")]
    MeteringWentBackwards {
        /// Which cumulative counter regressed.
        field: &'static str,
        /// Previous-interval cumulative value.
        prev: u64,
        /// Current-interval cumulative value.
        cur: u64,
    },

    /// An operation was attempted on a channel that has not been opened yet.
    /// `direction` names which half of the pair ("outbound" / "inbound").
    #[error("channel not opened yet: {direction}")]
    ChannelNotOpened {
        /// Which half of the pair is missing: "outbound" or "inbound".
        direction: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Metering snapshot
// ---------------------------------------------------------------------------

/// One interval's cumulative metering view for a single peer, used as input to
/// the netting engine.
///
/// Both fields are **cumulative since session start** (matching the protocol's
/// [`MeteringReport`]); the netting engine takes their delta between intervals.
/// "From the local peer's viewpoint" means: `delivered_to_remote` is how much
/// resource the local peer pushed toward the remote, `received_from_remote` is
/// how much the local peer pulled in from the remote.
///
/// [`MeteringReport`]: tollgate_core::protocol::MeteringReport
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IntervalMetering {
    /// Cumulative units the local peer delivered *to* the remote (local
    /// produced, remote consumed). Drains the remote peer's wallet at
    /// `local_price`.
    pub delivered_to_remote: u64,
    /// Cumulative units the local peer received *from* the remote (local
    /// consumed, remote produced). Drains the local peer's wallet at
    /// `remote_price`.
    pub received_from_remote: u64,
}

impl IntervalMetering {
    /// Build a snapshot from the two cumulative counters.
    #[must_use]
    pub fn new(delivered_to_remote: u64, received_from_remote: u64) -> Self {
        Self {
            delivered_to_remote,
            received_from_remote,
        }
    }
}

// ---------------------------------------------------------------------------
// Net settlement
// ---------------------------------------------------------------------------

/// Which channel carries this interval's settlement, and for how much.
///
/// Only one variant is ever active per interval: the net debtor signs a single
/// balance update and the counterparty merely acks. This is the "only the net
/// loser pays" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetSettlement {
    /// The **local** peer is the net debtor by `amount`. The caller must sign a
    /// balance update on the **outbound** channel (where local is sender) for
    /// `amount`, increasing its cumulative balance by `amount`.
    LocalOwes(u64),
    /// The **remote** peer is the net debtor by `amount`. The caller should
    /// *expect* a balance update on the **inbound** channel (where local is
    /// receiver) for `amount`; the remote signs it.
    RemoteOwes(u64),
    /// Exactly even this interval — no balance update on either channel.
    Even,
}

impl NetSettlement {
    /// The absolute sats that move this interval, regardless of direction.
    #[must_use]
    pub fn amount(self) -> u64 {
        match self {
            Self::LocalOwes(a) | Self::RemoteOwes(a) => a,
            Self::Even => 0,
        }
    }

    /// `true` if no settlement is needed this interval.
    #[must_use]
    pub fn is_even(self) -> bool {
        matches!(self, Self::Even)
    }
}

// ---------------------------------------------------------------------------
// Netting engine (pure)
// ---------------------------------------------------------------------------

/// Compute the per-interval net settlement between two peers.
///
/// Both metering snapshots are cumulative-since-session-start; the engine takes
/// the delta between them. `local_price` is the sats/unit the local peer
/// charges for its own delivery (producer price); `remote_price` is the same for
/// the remote peer.
///
/// # Algorithm
///
/// 1. `remote_owes_local = (cur.delivered - prev.delivered) × local_price`
///    — local pushed this many units to the remote, who consumed them.
/// 2. `local_owes_remote = (cur.received - prev.received) × remote_price`
///    — local pulled this many units from the remote, who produced them.
/// 3. The **net debtor** pays the absolute difference on their channel.
///
/// # Errors
///
/// Returns [`NettingError::MeteringWentBackwards`] if either cumulative counter
/// decreased between `prev` and `cur`.
///
/// # Example
///
/// ```
/// use tollgate_net::spilman_channel_pair::{
///     compute_net_settlement, IntervalMetering, NetSettlement,
/// };
///
/// // Interval: local delivered 10 (at local_price 1), received 3 (at remote_price 1).
/// // remote_owes_local = 10, local_owes_remote = 3 → local is net creditor →
/// // remote owes local 7.
/// let prev = IntervalMetering::new(0, 0);
/// let cur = IntervalMetering::new(10, 3);
/// assert_eq!(
///     compute_net_settlement(&prev, &cur, 1, 1),
///     Ok(NetSettlement::RemoteOwes(7))
/// );
/// ```
#[allow(clippy::missing_errors_doc)]
pub fn compute_net_settlement(
    prev: &IntervalMetering,
    cur: &IntervalMetering,
    local_price: u64,
    remote_price: u64,
) -> Result<NetSettlement, NettingError> {
    let d_delivered = cur
        .delivered_to_remote
        .checked_sub(prev.delivered_to_remote)
        .ok_or(NettingError::MeteringWentBackwards {
            field: "delivered_to_remote",
            prev: prev.delivered_to_remote,
            cur: cur.delivered_to_remote,
        })?;
    let d_received = cur
        .received_from_remote
        .checked_sub(prev.received_from_remote)
        .ok_or(NettingError::MeteringWentBackwards {
            field: "received_from_remote",
            prev: prev.received_from_remote,
            cur: cur.received_from_remote,
        })?;

    // remote consumed local's delivery → pays local's price.
    let remote_owes_local = d_delivered.saturating_mul(local_price);
    // local consumed remote's delivery → pays remote's price.
    let local_owes_remote = d_received.saturating_mul(remote_price);

    if local_owes_remote > remote_owes_local {
        Ok(NetSettlement::LocalOwes(
            local_owes_remote - remote_owes_local,
        ))
    } else if remote_owes_local > local_owes_remote {
        Ok(NetSettlement::RemoteOwes(
            remote_owes_local - local_owes_remote,
        ))
    } else {
        Ok(NetSettlement::Even)
    }
}

// ---------------------------------------------------------------------------
// Channel direction
// ---------------------------------------------------------------------------

/// Which half of a [`ChannelPair`] a channel id refers to, relative to the
/// local peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDirection {
    /// Local is the sender/funder — pays the remote.
    Outbound,
    /// Local is the receiver — the remote pays local.
    Inbound,
}

impl ChannelDirection {
    /// Human-readable name used in error messages ("outbound" / "inbound").
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
        }
    }
}

// ---------------------------------------------------------------------------
// ChannelPair — bidirectional coordinator
// ---------------------------------------------------------------------------

/// A bidirectional Spilman channel relationship with one peer.
///
/// Holds the two unidirectional channel ids (outbound where local is sender,
/// inbound where local is receiver), the rolling metering baseline, and both
/// peers' prices. Call [`ChannelPair::settle_interval`] each metering tick to
/// find out who owes whom; then the caller drives the [`SpilmanService`] to sign
/// the resulting balance update on the right channel.
///
/// This type owns **only** the coordination state — the actual on-chain
/// Spilman balance lives in the `cdk_spilman` bridges. Keeping the coordinator
/// decoupled from the async/mint layer makes the netting logic trivially
/// unit-testable.
///
/// [`SpilmanService`]: crate::spilman_service::SpilmanService
#[derive(Debug, Clone)]
pub struct ChannelPair {
    /// Channel id where the local peer is sender (local → remote). Set once the
    /// outbound channel is opened.
    outbound_channel_id: Option<String>,
    /// Cumulative balance the local peer has signed on the outbound channel.
    /// Incremented by each `LocalOwes` settlement.
    outbound_cumulative: u64,
    /// Channel id where the local peer is receiver (remote → local). Set once
    /// the remote opens its channel.
    inbound_channel_id: Option<String>,
    /// Cumulative balance the remote peer has signed on the inbound channel, as
    /// seen/verified by the local receiver. Incremented by each accepted
    /// `RemoteOwes` settlement.
    inbound_cumulative: u64,
    /// Cumulative metering at the last settled interval. Baseline for the next
    /// delta computation.
    last_metering: IntervalMetering,
    /// Sats/unit the local peer charges for its delivery (producer price).
    local_price: u64,
    /// Sats/unit the remote peer charges for its delivery (producer price).
    remote_price: u64,
}

impl ChannelPair {
    /// Create a new channel pair with the given producer prices and a zero
    /// metering baseline (session just starting).
    #[must_use]
    pub fn new(local_price: u64, remote_price: u64) -> Self {
        Self {
            outbound_channel_id: None,
            outbound_cumulative: 0,
            inbound_channel_id: None,
            inbound_cumulative: 0,
            last_metering: IntervalMetering::default(),
            local_price,
            remote_price,
        }
    }

    /// Record the outbound channel id once it has been opened by the local peer.
    pub fn set_outbound_channel(&mut self, channel_id: impl Into<String>) {
        self.outbound_channel_id = Some(channel_id.into());
    }

    /// Record the inbound channel id once the remote peer opens their channel
    /// (typically observed on the first inbound `BalanceUpdate`).
    pub fn set_inbound_channel(&mut self, channel_id: impl Into<String>) {
        self.inbound_channel_id = Some(channel_id.into());
    }

    /// The outbound channel id, if opened.
    #[must_use]
    pub fn outbound_channel_id(&self) -> Option<&str> {
        self.outbound_channel_id.as_deref()
    }

    /// The inbound channel id, if the remote has opened it.
    #[must_use]
    pub fn inbound_channel_id(&self) -> Option<&str> {
        self.inbound_channel_id.as_deref()
    }

    /// Cumulative balance the local peer has signed on the outbound channel.
    #[must_use]
    pub fn outbound_cumulative(&self) -> u64 {
        self.outbound_cumulative
    }

    /// Cumulative balance the remote peer has signed on the inbound channel.
    #[must_use]
    pub fn inbound_cumulative(&self) -> u64 {
        self.inbound_cumulative
    }

    /// Whether a given channel id is the outbound or inbound half of this pair.
    ///
    /// Returns `None` if the id matches neither half.
    #[must_use]
    pub fn direction_of(&self, channel_id: &str) -> Option<ChannelDirection> {
        if self.outbound_channel_id.as_deref() == Some(channel_id) {
            Some(ChannelDirection::Outbound)
        } else if self.inbound_channel_id.as_deref() == Some(channel_id) {
            Some(ChannelDirection::Inbound)
        } else {
            None
        }
    }

    /// Compute the net settlement for the current interval and advance the
    /// internal metering baseline.
    ///
    /// On [`NetSettlement::LocalOwes`], the caller must sign a balance update on
    /// the outbound channel; [`ChannelPair`] bumps `outbound_cumulative` by the
    /// net amount so subsequent intervals build on the new running balance. On
    /// [`NetSettlement::RemoteOwes`], the caller should expect the remote to
    /// sign on the inbound channel; `inbound_cumulative` is bumped to mirror the
    /// running balance the local receiver will verify. On [`NetSettlement::Even`]
    /// nothing changes.
    ///
    /// # Errors
    ///
    /// Returns [`NettingError::MeteringWentBackwards`] if `metering` regressed.
    #[allow(clippy::missing_errors_doc)]
    pub fn settle_interval(
        &mut self,
        metering: IntervalMetering,
    ) -> Result<NetSettlement, NettingError> {
        let settlement = compute_net_settlement(
            &self.last_metering,
            &metering,
            self.local_price,
            self.remote_price,
        )?;
        match settlement {
            NetSettlement::LocalOwes(amt) => {
                self.outbound_cumulative = self.outbound_cumulative.saturating_add(amt)
            }
            NetSettlement::RemoteOwes(amt) => {
                self.inbound_cumulative = self.inbound_cumulative.saturating_add(amt)
            }
            NetSettlement::Even => {}
        }
        // Advance the baseline regardless of outcome — the interval is settled.
        self.last_metering = metering;
        Ok(settlement)
    }

    /// Reset the metering baseline to a new starting point without producing a
    /// settlement (e.g. after a session renegotiation where metering counters
    /// legitimately restart). Does not touch cumulative balances.
    pub fn reset_baseline(&mut self, metering: IntervalMetering) {
        self.last_metering = metering;
    }

    /// Require the outbound channel to be open, else return a typed error.
    ///
    /// # Errors
    ///
    /// [`NettingError::ChannelNotOpened`] with `direction = "outbound"`.
    #[allow(clippy::missing_errors_doc)]
    pub fn require_outbound(&self) -> Result<&str, NettingError> {
        self.outbound_channel_id
            .as_deref()
            .ok_or(NettingError::ChannelNotOpened {
                direction: "outbound",
            })
    }

    /// Require the inbound channel to be open, else return a typed error.
    ///
    /// # Errors
    ///
    /// [`NettingError::ChannelNotOpened`] with `direction = "inbound"`.
    #[allow(clippy::missing_errors_doc)]
    pub fn require_inbound(&self) -> Result<&str, NettingError> {
        self.inbound_channel_id
            .as_deref()
            .ok_or(NettingError::ChannelNotOpened {
                direction: "inbound",
            })
    }
}

// ---------------------------------------------------------------------------
// Unit tests (no network, no mint, no async)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- compute_net_settlement: pure engine -------------------------------

    #[test]
    fn netting_local_is_net_debtor() {
        // Local received 5 from remote (at remote_price 2) = owes 10.
        // Local delivered 2 to remote (at local_price 1) = remote owes 2.
        // Net: local owes 8 → LocalOwes(8).
        let prev = IntervalMetering::new(0, 0);
        let cur = IntervalMetering::new(2, 5);
        assert_eq!(
            compute_net_settlement(&prev, &cur, 1, 2),
            Ok(NetSettlement::LocalOwes(8))
        );
    }

    #[test]
    fn netting_remote_is_net_debtor() {
        // Local delivered 10 (local_price 1) = remote owes 10.
        // Local received 3 (remote_price 1) = local owes 3.
        // Net: remote owes 7 → RemoteOwes(7).
        let prev = IntervalMetering::new(0, 0);
        let cur = IntervalMetering::new(10, 3);
        assert_eq!(
            compute_net_settlement(&prev, &cur, 1, 1),
            Ok(NetSettlement::RemoteOwes(7))
        );
    }

    #[test]
    fn netting_exactly_even_no_update() {
        // Symmetric: delivered 5 @1 = remote owes 5; received 5 @1 = local owes 5.
        let prev = IntervalMetering::new(0, 0);
        let cur = IntervalMetering::new(5, 5);
        assert_eq!(
            compute_net_settlement(&prev, &cur, 1, 1),
            Ok(NetSettlement::Even)
        );
        assert!(NetSettlement::Even.is_even());
        assert_eq!(NetSettlement::Even.amount(), 0);
    }

    #[test]
    fn netting_uses_interval_delta_not_absolute() {
        // A mid-session interval: previous cumulative was (40, 20), now (42, 25).
        // delta delivered = 2 @ local_price 3 = remote owes 6.
        // delta received  = 5 @ remote_price 1 = local owes 5.
        // Net: remote owes 1.
        let prev = IntervalMetering::new(40, 20);
        let cur = IntervalMetering::new(42, 25);
        assert_eq!(
            compute_net_settlement(&prev, &cur, 3, 1),
            Ok(NetSettlement::RemoteOwes(1))
        );
    }

    #[test]
    fn netting_asymmetric_prices_match_design_doc_example() {
        // Design doc interval-netting example:
        //   A owes B: units B delivered to A × B's price = 2 sats
        //   B owes A: units A delivered to B × A's price = 5 sats
        //   Net: B owes A 3 sats
        // From LOCAL=A's viewpoint: A delivered 5 (local_price 1) → B owes 5;
        // A received 2 (remote_price 1) → A owes 2. Net: B (remote) owes A 3.
        let prev = IntervalMetering::new(0, 0);
        let cur = IntervalMetering::new(5, 2);
        assert_eq!(
            compute_net_settlement(&prev, &cur, 1, 1),
            Ok(NetSettlement::RemoteOwes(3))
        );
    }

    #[test]
    fn netting_amount_and_is_even_accessors() {
        assert_eq!(NetSettlement::LocalOwes(42).amount(), 42);
        assert!(!NetSettlement::LocalOwes(42).is_even());
        assert_eq!(NetSettlement::RemoteOwes(7).amount(), 7);
        assert!(!NetSettlement::RemoteOwes(7).is_even());
    }

    #[test]
    fn netting_zero_delta_is_even() {
        // No traffic either direction this interval.
        let baseline = IntervalMetering::new(100, 100);
        assert_eq!(
            compute_net_settlement(&baseline, &baseline, 9, 9),
            Ok(NetSettlement::Even)
        );
    }

    #[test]
    fn netting_rejects_backwards_delivered() {
        let prev = IntervalMetering::new(10, 0);
        let cur = IntervalMetering::new(5, 0); // delivered went 10 → 5
        let err = compute_net_settlement(&prev, &cur, 1, 1).expect_err("must error");
        assert_eq!(
            err,
            NettingError::MeteringWentBackwards {
                field: "delivered_to_remote",
                prev: 10,
                cur: 5,
            }
        );
        assert!(err.to_string().contains("delivered_to_remote"));
    }

    #[test]
    fn netting_rejects_backwards_received() {
        let prev = IntervalMetering::new(0, 10);
        let cur = IntervalMetering::new(0, 3); // received went 10 → 3
        let err = compute_net_settlement(&prev, &cur, 1, 1).expect_err("must error");
        assert_eq!(
            err,
            NettingError::MeteringWentBackwards {
                field: "received_from_remote",
                prev: 10,
                cur: 3,
            }
        );
    }

    #[test]
    fn netting_saturates_on_huge_products() {
        // u64 overflow path: huge delta × price must saturate, not panic.
        let prev = IntervalMetering::new(0, 0);
        let cur = IntervalMetering::new(u64::MAX, u64::MAX);
        // Both saturate to u64::MAX → equal → Even.
        assert_eq!(
            compute_net_settlement(&prev, &cur, 1, 1),
            Ok(NetSettlement::Even)
        );
    }

    // -- ChannelPair coordinator -------------------------------------------

    #[test]
    fn pair_records_both_channel_ids_and_directions() {
        let mut pair = ChannelPair::new(1, 1);
        assert!(pair.outbound_channel_id().is_none());
        assert!(pair.inbound_channel_id().is_none());

        pair.set_outbound_channel("chan-ab-123");
        pair.set_inbound_channel("chan-ba-456");

        assert_eq!(pair.outbound_channel_id(), Some("chan-ab-123"));
        assert_eq!(pair.inbound_channel_id(), Some("chan-ba-456"));

        assert_eq!(
            pair.direction_of("chan-ab-123"),
            Some(ChannelDirection::Outbound)
        );
        assert_eq!(
            pair.direction_of("chan-ba-456"),
            Some(ChannelDirection::Inbound)
        );
        assert_eq!(pair.direction_of("unknown"), None);
        assert_eq!(ChannelDirection::Outbound.as_str(), "outbound");
        assert_eq!(ChannelDirection::Inbound.as_str(), "inbound");
    }

    #[test]
    fn pair_local_owes_advances_outbound_balance() {
        let mut pair = ChannelPair::new(1, 2);
        pair.set_outbound_channel("ab");
        pair.set_inbound_channel("ba");

        // Interval 1: local received 5 @2 (=10), delivered 2 @1 (=2) → owes 8.
        let s1 = pair.settle_interval(IntervalMetering::new(2, 5)).unwrap();
        assert_eq!(s1, NetSettlement::LocalOwes(8));
        assert_eq!(pair.outbound_cumulative(), 8);
        assert_eq!(pair.inbound_cumulative(), 0);

        // Interval 2: local received 3 @2 (=6), delivered 1 @1 (=1) → owes 5.
        // Outbound running balance: 8 + 5 = 13.
        let s2 = pair.settle_interval(IntervalMetering::new(3, 8)).unwrap();
        assert_eq!(s2, NetSettlement::LocalOwes(5));
        assert_eq!(pair.outbound_cumulative(), 13);
        assert_eq!(pair.inbound_cumulative(), 0);
    }

    #[test]
    fn pair_remote_owes_advances_inbound_balance() {
        let mut pair = ChannelPair::new(1, 1);
        pair.set_outbound_channel("ab");
        pair.set_inbound_channel("ba");

        // delivered 10 @1 = remote owes 10; received 3 @1 = local owes 3 → owes 7.
        let s = pair.settle_interval(IntervalMetering::new(10, 3)).unwrap();
        assert_eq!(s, NetSettlement::RemoteOwes(7));
        assert_eq!(pair.inbound_cumulative(), 7);
        assert_eq!(pair.outbound_cumulative(), 0);
    }

    #[test]
    fn pair_alternating_directions_each_interval() {
        // Prove a real mesh scenario: each interval the debtor flips.
        let mut pair = ChannelPair::new(1, 1);
        pair.set_outbound_channel("ab");
        pair.set_inbound_channel("ba");

        // I1: delivered 8, received 2 → remote owes 6.
        assert_eq!(
            pair.settle_interval(IntervalMetering::new(8, 2)).unwrap(),
            NetSettlement::RemoteOwes(6)
        );
        assert_eq!(pair.inbound_cumulative(), 6);
        assert_eq!(pair.outbound_cumulative(), 0);

        // I2: delivered 1, received 9 → local owes 8.
        assert_eq!(
            pair.settle_interval(IntervalMetering::new(9, 11)).unwrap(),
            NetSettlement::LocalOwes(8)
        );
        assert_eq!(pair.inbound_cumulative(), 6); // unchanged
        assert_eq!(pair.outbound_cumulative(), 8);

        // I3: balanced → even, neither balance moves.
        assert_eq!(
            pair.settle_interval(IntervalMetering::new(14, 16)).unwrap(),
            NetSettlement::Even
        );
        assert_eq!(pair.inbound_cumulative(), 6);
        assert_eq!(pair.outbound_cumulative(), 8);
    }

    #[test]
    fn pair_settle_interval_propagates_backwards_error() {
        let mut pair = ChannelPair::new(1, 1);
        // First settle establishes baseline (0,0).
        pair.settle_interval(IntervalMetering::new(10, 10)).unwrap();
        // Regress delivered → error, baseline must NOT advance.
        let err = pair
            .settle_interval(IntervalMetering::new(5, 12))
            .expect_err("regression must error");
        assert_eq!(
            err,
            NettingError::MeteringWentBackwards {
                field: "delivered_to_remote",
                prev: 10,
                cur: 5,
            }
        );
        // Next valid settle still computes off the (10,10) baseline, proving the
        // failed interval did not corrupt state.
        let s = pair.settle_interval(IntervalMetering::new(12, 12)).unwrap();
        assert_eq!(s, NetSettlement::Even); // Δ delivered=2, Δ received=2, prices both 1 → net 0
    }

    #[test]
    fn pair_require_helpers_error_when_unopened() {
        let pair = ChannelPair::new(1, 1);
        let err = pair.require_outbound().unwrap_err();
        assert_eq!(
            err,
            NettingError::ChannelNotOpened {
                direction: "outbound"
            }
        );
        let err = pair.require_inbound().unwrap_err();
        assert_eq!(
            err,
            NettingError::ChannelNotOpened {
                direction: "inbound"
            }
        );
    }

    #[test]
    fn pair_reset_baseline_without_settlement() {
        let mut pair = ChannelPair::new(1, 1);
        pair.set_outbound_channel("ab");
        // Establish a baseline.
        pair.settle_interval(IntervalMetering::new(50, 50)).unwrap();
        // Renegotiation: counters legitimately restart at zero.
        pair.reset_baseline(IntervalMetering::new(0, 0));
        // Now a delivered-only interval settles against the reset baseline.
        let s = pair.settle_interval(IntervalMetering::new(4, 0)).unwrap();
        assert_eq!(s, NetSettlement::RemoteOwes(4));
    }

    #[test]
    fn pair_new_starts_with_zero_baseline_and_prices() {
        let mut pair = ChannelPair::new(7, 3);
        assert_eq!(pair.outbound_cumulative(), 0);
        assert_eq!(pair.inbound_cumulative(), 0);
        let s = pair.settle_interval(IntervalMetering::default()).unwrap();
        assert_eq!(s, NetSettlement::Even);
    }
}
