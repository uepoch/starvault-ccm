//! Journaled application workflows for the single global campaign.
//!
//! Filesystem primitives stage and swap individual resources. This module is
//! the only place that sequences saves, campaign slots, Mods, and the SQLite
//! ledger as one recoverable operation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::StrategyChoice;
use crate::contracts::{
    ActiveCampaign, Health, HealthIssue, HealthState, LibrarySnapshot, StartupReport,
};
use crate::error::{internal_err, package_err, user_err, EnvironmentError, Error, Result};
use crate::identity::PackageId;
use crate::layout::WindowsLayout;
use crate::mods::{self, ExternalModsPolicy, PreparedModsTransition};
use crate::operation::{
    OperationKind, OperationPaths, OperationPhase, PendingOperation, SlotOperationJournal,
    SlotOperationPaths,
};
use crate::saves::{PreparedSaveTransition, SaveOwner, SaveTransition, SavesManager};
use crate::slots::{self, SlotManager};
use crate::store::{PackageManifest, Store};

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

/// Deterministic interruption points used by core crash-recovery tests.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePoint {
    Preparing,
    SavesPrepared,
    SlotsPrepared,
    ModsPrepared,
    Prepared,
    SavesSwapped,
    SlotsSwapped,
    ModsSwapped,
    LedgerCommittedBeforeJournal,
    LedgerCommitted,
    RollbackModsRestored,
    RollbackSlotsRestored,
    RollbackSavesRestored,
    RollbackVerified,
}

/// Core application workflow. The desktop shell must hold its process-wide
/// mutation mutex for every mutating method on this type.
pub struct Workflow<'a> {
    layout: &'a WindowsLayout,
    store: &'a Store,
    strategy: Option<StrategyChoice>,
    external_mods_policy: ExternalModsPolicy,
    saves: Option<SavesManager>,
    save_isolation_expected: bool,
    running_probe: Arc<dyn Fn() -> bool + Send + Sync>,
    verification_probe: Option<Arc<dyn Fn() + Send + Sync>>,
    rollback_pre_mutation_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    fail_after: Option<FailurePoint>,
}

impl<'a> Workflow<'a> {
    pub fn new(layout: &'a WindowsLayout, store: &'a Store) -> Self {
        Self {
            layout,
            store,
            strategy: None,
            external_mods_policy: ExternalModsPolicy::Reject,
            saves: None,
            save_isolation_expected: false,
            running_probe: Arc::new(crate::launch::sc2_running),
            verification_probe: None,
            rollback_pre_mutation_hook: None,
            fail_after: None,
        }
    }

    pub fn with_strategy(mut self, strategy: Option<StrategyChoice>) -> Self {
        self.strategy = strategy;
        self
    }

    pub fn with_external_mods_policy(mut self, policy: ExternalModsPolicy) -> Self {
        self.external_mods_policy = policy;
        self
    }

    pub fn with_saves(mut self, saves: Option<SavesManager>) -> Self {
        self.save_isolation_expected = saves.is_some();
        self.saves = saves;
        self
    }

    /// Preserve the configured isolation requirement even when the selected
    /// profile cannot currently be resolved. Recovery then blocks rather than
    /// skipping a save phase.
    pub fn with_save_isolation_expected(mut self, expected: bool) -> Self {
        self.save_isolation_expected = expected;
        self
    }

    #[doc(hidden)]
    pub fn with_running_probe(mut self, probe: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.running_probe = Arc::new(probe);
        self
    }

    /// Test observer invoked once for every complete live deployment verification.
    #[doc(hidden)]
    pub fn with_verification_probe(mut self, probe: impl Fn() + Send + Sync + 'static) -> Self {
        self.verification_probe = Some(Arc::new(probe));
        self
    }

    #[doc(hidden)]
    pub fn with_fail_after(mut self, point: FailurePoint) -> Self {
        self.fail_after = Some(point);
        self
    }

    /// Deterministic test hook after rollback preflight and before the first
    /// resource mutation.
    #[doc(hidden)]
    pub fn with_rollback_pre_mutation_hook(
        mut self,
        hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        self.rollback_pre_mutation_hook = Some(Arc::new(hook));
        self
    }

    /// Recover an interrupted operation, then report the complete Library
    /// state. A recovery failure remains visible as `recovery_required` rather
    /// than hiding the installed packages behind a startup error.
    pub fn initialize(&self) -> Result<StartupReport> {
        let mut notes = Vec::new();
        let recovery_performed = match PendingOperation::load(self.store.root()) {
            Ok(Some(_)) => match self.recover_pending() {
                Ok(()) => true,
                Err(error) => {
                    notes.push(error.to_string());
                    false
                }
            },
            Ok(None) => false,
            Err(error) => {
                notes.push(error.to_string());
                false
            }
        };
        Ok(StartupReport {
            library: self.library_snapshot()?,
            recovery_performed,
            notes,
        })
    }

    pub fn library_snapshot(&self) -> Result<LibrarySnapshot> {
        let mut snapshot = crate::library::scan(self.store)?;
        let workflow_health = self.cached_health().unwrap_or_else(|| self.health());
        if workflow_health.state == HealthState::RecoveryRequired {
            snapshot.health.state = HealthState::RecoveryRequired;
        } else if workflow_health.state == HealthState::Drifted
            && snapshot.health.state == HealthState::Ready
        {
            snapshot.health.state = HealthState::Drifted;
        }
        snapshot.health.issues.extend(workflow_health.issues);
        snapshot
            .health
            .issues
            .sort_by(|left, right| left.code.cmp(&right.code));
        snapshot
            .health
            .issues
            .dedup_by(|left, right| left.code == right.code && left.path == right.path);
        Ok(snapshot)
    }

