import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { notifications } from "@mantine/notifications";
import ConflictDialog, { type ConflictDialogState } from "./ConflictDialog";
import { FACTION_COLORS, FACTION_TITLES, FACTION_NAMES } from "./factions";
import { errConflict, errMessage } from "./errors";
import type { LibraryEntry } from "./types";
import {
  Alert,
  Badge,
  Button,
  Card,
  Group,
  Loader,
  Modal,
  ScrollArea,
  SimpleGrid,
  Stack,
  Text,
  TextInput,
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

export default function Campaigns() {
  const [slots, setSlots] = useState<CampaignSlot[] | null>(null);
  const [library, setLibrary] = useState<LibraryEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState<string | null>(null);
  const [pickQuery, setPickQuery] = useState("");
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

  const pickOptionsFor = (slot: string) => {
    const q = pickQuery.trim().toLowerCase();
    return library
      .filter((e) => e.slot === slot)
      .filter(
        (e) =>
          !q ||
          e.id.toLowerCase().includes(q) ||
          (e.title ?? "").toLowerCase().includes(q) ||
          (e.author ?? "").toLowerCase().includes(q),
      )
      .map((e) => ({ id: e.id, title: e.title ?? e.id, author: e.author }));
  };

  return (
    <Stack p="lg" gap="lg" h="calc(100vh - 50px)">
      <Title order={2}>Campaigns</Title>

      <LaunchControls onError={setError} />

      {error && (
        <Alert color="red" title="Operation failed" withCloseButton onClose={() => setError(null)}>
          {error}
        </Alert>
      )}

      <SimpleGrid cols={{ base: 1, sm: 2 }} spacing="md" style={{ flex: 1 }}>
        {(slots ?? []).map((entry) => (
          <Card key={entry.slot} withBorder shadow="sm" h="100%">
            <Stack gap="xs">
              <Group justify="space-between">
                <Title order={4}>{FACTION_NAMES[entry.slot] ?? entry.slot}</Title>
                <Badge variant="light" color={FACTION_COLORS[entry.slot] ?? "gray"}>
                  {FACTION_TITLES[entry.slot] ?? entry.slot}
                </Badge>
              </Group>
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
                  variant="subtle"
                  disabled={busy !== null}
                  onClick={() => setPicking(entry.slot)}
                >
                  {entry.pkg_id ? "Replace…" : "Activate…"}
                </Button>
                <Button
                  size="xs"
                  variant="subtle"
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
        <Stack gap="sm">
          <style>{`
            .svccm-pick-row:hover {
              background: light-dark(var(--mantine-color-gray-1), var(--mantine-color-dark-6));
            }
          `}</style>
          <TextInput
            data-autofocus
            placeholder="Search by title, author, id…"
            value={pickQuery}
            onChange={(e) => setPickQuery(e.currentTarget.value)}
          />
          <ScrollArea h={280} type="auto">
            <Stack gap={4}>
              {picking &&
                pickOptionsFor(picking).map((opt) => (
                  <Group
                    key={opt.id}
                    justify="space-between"
                    wrap="nowrap"
                    px="sm"
                    py={6}
                    style={{
                      borderRadius: 4,
                      cursor: busy !== null ? "wait" : "pointer",
                      opacity: busy !== null ? 0.6 : 1,
                    }}
                    className="svccm-pick-row"
                    onClick={() => picking && busy === null && activate(picking, opt.id)}
                  >
                    <Stack gap={0} miw={0} style={{ flex: 1 }}>
                      <Text size="sm" truncate="end">
                        {opt.title}
                      </Text>
                      <Text size="xs" c="dimmed" truncate="end">
                        {opt.author ? `${opt.author} · ` : ""}
                        {opt.id}
                      </Text>
                    </Stack>
                    {busy === `activate-${picking}` ? (
                      <Loader size="xs" color={FACTION_COLORS[picking]} />
                    ) : null}
                  </Group>
                ))}
              {picking && pickOptionsFor(picking).length === 0 && (
                <Text c="dimmed" size="sm" ta="center" mt="md">
                  {pickQuery ? "No matches." : "No packages for this faction. Import one first."}
                </Text>
              )}
            </Stack>
          </ScrollArea>
        </Stack>
      </Modal>
    </Stack>
  );
}
