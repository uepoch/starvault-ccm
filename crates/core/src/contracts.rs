//! Core-owned response types shared with the desktop shell.

use serde::{Deserialize, Serialize};

use crate::identity::PackageId;
use crate::layout::SlotId;
use crate::library::LibraryEntry;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveCampaign {
    pub id: PackageId,
    pub revision: String,
    pub faction: SlotId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Ready,
    Drifted,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthIssue {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub repairable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub state: HealthState,
    pub issues: Vec<HealthIssue>,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            state: HealthState::Ready,
            issues: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibrarySnapshot {
    pub entries: Vec<LibraryEntry>,
    pub active_campaign: Option<ActiveCampaign>,
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartupReport {
    pub library: LibrarySnapshot,
    pub recovery_performed: bool,
    pub notes: Vec<String>,
}
