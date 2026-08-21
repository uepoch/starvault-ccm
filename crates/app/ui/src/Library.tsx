import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Badge,
  Button,
  Card,
  Center,
  Group,
  Loader,
  Modal,
  Select,
  Stack,
  Table,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { IconPlayerPlay, IconTrash } from "@tabler/icons-react";
import {
  createColumnHelper,
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getSortedRowModel,
  useReactTable,
  type ColumnFiltersState,
  type SortingState,
} from "@tanstack/react-table";
import ImportWizard from "./ImportWizard";
import MigrationBanner from "./MigrationBanner";

interface LibraryEntry {
  id: string;
  rev: string;
  slot: string;
  active_on: string[];
  title: string | null;
  author: string | null;
  version: string | null;
  desc: string | null;
  imported_at: number | null;
}

interface LegacyCcmInstall {
  exe_hint: string | null;
}

const FACTION_TITLES: Record<string, string> = {
  wol: "WoL",
  hots: "HotS",
  lotv: "LotV",
  nco: "NCO",
};

const columnHelper = createColumnHelper<LibraryEntry>();

function formatDate(epoch: number | null): string {
  if (!epoch) return "—";
  return new Date(epoch * 1000).toISOString().slice(0, 10);
}

export default function Library() {
  const [entries, setEntries] = useState<LibraryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [legacy, setLegacy] = useState<LegacyCcmInstall | null>(null);
  const [removing, setRemoving] = useState<LibraryEntry | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const [factionFilter, setFactionFilter] = useState<string | null>(null);
  const [sorting, setSorting] = useState<SortingState>([]);
  const [columnFilters, setColumnFilters] = useState<ColumnFiltersState>([]);

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

  const activate = async (entry: LibraryEntry) => {
    setBusyId(entry.id);
    try {
      await invoke("activate_campaign", { slot: entry.slot, id: entry.id });
      notifications.show({
        color: "green",
        message: `${entry.id} activated on ${FACTION_TITLES[entry.slot] ?? entry.slot}.`,
      });
      refresh();
    } catch (e) {
      notifications.show({ color: "red", title: "Activation failed", message: String(e) });
    } finally {
      setBusyId(null);
    }
  };

  const remove = async (entry: LibraryEntry) => {
    setBusyId(entry.id);
    setRemoving(null);
    try {
      await invoke("remove_package", { id: entry.id });
      notifications.show({ color: "green", message: `${entry.id} removed.` });
      refresh();
    } catch (e) {
      notifications.show({ color: "red", title: "Remove failed", message: String(e) });
    } finally {
      setBusyId(null);
    }
  };

  const columns = useMemo(
    () => [
      columnHelper.accessor((e) => e.title ?? e.id, {
        id: "title",
        header: "Title",
        cell: (info) => (
          <Stack gap={0}>
            <Text fw={500}>{info.getValue()}</Text>
            <Text size="xs" c="dimmed">
              {info.row.original.version ?? info.row.original.id}
            </Text>
          </Stack>
        ),
      }),
      columnHelper.accessor("author", {
        header: "Author",
        cell: (info) => info.getValue() ?? "—",
      }),
      columnHelper.accessor("slot", {
        header: "Faction",
        cell: (info) => (
          <Badge variant="light">{FACTION_TITLES[info.getValue()] ?? info.getValue()}</Badge>
        ),
      }),
      columnHelper.accessor("imported_at", {
        header: "Imported",
        cell: (info) => formatDate(info.getValue()),
      }),
      columnHelper.display({
        id: "actions",
        header: "",
        cell: (info) => {
          const entry = info.row.original;
          const active = entry.active_on.length > 0;
          const busy = busyId === entry.id;
          return (
            <Group gap="xs" wrap="nowrap">
              <Button
                size="compact-sm"
                variant={active ? "filled" : "default"}
                color="green"
                disabled={active || busy}
                loading={busy}
                leftSection={<IconPlayerPlay size={14} />}
                onClick={() => activate(entry)}
              >
                Play
              </Button>
              <Button
                size="compact-sm"
                variant="subtle"
                color="red"
                disabled={busy}
                onClick={() => setRemoving(entry)}
              >
                <IconTrash size={14} />
              </Button>
            </Group>
          );
        },
      }),
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [busyId],
  );

  const table = useReactTable({
    data: entries ?? [],
    columns,
    state: { sorting, columnFilters, globalFilter: search },
    onSortingChange: setSorting,
    onColumnFiltersChange: setColumnFilters,
    onGlobalFilterChange: setSearch,
    globalFilterFn: (row, _columnId, value) => {
      const e = row.original as LibraryEntry;
      const hay = `${e.title ?? ""} ${e.author ?? ""} ${e.desc ?? ""} ${e.id}`.toLowerCase();
      return hay.includes(value.toLowerCase());
    },
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
  });

  useEffect(() => {
    setColumnFilters(factionFilter ? [{ id: "slot", value: factionFilter }] : []);
  }, [factionFilter]);

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

      <Group align="flex-end">
        <TextInput
          placeholder="Search title, author, description…"
          leftSection={<Text size="sm">⌕</Text>}
          value={search}
          onChange={(e) => setSearch(e.currentTarget.value)}
          w={320}
        />
        <Select
          placeholder="Faction"
          clearable
          data={[
            { value: "wol", label: "WoL" },
            { value: "hots", label: "HotS" },
            { value: "lotv", label: "LotV" },
            { value: "nco", label: "NCO" },
          ]}
          value={factionFilter}
          onChange={setFactionFilter}
          w={140}
        />
      </Group>

      {entries === null && !error && (
        <Center>
          <Loader size="sm" />
        </Center>
      )}

      {entries !== null && table.getRowModel().rows.length === 0 && (
        <Text c="dimmed">No packages match. Drop a campaign zip to import it.</Text>
      )}

      {table.getRowModel().rows.length > 0 && (
        <Card withBorder p={0}>
          <Table highlightOnHover verticalSpacing="sm">
            <Table.Thead>
              {table.getHeaderGroups().map((hg) => (
                <Table.Tr key={hg.id}>
                  {hg.headers.map((header) => (
                    <Table.Th
                      key={header.id}
                      onClick={header.column.getToggleSortingHandler()}
                      style={{ cursor: header.column.getCanSort() ? "pointer" : undefined }}
                    >
                      {flexRender(header.column.columnDef.header, header.getContext())}
                      {{ asc: " ↑", desc: " ↓" }[header.column.getIsSorted() as string] ?? ""}
                    </Table.Th>
                  ))}
                </Table.Tr>
              ))}
            </Table.Thead>
            <Table.Tbody>
              {table.getRowModel().rows.map((row) => (
                <Table.Tr key={row.id}>
                  {row.getVisibleCells().map((cell) => (
                    <Table.Td key={cell.id}>
                      {flexRender(cell.column.columnDef.cell, cell.getContext())}
                    </Table.Td>
                  ))}
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </Card>
      )}

      <Modal
        opened={removing !== null}
        onClose={() => setRemoving(null)}
        title="Remove package?"
        size="sm"
      >
        <Stack gap="sm">
          <Text size="sm">
            Delete `{removing?.id}` and its files from the store. Packages that are active on a
            faction must be restored first.
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
