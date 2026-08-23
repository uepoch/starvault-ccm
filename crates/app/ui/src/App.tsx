import { useCallback, useEffect, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  Alert,
  Box,
  Button,
  Center,
  Loader,
  MantineProvider,
  Stack,
  Tabs,
  createTheme,
} from "@mantine/core";
import { Notifications, notifications } from "@mantine/notifications";
import "@mantine/core/styles.css";
import "@mantine/notifications/styles.css";
import ChangelogButton from "./ChangelogButton";
import { toCommandError } from "./errors";
import { discoverGameExe, getConfig, initialize, saveConfig } from "./ipc";
import Library from "./Library";
import Log from "./Log";
import Settings from "./Settings";
import type { CommandError, StartupReport } from "./types";

const theme = createTheme({
  primaryColor: "blue",
  defaultRadius: "sm",
});

function AppShell() {
  const [tab, setTab] = useState<string | null>("library");
  const [pendingZip, setPendingZip] = useState<string | null>(null);
  const [startup, setStartup] = useState<StartupReport | null>(null);
  const [startupError, setStartupError] = useState<CommandError | null>(null);

  const beginInitialization = useCallback(async () => {
    setStartupError(null);
    try {
      const report = await initialize();
      setStartup(report);
    } catch (error) {
      setStartupError(toCommandError(error));
      return;
    }

    try {
      const config = await getConfig();
      if (config.game_exe) return;
      const found = await discoverGameExe();
      if (!found) return;
      await saveConfig({
        gameExe: found,
        strategyOverride: config.strategy_override,
        crashReportsOptIn: config.crash_reports_opt_in,
        logLevel: config.log_level,
        saveIsolation: config.save_isolation,
        savesProfile: config.saves_profile,
        replaceExternalMods: config.replace_external_mods,
      });
    } catch {
      // Install discovery is a convenience. Settings remains available when
      // discovery or its config write fails.
    }
  }, []);

  useEffect(() => {
    void beginInitialization();
  }, [beginInitialization]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type !== "drop") return;
        const zip = event.payload.paths.find((path) => path.toLowerCase().endsWith(".zip"));
        if (!zip) return;
        notifications.show({
          color: "blue",
          message: `Importing ${zip.split(/[\\/]/).pop()}…`,
        });
        setTab("library");
        setPendingZip(zip);
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, []);

  if (startupError) {
    return (
      <Center h="100vh" p="lg">
        <Alert color="red" title="StarVault could not start" maw={560}>
          <Stack gap="sm">
            <span>{startupError.message}</span>
            <Button variant="light" color="red" w="fit-content" onClick={beginInitialization}>
              Try again
            </Button>
          </Stack>
        </Alert>
      </Center>
    );
  }

  if (!startup) {
    return (
      <Center h="100vh" aria-label="Starting StarVault">
        <Loader size="sm" />
      </Center>
    );
  }

  return (
    <Tabs value={tab} onChange={setTab} keepMounted={false}>
      <Tabs.List px="lg" pt="sm">
        <Tabs.Tab value="library">Library</Tabs.Tab>
        <Tabs.Tab value="log">Log</Tabs.Tab>
        <Tabs.Tab value="settings">Settings</Tabs.Tab>
        <Box ml="auto">
          <ChangelogButton />
        </Box>
      </Tabs.List>

      {startup.notes.length > 0 && (
        <Alert color={startup.recovery_performed ? "yellow" : "blue"} mx="lg" mt="sm">
          {startup.notes.join(" ")}
        </Alert>
      )}

      <Tabs.Panel value="library">
        <Library pendingZip={pendingZip} onZipConsumed={() => setPendingZip(null)} />
      </Tabs.Panel>
      <Tabs.Panel value="log">
        <Log />
      </Tabs.Panel>
      <Tabs.Panel value="settings">
        <Settings />
      </Tabs.Panel>
    </Tabs>
  );
}

export default function App() {
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Notifications position="top-right" />
      <AppShell />
    </MantineProvider>
  );
}
