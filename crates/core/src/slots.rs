//! Campaign slot switching.
//!
//! Transaction state machine and strategies per `docs/design/slot-manager.md`.
//! Copy strategy completes in M1; junction strategy in M2.

use crate::layout::SlotId;

/// Which package revision a slot points at. `None` = plain campaign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotState {
    pub slot: SlotId,
    /// Package id + content revision, when a custom campaign is active.
    pub active: Option<(String, String)>,
}

/// Phases of a switch transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchPhase {
    Staging,
    Verified,
    Committed,
    RolledBack,
}