    /// Verify the committed campaign and owned filesystem resources without
    /// changing them.
    pub fn health(&self) -> Health {
        let health = self.compute_health();
        self.cache_health(health.clone());
        health
    }

    fn compute_health(&self) -> Health {
        match PendingOperation::load(self.store.root()) {
            Ok(Some(_)) => {
                return Health {
                    state: HealthState::RecoveryRequired,
                    issues: vec![HealthIssue {
                        code: "recovery_required".into(),
                        message: "An interrupted campaign operation must be recovered".into(),
                        path: None,
                        repairable: false,
                    }],
                };
            }
            Err(error) => {
                return Health {
                    state: HealthState::RecoveryRequired,
                    issues: vec![HealthIssue {
                        code: error.code().to_string(),
                        message: error.to_string(),
                        path: error.path().map(|path| path.display().to_string()),
                        repairable: false,
                    }],
                };
            }
            Ok(None) => {}
        }
        let expected = match self.store.active_campaign() {
            Ok(expected) => expected,
            Err(error) => return health_from_error(error, HealthState::RecoveryRequired),
        };
        if self.save_isolation_expected && self.saves.is_none() {
            return health_from_error(save_profile_unavailable(), HealthState::Drifted);
        }
        match self.verify_state(expected.as_ref()) {
            Ok(()) => Health::default(),
            Err(error) => health_from_error(error, HealthState::Drifted),
        }
    }

    fn cached_health(&self) -> Option<Health> {
        self.store.cached_workflow_health(
            self.layout.root(),
            self.save_isolation_expected,
            self.saves.is_some(),
        )
    }

    fn cache_health(&self, health: Health) {
        self.store.cache_workflow_health(
            self.layout.root(),
            self.save_isolation_expected,
            self.saves.is_some(),
            health,
        );
    }

    fn cache_ready(&self) {
        self.cache_health(Health::default());
    }

    /// Preflight an installed target, or the currently active campaign when
    /// `package_id` is `None`.
    pub fn preflight(&self, package_id: Option<&PackageId>) -> Result<Health> {
        self.ensure_mutation_checkpoint()?;
        self.layout.validate()?;
        if PendingOperation::load(self.store.root())?.is_some() {
            return Ok(Health {
                state: HealthState::RecoveryRequired,
                issues: vec![HealthIssue {
                    code: "recovery_required".into(),
                    message: "An interrupted campaign operation must be recovered".into(),
                    path: None,
                    repairable: false,
                }],
            });
        }
        if let Some(package_id) = package_id {
            let active = self.store.active_campaign()?;
            if let Some(active) = active.as_ref().filter(|active| &active.id == package_id) {
                self.campaign_manifest(active)?;
            } else {
                self.store.verify_package(package_id)?;
            }
        }
        Ok(self.health())
    }

    pub fn activate(&self, package_id: &PackageId) -> Result<ActiveCampaign> {
        self.ensure_mutation_ready()?;
        self.layout.validate()?;
        if let Some(active) = self
            .store
            .active_campaign()?
            .filter(|active| &active.id == package_id)
        {
            self.campaign_manifest(&active)?;
            self.verify_state_ready(Some(&active)).map_err(|error| {
                package_err(
                    "active_campaign_drifted",
                    format!("the active campaign needs repair: {}", error),
                )
            })?;
            return Ok(active);
        }
        self.require_save_isolation_available()?;
        let target_manifest = self.store.verify_package(package_id)?;
        let target = campaign_from_manifest(&target_manifest);
        self.transition(OperationKind::Activate, Some(target_manifest))?;
        Ok(target)
    }

    pub fn restore_vanilla(&self) -> Result<()> {
        self.ensure_mutation_ready()?;
        if self.store.active_campaign()?.is_none() {
            self.verify_state_ready(None)?;
            return Ok(());
        }
        self.transition(OperationKind::Restore, None)
    }

    /// Explicitly replace drifted StarVault-created deployment files. The
    /// original slot and changed created Mods files are backed up until the
    /// repaired state verifies. Borrowed Mods are never overwritten.
    pub fn repair_active(&self) -> Result<()> {
        self.ensure_mutation_ready()?;
        let active = self.store.active_campaign()?.ok_or_else(|| {
            package_err(
                "no_active_campaign",
                "there is no active campaign to repair",
            )
        })?;
        if self.verify_state_ready(Some(&active)).is_ok() {
            return Ok(());
        }
        let manifest = self.verified_package_manifest(&active)?;
        self.repair_transition(active, manifest)
    }

    /// Activate if needed, run final preflight, and launch without releasing
    /// the caller's mutation lock between those steps.
    pub fn play(&self, package_id: &PackageId) -> Result<ActiveCampaign> {
        self.play_with(package_id, crate::launch::launch)
    }

