import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Alert, Button, Group, Select, Stack, Text } from "@mantine/core";
import { SLOTS } from "./factions";

interface Candidate {
  path: string;
  name: string;
}

/// Per-campaign import list for an old SC2CCM install (P2). Old files stay
/// in place; cleanup is manual and documented.
export default function MigrationBanner({
  onMigrated,
  legacy,
}: {
  onMigrated: () => void;
  legacy: { exe_hint: string | null } | null;
}) {
  const [candidates, setCandidates] = useState<Candidate[] | null>(null);
  const [slots, setSlots] = useState<Record<string, string>>({});
  const [done, setDone] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<Candidate[]>("list_migration_candidates")
      .then(setCandidates)
      .catch(() => setCandidates([]));
  }, []);

  // Old install detected but Maps\Campaign holds no importable folders:
  // explain why there is nothing to do and how to dismiss.
  if (legacy && candidates?.length === 0) {
    return (
      <Alert title="Old SC2CCM install detected" color="yellow">
        A legacy SC2CCM config was found
        {legacy.exe_hint ? ` (game: ${legacy.exe_hint})` : ""}, but no old campaign folders in the
        game's Maps\Campaign directory. Nothing to import — delete %APPDATA%\SC2CCM\SC2CCM.txt to
        dismiss this.
      </Alert>
    );
  }
  if (!candidates || candidates.length === 0) return null;

  const migrate = async (candidate: Candidate) => {
    const slot = slots[candidate.path];
    if (!slot) return;
    const id = candidate.name
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-|-$/g, "");
    if (!id) return; // nothing slugifiable in the name
    try {
      await invoke("migrate_candidate", { path: candidate.path, id, slot });
      setDone((d) => [...d, candidate.path]);
      onMigrated();
    } catch (e) {
      setError(String(e));
    }
  };

  const remaining = candidates.filter((c) => !done.includes(c.path));

  return (
    <Alert title="Old SC2CCM campaigns found" color="yellow">
      <Stack gap="sm">
        <Text size="sm">
          These are campaigns from your old SC2CCM install. Pick the faction each was built for,
          then Import: the campaign is copied into the library as a normal package (detected
          metadata, editable, playable) and the originals stay untouched.
        </Text>
        {error && (
          <Text c="red" size="sm">
            {error}
          </Text>
        )}
        {remaining.map((c) => (
          <Group key={c.path} justify="space-between">
            <Text size="sm">{c.name}</Text>
            <Group gap="xs">
              <Select
                placeholder="Slot"
                data={SLOTS}
                w={110}
                value={slots[c.path] ?? null}
                onChange={(v) => setSlots((s) => ({ ...s, [c.path]: v ?? "" }))}
              />
              <Button
                size="xs"
                variant="light"
                disabled={!slots[c.path]}
                onClick={() => migrate(c)}
              >
                Import
              </Button>
            </Group>
          </Group>
        ))}
        {done.length > 0 && (
          <Text size="xs" c="dimmed">
            Imported: {done.map((p) => p.split(/[\\/]/).pop()).join(", ")}
          </Text>
        )}
      </Stack>
    </Alert>
  );
}
