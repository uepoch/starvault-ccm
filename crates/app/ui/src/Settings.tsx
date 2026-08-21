import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Button,
  Card,
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
}

export default function Settings() {
  const [gameExe, setGameExe] = useState<string>("");
  const [strategy, setStrategy] = useState<string | null>("auto");
  const [crashReports, setCrashReports] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);

  useEffect(() => {
    invoke<ConfigDto>("get_config")
      .then((cfg) => {
        setGameExe(cfg.game_exe ?? "");
        setStrategy(cfg.strategy_override ?? "auto");
        setCrashReports(cfg.crash_reports_opt_in);
      })
      .catch((e) => setError(String(e)));
  }, []);

  const browse = async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "StarCraft II", extensions: ["exe"] }],
    });
    if (selected) setGameExe(selected);
  };

  const save = async () => {
    setError(null);
    try {
      await invoke("save_config", {
        gameExe: gameExe === "" ? null : gameExe,
        strategyOverride: strategy === "auto" ? null : strategy,
        crashReportsOptIn: crashReports,
      });
      // Reload what the backend actually stored — never trust the optimistic
      // form state (a rejected path must not linger in the field).
      const cfg = await invoke<ConfigDto>("get_config");
      setGameExe(cfg.game_exe ?? "");
      setStrategy(cfg.strategy_override ?? "auto");
      setCrashReports(cfg.crash_reports_opt_in);
      notifications.show({ color: "green", message: "Settings saved." });
    } catch (e) {
      setError(String(e));
      notifications.show({ color: "red", title: "Could not save", message: String(e) });
    }
  };

  return (
    <Stack p="lg" gap="lg" maw={640}>
      <Title order={2}>Settings</Title>

      {error && (
        <Alert color="red" title="Could not save">
          {error}
        </Alert>
      )}

      <Card withBorder>
        <Stack gap="sm">
          <TextInput
            label="StarCraft II.exe"
            placeholder="C:\Program Files (x86)\StarCraft II\StarCraft II.exe"
            value={gameExe}
            onChange={(e) => setGameExe(e.currentTarget.value)}
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
          </Group>
          <Select
            label="Switch strategy"
            data={[
              { value: "auto", label: "Auto (junction first)" },
              { value: "junction", label: "Junctions" },
              { value: "copy", label: "Copy" },
            ]}
            value={strategy}
            onChange={setStrategy}
          />
          <Switch
            label="Crash reports"
            description="Opt-in. Sends crash data only; no analytics exist."
            checked={crashReports}
            onChange={(e) => setCrashReports(e.currentTarget.checked)}
          />
          <Group justify="flex-end">
            <Text size="xs" c="dimmed">
              StarVault CCM 0.1.0 · unofficial builds must self-declare
            </Text>
          </Group>
          <Button onClick={save} w="fit-content">
            Save
          </Button>
        </Stack>
      </Card>

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
