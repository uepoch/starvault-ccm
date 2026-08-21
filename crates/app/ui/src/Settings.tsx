import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Button,
  Card,
  Group,
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
          <Button variant="light" size="xs" onClick={browse} w="fit-content">
            Browse…
          </Button>
          <Select
            label="Slot strategy"
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
    </Stack>
  );
}