    #[doc(hidden)]
    pub fn play_with(
        &self,
        package_id: &PackageId,
        launcher: impl FnOnce(&WindowsLayout) -> Result<()>,
    ) -> Result<ActiveCampaign> {
        self.ensure_mutation_ready()?;
        let before_health = self.preflight(Some(package_id))?;
        if before_health.state != HealthState::Ready {
            return Err(package_err(
                "launch_preflight_failed",
                "the current game state did not pass launch preflight",
            ));
        }
        let before = self.store.active_campaign()?;
        let active = self.activate(package_id)?;
        let activated = before.as_ref() != Some(&active);
        let health = self.preflight(None)?;
        if health.state != HealthState::Ready {
            return Err(package_err(
                "launch_preflight_failed",
                "the active campaign did not pass launch preflight",
            ));
        }
        launcher(self.layout).map_err(|error| {
            if activated {
                Error::from(EnvironmentError::LaunchFailedAfterActivation {
                    detail: error.diagnostic(),
                })
            } else {
                Error::from(EnvironmentError::LaunchFailed {
                    detail: error.diagnostic(),
                })
            }
        })?;
        Ok(active)
    }

    pub fn recover_pending(&self) -> Result<()> {
        let Some(journal) = PendingOperation::load(self.store.root())? else {
            return Ok(());
        };
        self.ensure_mutation_checkpoint()?;
        self.validate_journal(&journal)?;
        let ledger = self.store.active_campaign()?;
        if journal.phase == OperationPhase::Preparing {
            if ledger != journal.previous_campaign {
                return Err(package_err(
                    "recovery_required",
                    "the preparation journal and activation ledger disagree",
                ));
            }
            return self.finish_preparing_cleanup(&journal);
        }
        if journal.phase == OperationPhase::RollbackVerified {
            if ledger != journal.previous_campaign {
                return Err(package_err(
                    "recovery_required",
                    "the verified rollback marker and activation ledger disagree",
                ));
            }
            return self.finish_rollback_cleanup(&journal);
        }
        if journal.kind == OperationKind::Repair {
            if self.verify_state(journal.target_campaign.as_ref()).is_ok() {
                return self.finish_committed(&journal);
            }
            return self.rollback_journal(&journal);
        }
        if ledger == journal.target_campaign {
            self.finish_committed(&journal)?;
            return Ok(());
        }
        if ledger == journal.previous_campaign {
            return self.rollback_journal(&journal);
        }
        Err(package_err(
            "recovery_required",
            "the operation journal and activation ledger disagree; backups were preserved",
        ))
    }

    fn ensure_mutation_ready(&self) -> Result<()> {
        self.ensure_mutation_checkpoint()?;
        self.store.invalidate_workflow_health();
        if PendingOperation::load(self.store.root())?.is_some() {
            self.recover_pending()?;
        }
        Ok(())
    }

    fn ensure_mutation_checkpoint(&self) -> Result<()> {
        self.ensure_game_stopped()?;
        self.layout.validate_mutation_roots()
    }

    fn ensure_game_stopped(&self) -> Result<()> {
        if (self.running_probe)() {
            Err(EnvironmentError::GameRunning.into())
        } else {
            Ok(())
        }
    }

