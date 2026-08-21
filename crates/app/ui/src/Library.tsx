import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Alert,
  Badge,
  Button,
  Card,
  Center,
  Group,
  Loader,
  Modal,
  SimpleGrid,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import ImportWizard from "./ImportWizard";
import MigrationBanner from "./MigrationBanner";

interface LibraryEntry {
  id: string;
  rev: string;
  slot: string;
  active_on: string[];
}

interface LegacyCcmInstall {
  exe_hint: string | null;
}

export default function Library() {
  const [entries, setEntries] = useState<LibraryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [legacy, setLegacy] = useState<LegacyCcmInstall | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  const refresh = () => {
    invoke<LibraryEntry[]>("list_library")
      .then(setEntries)
      .catch((e) => setError(String(e)));
  };

  useEffect(refresh, []);

  const remove = async (id: string) => {
    setBusyId(id);
    setRemoving(null);
    try {
      await invoke("remove_package", { id });
      notifications.show({ color: "green", message: `${id} removed.` });
      refresh();
    } catch (e) {
      // Active packages refuse removal; the error says which faction.
      notifications.show({ color: "red", title: "Remove failed", message: String(e) });
    } finally {
      setBusyId(null);
    }
  };
  useEffect(() => {
    invoke<LegacyCcmInstall | null>("detect_legacy_ccm")
      .then(setLegacy)
      .catch(() => setLegacy(null));
  }, []);

  return (
    <Stack p="lg" gap="lg">
      <Group justify="space-between">
        <Title order={2}>Library</Title>
        <ImportWizard knownIds={new Set(entries?.map((e) => e.id) ?? [])} onImported={refresh} />
      </Group>

      {legacy && (
        <Alert title="Old SC2CCM install detected" color="yellow">
          A legacy config was found
          {legacy.exe_hint ? ` (game: ${legacy.exe_hint})` : ""}. Import your campaigns below.
        </Alert>
      )}

      <MigrationBanner onMigrated={refresh} />

      {error && (
        <Alert color="red" title="Error">
          {error}
        </Alert>
      )}

      {entries === null && !error && (
        <Center>
          <Loader size="sm" />
        </Center>
      )}

      {entries?.length === 0 && (
        <Text c="dimmed">No packages installed yet. Drop a campaign zip to import it.</Text>
      )}

      <SimpleGrid cols={{ base: 1, sm: 2, lg: 3 }} spacing="md">
        {entries?.map((entry) => (
          <Card key={`${entry.id}@${entry.rev}`} withBorder shadow="sm">
            <Stack gap="xs">
              <Text fw={600}>{entry.id}</Text>
              <Group gap="xs" justify="space-between">
                <Badge variant="light">{entry.slot}</Badge>
                {entry.active_on.length > 0 ? (
                  <Badge color="green">active</Badge>
                ) : (
                  <Badge color="gray">inactive</Badge>
                )}
              </Group>
              <Group justify="space-between">
                <Text size="xs" c="dimmed" ff="monospace">
                  {entry.rev.slice(0, 12)}
                </Text>
                <Button
                  size="compact-xs"
                  variant="subtle"
                  color="red"
                  disabled={busyId !== null}
                  onClick={() => setRemoving(entry.id)}
                >
                  Remove
                </Button>
              </Group>
            </Stack>
          </Card>
        ))}
      </SimpleGrid>

      <Modal
        opened={removing !== null}
        onClose={() => setRemoving(null)}
        title="Remove package?"
        size="sm"
      >
        <Stack gap="sm">
          <Text size="sm">
            Delete `{removing}` and its files from the store. Packages that are active on a faction
            must be restored first.
          </Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setRemoving(null)}>
              Cancel
            </Button>
            <Button
              color="red"
              loading={busyId !== null}
              onClick={() => removing && remove(removing)}
            >
              Remove
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
