import { invoke } from "@tauri-apps/api/core";
import type {
  ConfigDto,
  ImportOperationSnapshot,
  ImportProgressPhase,
  LibrarySnapshot,
  MigrationCandidate,
  SavesStatus,
  StartupReport,
  TranslatorLinkTarget,
} from "./types";

export const initialize = () => invoke<StartupReport>("initialize");
export const listLibrary = () => invoke<LibrarySnapshot>("list_library");
export interface ExternalModsOptions {
  replaceExternalMods?: boolean;
  rememberExternalMods?: boolean;
}

export const activatePackage = (id: string, options: ExternalModsOptions = {}) =>
  invoke<void>("activate_package", { id, ...options });
export const playPackage = (id: string, options: ExternalModsOptions = {}) =>
  invoke<void>("play_package", { id, ...options });
export const restoreVanilla = () => invoke<void>("restore_vanilla");
export const resolveTranslatorLink = (instanceId: string) =>
  invoke<TranslatorLinkTarget>("resolve_translator_link", { instanceId });

export const getConfig = () => invoke<ConfigDto>("get_config");
export const getSavesStatus = () => invoke<SavesStatus>("get_saves_status");
export const discoverGameExe = () => invoke<string | null>("discover_game_exe");

export interface SaveConfigInput {
  gameExe: string | null;
  strategyOverride: string | null;
  crashReportsOptIn: boolean;
  logLevel: string;
  saveIsolation: boolean;
  savesProfile: string | null;
  replaceExternalMods: boolean;
  analyticsEnabled: boolean;
}

export const saveConfig = (input: SaveConfigInput) =>
  invoke<void>("save_config", {
    gameExe: input.gameExe,
    strategyOverride: input.strategyOverride,
    crashReportsOptIn: input.crashReportsOptIn,
    logLevel: input.logLevel,
    extras: {
      saveIsolation: input.saveIsolation,
      savesProfile: input.savesProfile ?? "",
      replaceExternalMods: input.replaceExternalMods,
      analyticsEnabled: input.analyticsEnabled,
    },
  });

export const setAnalytics = (enabled: boolean, acknowledged: boolean) =>
  invoke<void>("set_analytics", { enabled, acknowledged });

export const clearAllData = () => invoke<void>("clear_all_data");
export const revealPackage = (id: string) => invoke<string>("reveal_package", { id });
export const removePackage = (id: string) => invoke<void>("remove_package", { id });

export interface PackageMetadataInput {
  id: string;
  title: string;
  author: string;
  version: string;
  desc: string;
}

export const editPackageMetadata = (input: PackageMetadataInput) =>
  invoke<void>("edit_package_metadata", { ...input });

export interface ImportProgressEvent {
  op_id: string;
  phase: ImportProgressPhase;
  completed: number;
  total: number;
}

export interface ConfirmedImport {
  opId: string;
  id: string;
  faction: string;
  title: string | null;
  author: string | null;
  version: string | null;
  desc: string | null;
}

export const importApi = {
  analyze: (opId: string, path: string): Promise<ImportOperationSnapshot> =>
    invoke<ImportOperationSnapshot>("import_analyze", { opId, path }),
  analyzeTranslator: (
    opId: string,
    instanceId: string,
    expectedSize: number,
  ): Promise<ImportOperationSnapshot> =>
    invoke<ImportOperationSnapshot>("import_analyze_translator", {
      opId,
      instanceId,
      expectedSize,
    }),

  ingest: (input: ConfirmedImport): Promise<ImportOperationSnapshot> =>
    invoke<ImportOperationSnapshot>("import_ingest", {
      opId: input.opId,
      id: input.id,
      slot: input.faction,
      meta: {
        title: input.title,
        author: input.author,
        version: input.version,
        desc: input.desc,
      },
    }),

  cancel: (opId: string): Promise<void> => invoke<void>("import_cancel", { opId }),
};

export const listMigrationCandidates = () =>
  invoke<MigrationCandidate[]>("list_migration_candidates");

export const migrateCandidate = (candidateId: string, id: string, faction: string) =>
  invoke<string>("migrate_candidate", { candidateId, id, faction });
