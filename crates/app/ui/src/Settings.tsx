import { useCallback, useEffect, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { open } from "@tauri-apps/plugin-dialog";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Anchor,
  Button,
  Card,
  Grid,
  Group,
  Modal,
  Select,
  Stack,
  Switch,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { errMessage } from "./errors";
import {
  clearAllData,
  discoverGameExe,
  getConfig,
  getSavesStatus,
  listLibrary,
  saveConfig,
} from "./ipc";
import type { LibrarySnapshot, SavesStatus } from "./types";

type SaveStatus = "idle" | "saving" | "saved" | "error";

export default function Settings() {
  const [gameExe, setGameExe] = useState("");
  const [strategy, setStrategy] = useState<string | null>("auto");
  const [crashReports, setCrashReports] = useState(false);
  const [logLevel, setLogLevel] = useState("info");
  const [saveIsolation, setSaveIsolation] = useState(false);
  const [savesProfile, setSavesProfile] = useState<string | null>(null);
  const [replaceExternalMods, setReplaceExternalMods] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);
  const [appVersion, setAppVersion] = useState("");
  const [savesStatus, setSavesStatus] = useState<SavesStatus | null>(null);
  const [library, setLibrary] = useState<LibrarySnapshot | null>(null);
  const [status, setStatus] = useState<SaveStatus>("idle");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const loadedRef = useRef(false);
  const skipNextSaveRef = useRef(false);
  const autosaveTimerRef = useRef<number | null>(null);
  const activeSavesRef = useRef<Set<Promise<void>>>(new Set());

  const loadSettings = useCallback(async () => {
    loadedRef.current = false;
    try {
      const [config, saves, librarySnapshot] = await Promise.all([
        getConfig(),
        getSavesStatus(),
        listLibrary(),
      ]);
      skipNextSaveRef.current = true;
      setGameExe(config.game_exe ?? "");
      setStrategy(config.strategy_override ?? "auto");
      setCrashReports(config.crash_reports_opt_in);
      setLogLevel(config.log_level ?? "info");
      setSaveIsolation(config.save_isolation);
      setSavesProfile(config.saves_profile);
      setReplaceExternalMods(config.replace_external_mods);
      setSavesStatus(saves);
      setLibrary(librarySnapshot);
      setStatus("idle");
      setErrorMsg(null);
      loadedRef.current = true;
    } catch (error) {
      notifications.show({ color: "red", title: "Load failed", message: errMessage(error) });
    }
  }, []);

  useEffect(() => {
    getVersion()
      .then(setAppVersion)
      .catch(() => setAppVersion(""));
    void loadSettings();
  }, [loadSettings]);

  useEffect(() => {
    if (!loadedRef.current) return;
    if (skipNextSaveRef.current) {
      skipNextSaveRef.current = false;
      return;
    }
    const timeout = window.setTimeout(() => {
      if (autosaveTimerRef.current === timeout) autosaveTimerRef.current = null;
      const save = (async () => {
        setStatus("saving");
        try {
          await saveConfig({
            gameExe: gameExe === "" ? null : gameExe,
            strategyOverride: strategy === "auto" ? null : strategy,
            crashReportsOptIn: crashReports,
            logLevel,
            saveIsolation,
            savesProfile,
            replaceExternalMods,
          });
          setStatus("saved");
          setErrorMsg(null);
        } catch (error) {
          setStatus("error");
          setErrorMsg(errMessage(error));
        }
      })();
      activeSavesRef.current.add(save);
      void save.finally(() => activeSavesRef.current.delete(save));
    }, 700);
    autosaveTimerRef.current = timeout;
    return () => {
      if (autosaveTimerRef.current === timeout) {
        window.clearTimeout(timeout);
        autosaveTimerRef.current = null;
      }
    };
  }, [gameExe, strategy, crashReports, logLevel, saveIsolation, savesProfile, replaceExternalMods]);

  const quiesceAutosave = useCallback(async () => {
    loadedRef.current = false;
    if (autosaveTimerRef.current !== null) {
      window.clearTimeout(autosaveTimerRef.current);
      autosaveTimerRef.current = null;
    }
    await Promise.allSettled([...activeSavesRef.current]);
  }, []);

  const recoveryRequired = library?.health.state === "recovery_required";
  const deploymentSettingsLocked =
    library === null || library.active_campaign !== null || recoveryRequired;
  const activeTitle = library?.active_campaign?.id ?? null;

  const browse = async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "StarCraft II", extensions: ["exe"] }],
    });
    if (selected && !Array.isArray(selected)) setGameExe(selected);
  };

  return (
    <Stack p="lg" gap="lg" h="100%">
      <Title order={2}>Settings</Title>

      {activeTitle && (
        <Alert color="blue" title="Campaign settings are locked">
          Return to vanilla before changing the game path, switch strategy, save isolation, or save
          profile. {activeTitle} is currently active.
        </Alert>
      )}
      {!activeTitle && recoveryRequired && (
        <Alert color="red" title="Recovery is required">
          Complete recovery from Library before changing the game path, switch strategy, save
          isolation, or save profile.
        </Alert>
      )}

      <Grid>
        <Grid.Col span={6}>
          <Card withBorder h="100%">
            <Stack gap="sm" h="100%" justify="space-between">
              <Stack gap="sm">
                <Text fw={500}>General</Text>
                <TextInput
                  label="StarCraft II.exe"
                  placeholder="C:\\Program Files (x86)\\StarCraft II\\StarCraft II.exe"
                  value={gameExe}
                  disabled={deploymentSettingsLocked}
                  onChange={(event) => setGameExe(event.currentTarget.value)}
                  error={status === "error" ? errorMsg : undefined}
                />
                <Group gap="xs">
                  <Button
                    variant="light"
                    size="xs"
                    disabled={deploymentSettingsLocked}
                    onClick={browse}
                  >
                    Browse…
                  </Button>
                  <Button
                    variant="light"
                    size="xs"
                    disabled={deploymentSettingsLocked}
                    onClick={async () => {
                      const found = await discoverGameExe();
                      if (found) {
                        setGameExe(found);
                        notifications.show({ color: "green", message: "Found StarCraft II." });
                      } else {
                        notifications.show({
                          color: "yellow",
                          message:
                            "Could not find an SC2 install automatically. Browse for it instead.",
                        });
                      }
                    }}
                  >
                    Auto-detect
                  </Button>
                  {status === "saving" && (
                    <Text size="xs" c="dimmed">
                      Saving…
                    </Text>
                  )}
                  {status === "saved" && (
                    <Text size="xs" c="dimmed">
                      Saved ✓
                    </Text>
                  )}
                </Group>
              </Stack>
              <Switch
                label="Crash reports"
                description="Opt-in. Sends internal failures only; no analytics exist."
                checked={crashReports}
                onChange={(event) => setCrashReports(event.currentTarget.checked)}
              />
            </Stack>
          </Card>
        </Grid.Col>

        <Grid.Col span={6}>
          <Card withBorder h="100%">
            <Stack gap="sm">
              <Text fw={500}>Advanced</Text>
              <Select
                label="Switch strategy"
                description="How campaign folders are placed into the game directory."
                data={[
                  { value: "auto", label: "Auto (junction first)" },
                  { value: "junction", label: "Junctions" },
                  { value: "copy", label: "Copy" },
                ]}
                value={strategy}
                disabled={deploymentSettingsLocked}
                onChange={setStrategy}
              />
              <Select
                label="Log level"
                description="Minimum severity kept in the operation log."
                data={[
                  { value: "info", label: "Info" },
                  { value: "warn", label: "Warnings and errors" },
                  { value: "error", label: "Errors only" },
                ]}
                value={logLevel}
                onChange={(value) => setLogLevel(value ?? "info")}
              />
              <Switch
                label="Replace conflicting external Mods automatically"
                description="Permanently replaces different files already in StarCraft II\\Mods. Failed activations roll back, but Return to vanilla cannot restore the replaced external file."
                checked={replaceExternalMods}
                onChange={(event) => setReplaceExternalMods(event.currentTarget.checked)}
              />
              <Switch
                label="Save isolation (Beta)"
                description={
                  activeTitle
                    ? "Return to vanilla before changing save isolation."
                    : recoveryRequired
                      ? "Complete recovery from Library before changing save isolation."
                      : (savesStatus?.reason ??
                        "Each campaign keeps its own saves. Enabling it first creates a recovery backup.")
                }
                disabled={deploymentSettingsLocked || (savesStatus ? !savesStatus.supported : true)}
                checked={saveIsolation}
                onChange={(event) => {
                  const enabled = event.currentTarget.checked;
                  if (enabled && !savesProfile && savesStatus?.selected) {
                    setSavesProfile(savesStatus.selected);
                  }
                  setSaveIsolation(enabled);
                }}
              />
              {savesStatus && savesStatus.supported && savesStatus.profiles.length > 1 && (
                <Select
                  label="Saves profile"
                  description="Multiple Battle.net accounts were found. Choose which saves to isolate."
                  data={savesStatus.profiles.map((profile) => ({
                    value: profile.id,
                    label: profile.label,
                  }))}
                  value={savesProfile}
                  disabled={deploymentSettingsLocked}
                  onChange={setSavesProfile}
                />
              )}
            </Stack>
          </Card>
        </Grid.Col>
      </Grid>

      <Card withBorder color="red">
        <Stack gap="sm">
          <Text fw={500}>Danger zone</Text>
          <Text size="sm" c="dimmed">
            Returns to vanilla, verifies the game files, then removes every imported package, the
            ledger, the log, and your settings. If restoration cannot be verified, nothing is
            deleted.
          </Text>
          <Button color="red" variant="light" w="fit-content" onClick={() => setConfirmClear(true)}>
            Clear all data…
          </Button>
        </Stack>
      </Card>

      <Group justify="flex-end">
        <Text size="xs" c="dimmed">
          StarVault CCM {appVersion} · unofficial builds must self-declare ·{" "}
          <Anchor
            component="a"
            href="https://discord.com/users/440833687257481227"
            target="_blank"
            rel="noreferrer"
            size="xs"
          >
            Support on Discord
          </Anchor>
        </Text>
      </Group>

      <Modal
        opened={confirmClear}
        onClose={() => setConfirmClear(false)}
        title="Clear all data?"
        size="sm"
      >
        <Stack gap="sm">
          <Text size="sm">
            StarVault will first return to vanilla and verify the result. Only then will it delete
            every imported package, the log, and your settings. This cannot be undone.
          </Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setConfirmClear(false)}>
              Cancel
            </Button>
            <Button
              color="red"
              onClick={async () => {
                setConfirmClear(false);
                try {
                  await quiesceAutosave();
                  await clearAllData();
                  await loadSettings();
                  notifications.show({ color: "green", message: "All data cleared." });
                } catch (error) {
                  notifications.show({
                    color: "red",
                    title: "Clear failed",
                    message: errMessage(error),
                  });
                  loadedRef.current = true;
                }
              }}
            >
              Delete everything
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
