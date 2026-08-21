import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Alert,
  Badge,
  Card,
  Center,
  Group,
  Loader,
  SimpleGrid,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import ImportWizard from "./ImportWizard";

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

  const refresh = () => {
    invoke<LibraryEntry[]>("list_library")
      .then(setEntries)
      .catch((e) => setError(String(e)));
  };

  useEffect(refresh, []);
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
          {legacy.exe_hint ? ` (game: ${legacy.exe_hint})` : ""}. Migration arrives in a later
          release.
        </Alert>
      )}

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
              <Text size="xs" c="dimmed" ff="monospace">
                {entry.rev.slice(0, 12)}
              </Text>
            </Stack>
          </Card>
        ))}
      </SimpleGrid>
    </Stack>
  );
}
