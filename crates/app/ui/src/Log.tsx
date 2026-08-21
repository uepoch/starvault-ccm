import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button, Group, Stack, Table, Text, Title } from "@mantine/core";

interface LogEntry {
  time: string;
  kind: string;
  detail: string;
}

/// Epoch-seconds stamps from the core, rendered readably.
function formatTime(epochSecs: string): string {
  const n = Number(epochSecs);
  if (!Number.isFinite(n)) return epochSecs;
  return new Date(n * 1000).toISOString().replace("T", " ").slice(0, 19) + " UTC";
}

export default function Log() {
  const [entries, setEntries] = useState<LogEntry[]>([]);

  const refresh = () => invoke<LogEntry[]>("read_log", { limit: 500 }).then(setEntries);

  useEffect(() => {
    void refresh();
  }, []);

  return (
    <Stack p="lg" gap="lg">
      <Group justify="space-between">
        <Title order={2}>Log</Title>
        <Button variant="light" size="xs" onClick={refresh}>
          Refresh
        </Button>
      </Group>

      {entries.length === 0 ? (
        <Text c="dimmed">No operations yet. Imports and switches appear here.</Text>
      ) : (
        <Table highlightOnHover>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Time</Table.Th>
              <Table.Th>Operation</Table.Th>
              <Table.Th>Detail</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {entries.map((entry, i) => (
              <Table.Tr key={i}>
                <Table.Td style={{ whiteSpace: "nowrap" }}>{formatTime(entry.time)}</Table.Td>
                <Table.Td>{entry.kind}</Table.Td>
                <Table.Td style={{ whiteSpace: "pre-wrap" }}>{entry.detail}</Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}
    </Stack>
  );
}
