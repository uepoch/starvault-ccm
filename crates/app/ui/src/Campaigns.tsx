import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { notifications } from "@mantine/notifications";
import ConflictDialog, { type ConflictDialogState } from "./ConflictDialog";
import { errConflict, errMessage } from "./errors";
import {
  Alert,
  Button,
  Card,
  Grid,
  Group,
  Modal,
  SimpleGrid,
  Stack,
  Text,
  Title,
} from "@mantine/core";

interface PreflightReport {
  exe_ok: boolean;
  no_running_instance: boolean;
  drift: string[];
}

function LaunchControls({ onError }: { onError: (msg: string) => void }) {
  const [report, setReport] = useState<PreflightReport | null>(null);
  const [repairing, setRepairing] = useState(false);
  const [checking, setChecking] = useState(false);
  const [launching, setLaunching] = useState(false);

  const runPreflight = () => {
    setChecking(true);
    invoke<PreflightReport>("launch_preflight")
      .then(setReport)
      .catch(onError)
      .finally(() => setChecking(false));
  };

  const launch = async () => {
    setLaunching(true);
    try {
      await invoke("launch_game");
    } catch (e) {
      // Exe unusable: offer the Battle.net fallback.
      await invoke("launch_battlenet").catch(() => {});
      onError(errMessage(e));
    } finally {
      setLaunching(false);
    }
  };

  const repair = async () => {
    setRepairing(true);
    try {
      await invoke("reconcile");
      runPreflight();
    } catch (e) {
      onError(String(e));
    } finally {
      setRepairing(false);
    }
  };

  return (
    <Stack gap="sm">
      <Group>
        <Button variant="filled" loading={checking} disabled={checking} onClick={runPreflight}>
          Pre-flight check
        </Button>
        <Button variant="light" loading={launching} disabled={launching} onClick={launch}>
          Launch StarCraft II
        </Button>
      </Group>
      {report && !(report.exe_ok && report.no_running_instance && report.drift.length === 0) && (
        <Alert color="yellow" title="Pre-flight found problems">
          <Stack gap="xs">
            {!report.exe_ok && <Text size="sm">Game executable not found.</Text>}
            {!report.no_running_instance && <Text size="sm">StarCraft II is already running.</Text>}
            {report.drift.map((d) => (
              <Text key={d} size="sm">
                {d}
              </Text>
            ))}
            {report.drift.length > 0 && (
              <Button
                size="xs"
                variant="light"
                loading={repairing}
                onClick={repair}
                w="fit-content"
              >
                Repair
              </Button>
            )}
          </Stack>
        </Alert>
      )}
      {report?.exe_ok && report.no_running_instance && report.drift.length === 0 && (
        <Alert color="green">All clear. Ready to launch.</Alert>
      )}
    </Stack>
  );
}

interface CampaignSlot {
  slot: string;
  title: string;
  pkg_id: string | null;
  rev: string | null;
  author: string | null;
  version: string | null;
}

interface LibraryEntry {
  id: string;
  slot: string;
}

const FACTION_TITLES: Record<string, string> = {
  wol: "Wings of Liberty",
  hots: "Heart of the Swarm",
  lotv: "Legacy of the Void",
  nco: "Nova Covert Ops",
};

export default function Campaigns() {
  const [slots, setSlots] = useState<CampaignSlot[] | null>(null);
  const [library, setLibrary] = useState<LibraryEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [conflict, setConflict] = useState<ConflictDialogState | null>(null);

  const refresh = () => {
    invoke<CampaignSlot[]>("list_campaigns")
      .then(setSlots)
      .catch((e) => setError(String(e)));
    invoke<LibraryEntry[]>("list_library")
      .then(setLibrary)
      .catch(() => {});
  };

  useEffect(refresh, []);

  const activate = async (slot: string, id: string) => {
    setError(null);
    setPicking(null);
    setBusy(`activate-${slot}`);
    try {
      await invoke("activate_campaign", { slot, id });
      notifications.show({
        color: "green",
        message: `${id} activated on ${FACTION_TITLES[slot] ?? slot}.`,
      });
      refresh();
    } catch (e) {
      const conflictInfo = errConflict(e);
      if (conflictInfo) {
        // M5 dialog: name both packages and the conflicting path; offer to
        // clear the other faction.
        setConflict({ info: conflictInfo, retrySlot: slot, retryId: id });
      } else {
        setError(errMessage(e));
        notifications.show({ color: "red", title: "Activation failed", message: errMessage(e) });
      }
    } finally {
      setBusy(null);
    }
  };

  const restore = async (slot: string) => {
    setError(null);
    setBusy(`restore-${slot}`);
    try {
      await invoke("restore_campaign", { slot });
      notifications.show({
        color: "green",
        message: `${FACTION_TITLES[slot] ?? slot} restored to plain.`,
      });
      refresh();
    } catch (e) {
      setError(String(e));
      notifications.show({ color: "red", title: "Restore failed", message: String(e) });
    } finally {
      setBusy(null);
    }
  };

  const optionsFor = (slot: string) =>
    library.filter((e) => e.slot === slot).map((e) => ({ value: e.id, label: e.id }));

  return (
    <Stack p="lg" gap="lg">
      <Title order={2}>Campaigns</Title>

      <LaunchControls onError={setError} />

      {error && (
        <Alert color="red" title="Operation failed" withCloseButton onClose={() => setError(null)}>
          {error}
        </Alert>
      )}

      <SimpleGrid cols={{ base: 1, sm: 2 }} spacing="md">
        {(slots ?? []).map((entry) => (
          <Card key={entry.slot} withBorder shadow="sm">
            <Stack gap="xs">
              <Title order={4}>{FACTION_TITLES[entry.slot] ?? entry.slot}</Title>
              <Text c={entry.pkg_id ? undefined : "dimmed"}>{entry.title}</Text>
              {entry.author && (
                <Text size="xs" c="dimmed">
                  {entry.author}
                  {entry.version ? ` · ${entry.version}` : ""}
                </Text>
              )}
              <Group justify="space-between" mt="xs">
                <Button
                  size="xs"
                  variant="light"
                  disabled={busy !== null}
                  onClick={() => setPicking(entry.slot)}
                >
                  {entry.pkg_id ? "Replace…" : "Activate…"}
                </Button>
                <Button
                  size="xs"
                  variant="default"
                  disabled={!entry.pkg_id || busy !== null}
                  loading={busy === `restore-${entry.slot}`}
                  onClick={() => restore(entry.slot)}
                >
                  Restore to plain
                </Button>
              </Group>
            </Stack>
          </Card>
        ))}
      </SimpleGrid>

      {slots === null && !error && <Text c="dimmed">Loading…</Text>}

      <ConflictDialog state={conflict} onClose={() => setConflict(null)} onDone={refresh} />

      <Modal
        opened={picking !== null}
        onClose={() => setPicking(null)}
        title={picking ? `Activate on ${FACTION_TITLES[picking]}` : ""}
        size="sm"
      >
        <Grid>
          {picking &&
            optionsFor(picking).map((opt) => (
              <Grid.Col span={12} key={opt.value}>
                <Button
                  variant="light"
                  fullWidth
                  loading={busy === `activate-${picking}`}
                  disabled={busy !== null}
                  onClick={() => picking && activate(picking, opt.value)}
                >
                  {opt.label}
                </Button>
              </Grid.Col>
            ))}
          {picking && optionsFor(picking).length === 0 && (
            <Text c="dimmed">No packages installed. Import one first.</Text>
          )}
        </Grid>
      </Modal>
    </Stack>
  );
}