    fn transition(
        &self,
        kind: OperationKind,
        target_manifest: Option<PackageManifest>,
    ) -> Result<()> {
        self.require_save_isolation_available()?;
        let previous_campaign = self.store.active_campaign()?;
        let previous_manifest = previous_campaign
            .as_ref()
            .map(|campaign| self.campaign_manifest(campaign))
            .transpose()?;
        self.verify_state(previous_campaign.as_ref())?;
        let previous_mods = self.store.managed_mods()?;
        let target_campaign = target_manifest.as_ref().map(campaign_from_manifest);
        let operation_id = next_operation_id();
        let planned_saves = self
            .saves
            .as_ref()
            .map(|manager| {
                manager.planned_paths(
                    save_transition(previous_campaign.as_ref(), target_campaign.as_ref()),
                    &operation_id,
                )
            })
            .transpose()?;
        let mut paths = OperationPaths {
            slots: SlotOperationJournal::new(
                expected_slot_paths(
                    self.layout,
                    previous_campaign.as_ref(),
                    target_campaign.as_ref(),
                    &operation_id,
                ),
                Vec::new(),
                Vec::new(),
            ),
            mods_staging: Some(sibling_path(
                &self.layout.mods_dir(),
                "staging",
                &operation_id,
            )),
            mods_backup: Some(sibling_path(
                &self.layout.mods_dir(),
                "backup",
                &operation_id,
            )),
            ..OperationPaths::default()
        };
        if let Some(saves) = &planned_saves {
            paths.saves_staging = Some(saves.saves_staging.clone());
            paths.saves_backup = Some(saves.saves_backup.clone());
            paths.banks_staging = Some(saves.banks_staging.clone());
            paths.banks_backup = Some(saves.banks_backup.clone());
        }
        let mut journal = PendingOperation::new_preparing(
            operation_id.clone(),
            kind,
            previous_campaign.clone(),
            target_campaign.clone(),
            paths,
        );
        journal.persist(self.store.root())?;
        self.fail(FailurePoint::Preparing)?;

        let prepared = (|| -> Result<_> {
            self.ensure_mutation_checkpoint()?;
            let saves = self.prepare_saves(
                previous_campaign.as_ref(),
                target_campaign.as_ref(),
                &operation_id,
            )?;
            self.fail(FailurePoint::SavesPrepared)?;

            self.ensure_mutation_checkpoint()?;
            let slots = SlotManager::new(self.layout, self.store)
                .with_strategy(self.strategy)
                .prepare(
                    previous_manifest.as_ref(),
                    target_manifest.as_ref(),
                    &operation_id,
                )?;
            self.fail(FailurePoint::SlotsPrepared)?;

            self.ensure_mutation_checkpoint()?;
            let mods = PreparedModsTransition::prepare_with_policy(
                self.store,
                &self.layout.mods_dir(),
                &previous_mods,
                target_manifest.as_ref(),
                &operation_id,
                self.external_mods_policy,
            )?;
            self.fail(FailurePoint::ModsPrepared)?;
            Ok((saves, slots, mods))
        })();
        let (saves, slots, mods) = match prepared {
            Ok(prepared) => prepared,
            Err(error) if matches!(error.code(), "simulated_interruption" | "game_running") => {
                return Err(error);
            }
            Err(error) => return self.recover_preparation_error(&journal, error),
        };
        journal.paths.slots = slots.journal_paths();
        journal.paths.save_recovery_proof = saves
            .as_ref()
            .map(|saves| saves.recovery_proof().cloned())
            .transpose()?;
        journal.paths.mods_plan_sha256 = Some(mods.plan_sha256().to_owned());
        journal.advance(self.store.root(), OperationPhase::Prepared)?;
        self.fail(FailurePoint::Prepared)?;

        let result = (|| -> Result<()> {
            self.ensure_mutation_checkpoint()?;
            if let Some(saves) = &saves {
                saves.apply_journaled()?;
            }
            journal.advance(self.store.root(), OperationPhase::SavesSwapped)?;
            self.fail(FailurePoint::SavesSwapped)?;

            self.ensure_mutation_checkpoint()?;
            slots.apply_journaled()?;
            journal.advance(self.store.root(), OperationPhase::SlotsSwapped)?;
            self.fail(FailurePoint::SlotsSwapped)?;

            self.ensure_mutation_checkpoint()?;
            mods.apply()?;
            journal.advance(self.store.root(), OperationPhase::ModsSwapped)?;
            self.fail(FailurePoint::ModsSwapped)?;

            self.ensure_mutation_checkpoint()?;
            self.store
                .commit_active_state(target_campaign.as_ref(), mods.target_rows())?;
            self.fail(FailurePoint::LedgerCommittedBeforeJournal)?;
            journal.advance(self.store.root(), OperationPhase::LedgerCommitted)?;
            self.fail(FailurePoint::LedgerCommitted)?;
            self.finish_committed(&journal)
        })();

        match result {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.code(), "simulated_interruption" | "game_running") => {
                Err(error)
            }
            Err(error) => self.recover_after_error(&journal, error),
        }
    }

    fn prepare_saves(
        &self,
        previous: Option<&ActiveCampaign>,
        target: Option<&ActiveCampaign>,
        operation_id: &str,
    ) -> Result<Option<PreparedSaveTransition>> {
        self.saves
            .as_ref()
            .map(|manager| manager.prepare(save_transition(previous, target), operation_id))
            .transpose()
    }

    fn repair_transition(&self, active: ActiveCampaign, manifest: PackageManifest) -> Result<()> {
        let operation_id = next_operation_id();
        let previous_mods = self.store.managed_mods()?;
        let paths = OperationPaths {
            slots: SlotOperationJournal::new(
                expected_slot_paths(self.layout, Some(&active), Some(&active), &operation_id),
                Vec::new(),
                Vec::new(),
            ),
            mods_staging: Some(sibling_path(
                &self.layout.mods_dir(),
                "staging",
                &operation_id,
            )),
            mods_backup: Some(sibling_path(
                &self.layout.mods_dir(),
                "backup",
                &operation_id,
            )),
            ..OperationPaths::default()
        };
        let mut journal = PendingOperation::new_preparing(
            operation_id.clone(),
            OperationKind::Repair,
            Some(active.clone()),
            Some(active.clone()),
            paths,
        );
        journal.persist(self.store.root())?;
        self.fail(FailurePoint::Preparing)?;

        let prepared = (|| -> Result<_> {
            self.ensure_mutation_checkpoint()?;
            self.fail(FailurePoint::SavesPrepared)?;
            let slots = SlotManager::new(self.layout, self.store)
                .with_strategy(self.strategy)
                .prepare_repair(&manifest, &operation_id)?;
            self.fail(FailurePoint::SlotsPrepared)?;
            self.ensure_mutation_checkpoint()?;
            let mods = PreparedModsTransition::prepare_repair(
                self.store,
                &self.layout.mods_dir(),
                &previous_mods,
                &manifest,
                &operation_id,
            )?;
            self.fail(FailurePoint::ModsPrepared)?;
            Ok((slots, mods))
        })();
        let (slots, mods) = match prepared {
            Ok(prepared) => prepared,
            Err(error) if matches!(error.code(), "simulated_interruption" | "game_running") => {
                return Err(error);
            }
            Err(error) => return self.recover_preparation_error(&journal, error),
        };
        journal.paths.slots = slots.journal_paths();
        journal.paths.mods_plan_sha256 = Some(mods.plan_sha256().to_owned());
        journal.advance(self.store.root(), OperationPhase::Prepared)?;
        self.fail(FailurePoint::Prepared)?;

        let result = (|| -> Result<()> {
            self.ensure_mutation_checkpoint()?;
            journal.advance(self.store.root(), OperationPhase::SavesSwapped)?;
            self.fail(FailurePoint::SavesSwapped)?;
            self.ensure_mutation_checkpoint()?;
            slots.apply_journaled()?;
            journal.advance(self.store.root(), OperationPhase::SlotsSwapped)?;
            self.fail(FailurePoint::SlotsSwapped)?;
            self.ensure_mutation_checkpoint()?;
            mods.apply()?;
            journal.advance(self.store.root(), OperationPhase::ModsSwapped)?;
            self.fail(FailurePoint::ModsSwapped)?;
            self.ensure_mutation_checkpoint()?;
            self.store
                .commit_active_state(Some(&active), mods.target_rows())?;
            self.fail(FailurePoint::LedgerCommittedBeforeJournal)?;
            journal.advance(self.store.root(), OperationPhase::LedgerCommitted)?;
            self.fail(FailurePoint::LedgerCommitted)?;
            self.finish_committed(&journal)
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.code(), "simulated_interruption" | "game_running") => {
                Err(error)
            }
            Err(error) => self.recover_after_error(&journal, error),
        }
    }

    fn recover_after_error(&self, journal: &PendingOperation, original: Error) -> Result<()> {
        if journal.kind == OperationKind::Repair {
            if self.verify_state(journal.target_campaign.as_ref()).is_ok() {
                return self.finish_committed(journal);
            }
            return match self.rollback_journal(journal) {
                Ok(()) => Err(original),
                Err(rollback) if rollback.code() == "game_running" => Err(rollback),
                Err(rollback) => Err(internal_err(
                    "repair_rollback_failed",
                    "StarVault could not restore the pre-repair files; recovery data was preserved",
                    format!(
                        "repair failed: {}; rollback failed: {}",
                        original.diagnostic(),
                        rollback.diagnostic()
                    ),
                )),
            };
        }
        let ledger = self.store.active_campaign()?;
        if ledger == journal.target_campaign {
            return match self.finish_committed(journal) {
                Ok(()) => Ok(()),
                Err(recovery) if recovery.code() == "game_running" => Err(recovery),
                Err(recovery) => Err(internal_err(
                    "operation_recovery_failed",
                    "StarVault could not finish the committed operation; recovery data was preserved",
                    format!(
                        "operation failed: {}; committed recovery failed: {}",
                        original.diagnostic(),
                        recovery.diagnostic()
                    ),
                )),
            };
        }
        if ledger == journal.previous_campaign {
            return match self.rollback_journal(journal) {
                Ok(()) => Err(original),
                Err(rollback) if rollback.code() == "game_running" => Err(rollback),
                Err(rollback) => Err(internal_err(
                    "operation_rollback_failed",
                    "StarVault could not restore the previous state; recovery data was preserved",
                    format!(
                        "operation failed: {}; rollback failed: {}",
                        original.diagnostic(),
                        rollback.diagnostic()
                    ),
                )),
            };
        }
        Err(internal_err(
            "operation_ledger_ambiguous",
            "StarVault could not determine which activation state was committed; recovery data was preserved",
            original.diagnostic(),
        ))
    }

    fn recover_preparation_error(&self, journal: &PendingOperation, original: Error) -> Result<()> {
        match self.finish_preparing_cleanup(journal) {
            Ok(()) => Err(original),
            Err(cleanup) if cleanup.code() == "game_running" => Err(cleanup),
            Err(cleanup) => Err(internal_err(
                "operation_prepare_cleanup_failed",
                "StarVault could not clean up the interrupted preparation; recovery data was preserved",
                format!(
                    "preparation failed: {}; cleanup failed: {}",
                    original.diagnostic(),
                    cleanup.diagnostic()
                ),
            )),
        }
    }

    fn finish_preparing_cleanup(&self, journal: &PendingOperation) -> Result<()> {
        if journal.phase != OperationPhase::Preparing {
            return Err(invalid_journal(
                "only a preparing operation can discard staged artifacts",
            ));
        }
        let mut failures = Vec::new();

        match self.recover_saves(journal) {
            Ok(Some(saves)) => {
                self.ensure_mutation_checkpoint()?;
                if let Err(error) = saves.discard_prepared() {
                    failures.push(format!("saves: {}", error.diagnostic()));
                }
            }
            Ok(None) => {}
            Err(error) => failures.push(format!("saves: {}", error.diagnostic())),
        }

        self.ensure_mutation_checkpoint()?;
        if let Err(error) = slots::finalize_paths(&journal.paths.slots) {
            failures.push(format!("slots: {}", error.diagnostic()));
        }

        self.ensure_mutation_checkpoint()?;
        match (
            journal.paths.mods_backup.as_deref(),
            journal.paths.mods_staging.as_deref(),
        ) {
            (Some(backup), Some(staging)) => {
                if let Err(error) = mods::finalize_paths(backup, staging) {
                    failures.push(format!("Mods: {}", error.diagnostic()));
                }
            }
            _ => failures.push("Mods: incomplete preparation paths".into()),
        }

        if !failures.is_empty() {
            return Err(internal_err(
                "operation_prepare_cleanup_failed",
                "StarVault could not clean up every preparation artifact; recovery data was preserved",
                failures.join("; "),
            ));
        }
        self.ensure_mutation_checkpoint()?;
        PendingOperation::remove_expected(self.store.root(), journal)
    }

    fn finish_committed(&self, journal: &PendingOperation) -> Result<()> {
        let saves = self.recover_saves(journal)?;
        let mods_backup = required_path(&journal.paths.mods_backup, "Mods backup")?;
        let mods_staging = required_path(&journal.paths.mods_staging, "Mods staging")?;
        let mods_plan_sha256 = journal
            .paths
            .mods_plan_sha256
            .as_deref()
            .ok_or_else(|| invalid_journal("missing Mods plan digest"))?;
        if let Some(saves) = &saves {
            saves.verify_finalize_ready()?;
        }
        slots::verify_finalize_bound_paths(&journal.paths.slots)?;
        mods::verify_finalize_paths_bound(mods_backup, mods_staging, mods_plan_sha256)?;
        if let Some(saves) = &saves {
            self.ensure_mutation_checkpoint()?;
            saves.finalize()?;
        }
        self.ensure_mutation_checkpoint()?;
        slots::commit_paths(
            &journal.paths.slots,
            journal.previous_campaign.is_some(),
            journal.target_campaign.is_some(),
        )?;
        self.verify_state_shape_ready(journal.target_campaign.as_ref())?;
        self.ensure_mutation_checkpoint()?;
        slots::finalize_preverified_paths(&journal.paths.slots)?;
        self.ensure_mutation_checkpoint()?;
        mods::finalize_preverified_paths(mods_backup, mods_staging)?;
        self.ensure_mutation_checkpoint()?;
        PendingOperation::remove_expected(self.store.root(), journal)?;
        if let Some(saves) = saves {
            self.ensure_mutation_checkpoint()?;
            saves.clear_receipt()?;
        }
        Ok(())
    }

    fn rollback_journal(&self, journal: &PendingOperation) -> Result<()> {
        let previous = journal
            .previous_campaign
            .as_ref()
            .map(|campaign| self.campaign_manifest(campaign))
            .transpose()?;
        let target = journal
            .target_campaign
            .as_ref()
            .map(|campaign| self.campaign_manifest(campaign))
            .transpose()?;
        let saves = self.recover_saves(journal)?;
        if let Some(saves) = &saves {
            saves.verify_rollback_ready()?;
        }
        let mods_plan_sha256 = journal
            .paths
            .mods_plan_sha256
            .as_deref()
            .ok_or_else(|| invalid_journal("missing Mods plan digest"))?;
        mods::verify_rollback_from_paths_bound(
            &self.layout.mods_dir(),
            required_path(&journal.paths.mods_backup, "Mods backup")?,
            mods_plan_sha256,
        )?;
        if journal.kind == OperationKind::Repair {
            slots::verify_repair_rollback_paths(&journal.paths.slots, target.as_ref())?;
        } else {
            slots::verify_rollback_paths_checked(
                &journal.paths.slots,
                previous.as_ref(),
                target.as_ref(),
            )?;
        }
        if let Some(hook) = &self.rollback_pre_mutation_hook {
            hook();
        }
        self.ensure_mutation_checkpoint()?;
        mods::rollback_from_paths_preserving_bound(
            &self.layout.mods_dir(),
            required_path(&journal.paths.mods_backup, "Mods backup")?,
            required_path(&journal.paths.mods_staging, "Mods staging")?,
            mods_plan_sha256,
        )?;
        self.fail(FailurePoint::RollbackModsRestored)?;
        self.ensure_mutation_checkpoint()?;
        if journal.kind == OperationKind::Repair {
            slots::rollback_repair_paths_checked(&journal.paths.slots, target.as_ref())?;
        } else {
            slots::rollback_paths_checked(
                &journal.paths.slots,
                previous.as_ref(),
                target.as_ref(),
            )?;
        }
        self.fail(FailurePoint::RollbackSlotsRestored)?;
        if let Some(saves) = saves {
            self.ensure_mutation_checkpoint()?;
            saves.rollback()?;
        }
        self.fail(FailurePoint::RollbackSavesRestored)?;
        mods::verify_rollback_from_paths_bound(
            &self.layout.mods_dir(),
            required_path(&journal.paths.mods_backup, "Mods backup")?,
            mods_plan_sha256,
        )?;
        if journal.kind != OperationKind::Repair {
            self.verify_state_ready(journal.previous_campaign.as_ref())?;
        }
        let mut verified = journal.clone();
        self.ensure_mutation_checkpoint()?;
        verified.advance(self.store.root(), OperationPhase::RollbackVerified)?;
        self.fail(FailurePoint::RollbackVerified)?;
        self.finish_rollback_cleanup(&verified)
    }

    fn finish_rollback_cleanup(&self, journal: &PendingOperation) -> Result<()> {
        let mods_backup = required_path(&journal.paths.mods_backup, "Mods backup")?;
        let mods_staging = required_path(&journal.paths.mods_staging, "Mods staging")?;
        let mods_plan_sha256 = journal
            .paths
            .mods_plan_sha256
            .as_deref()
            .ok_or_else(|| invalid_journal("missing Mods plan digest"))?;
        slots::verify_finalize_bound_paths(&journal.paths.slots)?;
        mods::verify_finalize_paths_bound(mods_backup, mods_staging, mods_plan_sha256)?;
        if let Some(saves) = self.recover_saves(journal)? {
            self.ensure_mutation_checkpoint()?;
            saves.rollback()?;
        }
        self.ensure_mutation_checkpoint()?;
        slots::finalize_preverified_paths(&journal.paths.slots)?;
        self.ensure_mutation_checkpoint()?;
        mods::finalize_preverified_paths(mods_backup, mods_staging)?;
        self.ensure_mutation_checkpoint()?;
        PendingOperation::remove_expected(self.store.root(), journal)
    }

    fn recover_saves(&self, journal: &PendingOperation) -> Result<Option<PreparedSaveTransition>> {
        let save_path_count = [
            &journal.paths.saves_staging,
            &journal.paths.saves_backup,
            &journal.paths.banks_staging,
            &journal.paths.banks_backup,
        ]
        .into_iter()
        .filter(|path| path.is_some())
        .count();
        let has_paths = save_path_count != 0;
        let complete_paths = save_path_count == 4;
        if journal.saves_participated != complete_paths
            || (has_paths && !complete_paths)
            || (journal.kind != OperationKind::Repair
                && journal.saves_participated != self.save_isolation_expected)
        {
            return Err(package_err(
                "recovery_required",
                "save isolation participation does not match the pending operation",
            ));
        }
        if journal.phase == OperationPhase::Preparing && journal.paths.save_recovery_proof.is_some()
        {
            return Err(invalid_journal(
                "preparing operation contains a save recovery proof",
            ));
        }
        if !journal.saves_participated && journal.paths.save_recovery_proof.is_some() {
            return Err(invalid_journal(
                "operation without save isolation contains a save recovery proof",
            ));
        }
        if journal.saves_participated
            && journal.phase != OperationPhase::Preparing
            && journal.paths.save_recovery_proof.is_none()
        {
            return Err(invalid_journal("missing save recovery proof"));
        }
        let Some(manager) = &self.saves else {
            if journal.saves_participated {
                return Err(package_err(
                    "recovery_required",
                    "save isolation configuration does not match the pending operation",
                ));
            }
            return Ok(None);
        };
        if journal.kind == OperationKind::Repair && !journal.saves_participated {
            return Ok(None);
        }
        if !journal.saves_participated {
            return Err(package_err(
                "recovery_required",
                "save isolation configuration does not match the pending operation",
            ));
        }
        let transition = save_transition(
            journal.previous_campaign.as_ref(),
            journal.target_campaign.as_ref(),
        );
        let prepared = if journal.phase == OperationPhase::Preparing {
            manager.preparing(transition, &journal.operation_id)?
        } else {
            let proof = journal
                .paths
                .save_recovery_proof
                .clone()
                .ok_or_else(|| invalid_journal("missing save recovery proof"))?;
            manager.prepared(transition, &journal.operation_id, proof)?
        };
        let paths = prepared.paths();
        if journal.paths.saves_staging.as_ref() != Some(&paths.saves_staging)
            || journal.paths.saves_backup.as_ref() != Some(&paths.saves_backup)
            || journal.paths.banks_staging.as_ref() != Some(&paths.banks_staging)
            || journal.paths.banks_backup.as_ref() != Some(&paths.banks_backup)
        {
            return Err(package_err(
                "unsafe_operation_journal",
                "save recovery paths do not match the selected profile",
            ));
        }
        Ok(Some(prepared))
    }

    fn validate_journal(&self, journal: &PendingOperation) -> Result<()> {
        validate_operation_id(&journal.operation_id)?;
        match journal.kind {
            OperationKind::Activate if journal.target_campaign.is_none() => {
                return Err(invalid_journal("activation has no target campaign"));
            }
            OperationKind::Restore if journal.target_campaign.is_some() => {
                return Err(invalid_journal("restore has a target campaign"));
            }
            OperationKind::Repair
                if journal.previous_campaign.is_none()
                    || journal.previous_campaign != journal.target_campaign =>
            {
                return Err(invalid_journal(
                    "repair does not name one unchanged active campaign",
                ));
            }
            _ => {}
        }
        if let Some(previous) = &journal.previous_campaign {
            self.campaign_manifest(previous)?;
        }
        if let Some(target) = &journal.target_campaign {
            self.campaign_manifest(target)?;
        }
        let expected_slots = expected_slot_paths(
            self.layout,
            journal.previous_campaign.as_ref(),
            journal.target_campaign.as_ref(),
            &journal.operation_id,
        );
        if journal.paths.slots != expected_slots {
            return Err(package_err(
                "unsafe_operation_journal",
                "campaign-slot recovery paths do not match the game layout",
            ));
        }
        slots::validate_journal_bindings(
            &journal.paths.slots,
            journal.phase != OperationPhase::Preparing,
        )?;
        let mods_root = self.layout.mods_dir();
        if journal.paths.mods_staging.as_ref()
            != Some(&sibling_path(&mods_root, "staging", &journal.operation_id))
            || journal.paths.mods_backup.as_ref()
                != Some(&sibling_path(&mods_root, "backup", &journal.operation_id))
        {
            return Err(package_err(
                "unsafe_operation_journal",
                "Mods recovery paths do not match the game layout",
            ));
        }
        let digest = journal.paths.mods_plan_sha256.as_deref();
        if journal.phase != OperationPhase::Preparing && digest.is_none() {
            return Err(invalid_journal("missing Mods plan digest"));
        }
        if let Some(digest) = digest {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(invalid_journal("invalid Mods plan digest"));
            }
        }
        let _ = self.recover_saves(journal)?;
        Ok(())
    }

    fn campaign_manifest(&self, campaign: &ActiveCampaign) -> Result<PackageManifest> {
        let manifest = self.store.load_manifest_fresh(&campaign.id)?;
        Self::match_campaign_manifest(campaign, manifest)
    }

    fn verified_package_manifest(&self, campaign: &ActiveCampaign) -> Result<PackageManifest> {
        let manifest = self.store.verify_package(&campaign.id)?;
        Self::match_campaign_manifest(campaign, manifest)
    }

    fn match_campaign_manifest(
        campaign: &ActiveCampaign,
        manifest: PackageManifest,
    ) -> Result<PackageManifest> {
        if manifest.revision != campaign.revision || manifest.faction != campaign.faction {
            return Err(package_err(
                "active_campaign_manifest_mismatch",
                "the activation ledger does not match the installed package manifest",
            ));
        }
        Ok(manifest)
    }

    fn require_save_isolation_available(&self) -> Result<()> {
        if self.save_isolation_expected && self.saves.is_none() {
            Err(save_profile_unavailable())
        } else {
            Ok(())
        }
    }

    fn verify_state(&self, expected: Option<&ActiveCampaign>) -> Result<()> {
        self.verify_state_with_mods(expected, true)
    }

    fn verify_state_shape_ready(&self, expected: Option<&ActiveCampaign>) -> Result<()> {
        self.verify_state_with_mods(expected, false)?;
        self.cache_ready();
        Ok(())
    }

    fn verify_state_with_mods(
        &self,
        expected: Option<&ActiveCampaign>,
        verify_mod_contents: bool,
    ) -> Result<()> {
        if let Some(probe) = &self.verification_probe {
            probe();
        }
        if expected.is_some() || self.layout.root().symlink_metadata().is_ok() {
            self.layout.validate_mutation_roots()?;
        }
        if self.store.active_campaign()?.as_ref() != expected {
            return Err(package_err(
                "active_campaign_drifted",
                "the activation ledger does not match the expected campaign",
            ));
        }
        let manifest = expected
            .map(|campaign| self.campaign_manifest(campaign))
            .transpose()?;
        if let Err(error) =
            SlotManager::new(self.layout, self.store).verify_current(manifest.as_ref())
        {
            if expected.is_none() && error.code() == "slot_drift" {
                return Err(package_err(
                    "unowned_campaign_files",
                    "campaign files exist while StarVault is in vanilla state",
                ));
            }
            return Err(error);
        }
        let managed = self.store.managed_mods()?;
        if expected.is_none() && !managed.is_empty() {
            return Err(package_err(
                "orphaned_managed_mods",
                "managed Mods remain while no campaign is active",
            ));
        }
        if let Some(manifest) = &manifest {
            self.store
                .verify_managed_mods_manifest(manifest, &managed)?;
        }
        if verify_mod_contents {
            mods::verify_managed(&self.layout.mods_dir(), &managed)
        } else {
            mods::verify_managed_shape(&self.layout.mods_dir(), &managed)
        }
    }

    fn verify_state_ready(&self, expected: Option<&ActiveCampaign>) -> Result<()> {
        self.verify_state(expected)?;
        self.cache_ready();
        Ok(())
    }

    fn fail(&self, point: FailurePoint) -> Result<()> {
        if self.fail_after == Some(point) {
            Err(internal_err(
                "simulated_interruption",
                "test interruption",
                format!("interrupted after {point:?}"),
            ))
        } else {
            Ok(())
        }
    }
}

