import { useEffect, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { notifications } from "@mantine/notifications";
import {
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

interface ConfigDto {
  game_exe: string | null;
  strategy_override: string | null;
  crash_reports_opt_in: boolean;
  log_level: string;
  save_isolation: boolean;
  saves_profile: string | null;
}

interface SavesStatus {
  supported: boolean;
  reason: string | null;
  profiles: string[];
  selected: string | null;
  enabled: boolean;
}

type SaveStatus = "idle" | "saving" | "saved" | "error";

export default function Settings() {
  const [gameExe, setGameExe] = useState<string>("");
  const [strategy, setStrategy] = useState<string | null>("auto");
  const [crashReports, setCrashReports] = useState(false);
  const [logLevel, setLogLevel] = useState("info");
  const [saveIsolation, setSaveIsolation] = useState(false);
  const [savesProfile, setSavesProfile] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [appVersion, setAppVersion] = useState("");
  const [savesStatus, setSavesStatus] = useState<SavesStatus | null>(null);
  const [status, setStatus] = useState<SaveStatus>("idle");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  // Skip the auto-save effect until the initial load has populated state.
  const loadedRef = useRef(false);

  useEffect(() => {
    getVersion()
      .then(setAppVersion)
      .catch(() => setAppVersion(""));
    invoke<SavesStatus>("get_saves_status")
      .then(setSavesStatus)
      .catch(() => {});
    invoke<ConfigDto>("get_config")
      .then((cfg) => {
        setGameExe(cfg.game_exe ?? "");
        setStrategy(cfg.strategy_override ?? "auto");
        setCrashReports(cfg.crash_reports_opt_in);
        setLogLevel(cfg.log_level ?? "info");
        setSaveIsolation(cfg.save_isolation);
        setSavesProfile(cfg.saves_profile);
        loadedRef.current = true;
      })
      .catch((e) => notifications.show({ color: "red", title: "Load failed", message: String(e) }));
  }, []);

  // Auto-save: debounced so typing a path doesn't fire per keystroke.
  useEffect(() => {
    if (!loadedRef.current) return;
    const t = setTimeout(async () => {
      setStatus("saving");
      try {
        await invoke("save_config", {
          gameExe: gameExe === "" ? null : gameExe,
          strategyOverride: strategy === "auto" ? null : strategy,
          crashReportsOptIn: crashReports,
          logLevel,
          extras: { saveIsolation, savesProfile },
        });
        setStatus("saved");
        setErrorMsg(null);
      } catch (e) {
        // Keep the typed value visible; the inline warning explains why it
        // is not persisted yet. Config still holds the last valid state.
        setStatus("error");
        setErrorMsg(String(e));
      }
    }, 700);
    return () => clearTimeout(t);
  }, [gameExe, strategy, crashReports, logLevel, saveIsolation, savesProfile]);

  const browse = async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "StarCraft II", extensions: ["exe"] }],
    });
    if (selected) setGameExe(selected);
  };

  return (
    <Stack p="lg" gap="lg" h="100%">
      <Title order={2}>Settings</Title>

      <Grid>
        <Grid.Col span={6}>
          <Card withBorder h="100%">
            <Stack gap="sm" h="100%" justify="space-between">
              <Stack gap="sm">
                <Text fw={500}>General</Text>
                <TextInput
                  label="StarCraft II.exe"
                  placeholder="C:\Program Files (x86)\StarCraft II\StarCraft II.exe"
                  value={gameExe}
                  onChange={(e) => setGameExe(e.currentTarget.value)}
                  error={status === "error" ? errorMsg : undefined}
                />
                <Group gap="xs">
                  <Button variant="light" size="xs" onClick={browse}>
                    Browse…
                  </Button>
                  <Button
                    variant="light"
                    size="xs"
                    onClick={async () => {
                      const found = await invoke<string | null>("discover_game_exe");
                      if (found) {
                        setGameExe(found);
                        notifications.show({ color: "green", message: "Found StarCraft II." });
                      } else {
                        notifications.show({
                          color: "yellow",
                          message: "Could not find an SC2 install automatically — browse for it.",
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
                description="Opt-in. Sends crash data only; no analytics exist."
                checked={crashReports}
                onChange={(e) => setCrashReports(e.currentTarget.checked)}
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
                onChange={(v) => setLogLevel(v ?? "info")}
              />
              <Switch
                label="Save isolation (experimental)"
                description={
                  savesStatus?.reason ??
                  "Each campaign keeps its own saves; switching never strands progress."
                }
                disabled={savesStatus ? !savesStatus.supported : true}
                checked={saveIsolation}
                onChange={(e) => setSaveIsolation(e.currentTarget.checked)}
              />
              {savesStatus && savesStatus.supported && savesStatus.profiles.length > 1 && (
                <Select
                  label="Saves profile"
                  description="Multiple Battle.net accounts found — pick which saves to isolate."
                  data={savesStatus.profiles.map((p) => ({ value: p, label: p }))}
                  value={savesProfile}
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
            Removes every imported package, the ledger, the log, and your settings. Your game
            install is not touched. This cannot be undone.
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
            Every imported package, the log, and your settings will be deleted. Slots currently
            active in the game directory stay as they are until you restore them.
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
                  await invoke("clear_all_data");
                  const cfg = await invoke<ConfigDto>("get_config");
                  setGameExe(cfg.game_exe ?? "");
                  setStrategy(cfg.strategy_override ?? "auto");
                  setCrashReports(cfg.crash_reports_opt_in);
                  notifications.show({ color: "green", message: "All data cleared." });
                } catch (e) {
                  notifications.show({ color: "red", title: "Clear failed", message: String(e) });
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
