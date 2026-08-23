import { useEffect, useState } from "react";
import { Alert, Button, Group, Select, Stack, Text } from "@mantine/core";
import { errMessage } from "./errors";
import { SLOTS } from "./factions";
import { listMigrationCandidates, migrateCandidate } from "./ipc";
import type { MigrationCandidate } from "./types";

export default function MigrationBanner({
  disabled = false,
  onMigrated,
}: {
  disabled?: boolean;
  onMigrated: () => void;
}) {
  const [candidates, setCandidates] = useState<MigrationCandidate[] | null>(null);
  const [factions, setFactions] = useState<Record<string, string>>({});
  const [done, setDone] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    listMigrationCandidates()
      .then(setCandidates)
      .catch(() => setCandidates([]));
  }, []);

  if (!candidates || candidates.length === 0) return null;

  const migrate = async (candidate: MigrationCandidate) => {
    if (disabled) return;
    const faction = factions[candidate.candidate_id];
    if (!faction) return;
    const id = candidate.name
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-|-$/g, "");
    if (!id) return;
    try {
      await migrateCandidate(candidate.candidate_id, id, faction);
      setDone((current) => [...current, candidate.candidate_id]);
      setError(null);
      onMigrated();
    } catch (migrationError) {
      setError(errMessage(migrationError));
    }
  };

  const remaining = candidates.filter((candidate) => !done.includes(candidate.candidate_id));
  const importedNames = candidates
    .filter((candidate) => done.includes(candidate.candidate_id))
    .map((candidate) => candidate.name);

  return (
    <Alert title="Old SC2CCM campaigns found" color="yellow">
      <Stack gap="sm">
        <Text size="sm">
          Choose the faction each campaign was built for, then import it. StarVault copies it into
          the Library and leaves the original untouched.
        </Text>
        {error && (
          <Text c="red" size="sm">
            {error}
          </Text>
        )}
        {remaining.map((candidate) => (
          <Group key={candidate.candidate_id} justify="space-between">
            <Text size="sm">{candidate.name}</Text>
            <Group gap="xs">
              <Select
                placeholder="Faction"
                data={SLOTS}
                w={110}
                disabled={disabled}
                value={factions[candidate.candidate_id] ?? null}
                onChange={(value) =>
                  setFactions((current) => ({
                    ...current,
                    [candidate.candidate_id]: value ?? "",
                  }))
                }
              />
              <Button
                size="xs"
                variant="light"
                disabled={disabled || !factions[candidate.candidate_id]}
                onClick={() => migrate(candidate)}
              >
                Import
              </Button>
            </Group>
          </Group>
        ))}
        {importedNames.length > 0 && (
          <Text size="xs" c="dimmed">
            Imported: {importedNames.join(", ")}
          </Text>
        )}
      </Stack>
    </Alert>
  );
}