fn campaign_from_manifest(manifest: &PackageManifest) -> ActiveCampaign {
    ActiveCampaign {
        id: manifest.id.clone(),
        revision: manifest.revision.clone(),
        faction: manifest.faction,
    }
}

fn save_transition(
    previous: Option<&ActiveCampaign>,
    target: Option<&ActiveCampaign>,
) -> SaveTransition {
    SaveTransition {
        previous_owner: previous
            .map(|campaign| SaveOwner::Package(campaign.id.clone()))
            .unwrap_or(SaveOwner::Plain),
        previous_faction: previous.map(|campaign| campaign.faction),
        target_owner: target
            .map(|campaign| SaveOwner::Package(campaign.id.clone()))
            .unwrap_or(SaveOwner::Plain),
        target_faction: target.map(|campaign| campaign.faction),
    }
}

fn expected_slot_paths(
    layout: &WindowsLayout,
    previous: Option<&ActiveCampaign>,
    target: Option<&ActiveCampaign>,
    operation_id: &str,
) -> Vec<SlotOperationPaths> {
    let faction = target
        .map(|campaign| campaign.faction)
        .or_else(|| previous.map(|campaign| campaign.faction))
        .unwrap_or(crate::layout::SlotId::Wol);
    let live = layout.campaign_dir();
    vec![SlotOperationPaths {
        staging: sibling_path(&live, "staging", operation_id),
        backup: sibling_path(&live, "backup", operation_id),
        live,
        faction,
    }]
}

