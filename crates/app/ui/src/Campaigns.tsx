import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
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

  const runPreflight = () => {
    invoke<PreflightReport>("launch_preflight").then(setReport).catch(onError);
  };

  const launch = async () => {
    try {
      await invoke("launch_game");
    } catch (e) {
      // Exe unusable: offer the Battle.net fallback.
      await invoke("launch_battlenet").catch(() => {});
      onError(String(e));
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
        <Button variant="filled" onClick={runPreflight}>
          Pre-flight check
        </Button>
        <Button variant="light" onClick={launch}>
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

const SLOT_TITLES: Record<string, string> = {
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
    try {
      await invoke("activate_campaign", { slot, id });
      refresh();
    } catch (e) {
      // Conflict errors name both packages (M5); show them as-is.
      setError(String(e));
    }
  };

  const restore = async (slot: string) => {
    setError(null);
    try {
      await invoke("restore_campaign", { slot });
      refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const optionsFor = (slot: string) => {
    const matching = library.filter((e) => e.slot === slot);
    const pool = matching.length > 0 ? matching : library;
    return pool.map((e) => ({ value: e.id, label: e.id }));
  };

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
              <Title order={4}>{SLOT_TITLES[entry.slot] ?? entry.slot}</Title>
              <Text c={entry.pkg_id ? undefined : "dimmed"}>{entry.title}</Text>
              {entry.author && (
                <Text size="xs" c="dimmed">
                  {entry.author}
                  {entry.version ? ` · ${entry.version}` : ""}
                </Text>
              )}
              <Group justify="space-between" mt="xs">
                <Button size="xs" variant="light" onClick={() => setPicking(entry.slot)}>
                  {entry.pkg_id ? "Replace…" : "Activate…"}
                </Button>
                <Button
                  size="xs"
                  variant="default"
                  disabled={!entry.pkg_id}
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

      <Modal
        opened={picking !== null}
        onClose={() => setPicking(null)}
        title={picking ? `Activate on ${SLOT_TITLES[picking]}` : ""}
        size="sm"
      >
        <Grid>
          {picking &&
            optionsFor(picking).map((opt) => (
              <Grid.Col span={12} key={opt.value}>
                <Button
                  variant="light"
                  fullWidth
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