fn sibling_path(path: &Path, kind: &str, operation_id: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{kind}-{operation_id}"))
}

fn required_path<'a>(path: &'a Option<PathBuf>, label: &str) -> Result<&'a Path> {
    path.as_deref()
        .ok_or_else(|| invalid_journal(format!("missing {label} path")))
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.is_empty()
        || operation_id.len() > 96
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(invalid_journal("operation id is not a safe path component"));
    }
    Ok(())
}

fn invalid_journal(detail: impl Into<String>) -> Error {
    package_err(
        "unsafe_operation_journal",
        format!(
            "the pending operation journal is invalid: {}",
            detail.into()
        ),
    )
}

fn save_profile_unavailable() -> Error {
    user_err(
        "save_profile_unavailable",
        "the configured save-isolation profile is unavailable; restore that profile or correct the setting before continuing",
    )
}

fn next_operation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{sequence}", std::process::id())
}

fn health_from_error(error: Error, state: HealthState) -> Health {
    let repairable = matches!(
        error.code(),
        "active_campaign_drifted" | "managed_file_changed" | "slot_drift"
    );
    Health {
        state,
        issues: vec![HealthIssue {
            code: error.code().to_string(),
            message: error.to_string(),
            path: error.path().map(|path| path.display().to_string()),
            repairable,
        }],
    }
}
