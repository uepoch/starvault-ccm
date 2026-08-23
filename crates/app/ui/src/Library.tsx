import { useCallback, useEffect, useState } from "react";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Badge,
  Button,
  Card,
  Center,
  Checkbox,
  Group,
  Loader,
  Modal,
  Select,
  Stack,
  Table,
  Text,
  Textarea,
  TextInput,
  Title,
  Tooltip,
  UnstyledButton,
} from "@mantine/core";
import {
  IconCircleCheck,
  IconFolder,
  IconPencil,
  IconPlayerPlay,
  IconRestore,
  IconTrash,
} from "@tabler/icons-react";
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
import { REPAIRABLE_ERROR_CODES, toCommandError } from "./errors";
import { FACTION_COLORS, FACTION_NAMES, FACTION_TITLES, SLOTS } from "./factions";
import ImportWizard from "./ImportWizard";
import {
  activatePackage,
  editPackageMetadata,
  listLibrary,
  playPackage,
  removePackage,
  repairActive,
  restoreVanilla,
  revealPackage,
} from "./ipc";
import MigrationBanner from "./MigrationBanner";
import type { CommandError, LibraryEntry, LibrarySnapshot } from "./types";

const columnHelper = createColumnHelper<LibraryEntry>();

const lastView = {
  search: "",
  factionFilter: null as string | null,
  sorting: [] as SortingState,
  columnFilters: [] as ColumnFiltersState,
};

function formatDate(epoch: number | null): string {
  if (!epoch) return "—";
  return new Date(epoch * 1000).toISOString().slice(0, 10);
}

export default function Library({
  pendingZip,
  onZipConsumed,
}: {
  pendingZip: string | null;
  onZipConsumed: () => void;
}) {
  const [snapshot, setSnapshot] = useState<LibrarySnapshot | null>(null);
  const [loadError, setLoadError] = useState<CommandError | null>(null);
  const [operationError, setOperationError] = useState<CommandError | null>(null);
  const [removing, setRemoving] = useState<LibraryEntry | null>(null);
  const [editing, setEditing] = useState<LibraryEntry | null>(null);
  const [editTitle, setEditTitle] = useState("");
  const [editAuthor, setEditAuthor] = useState("");
  const [editVersion, setEditVersion] = useState("");
  const [editDesc, setEditDesc] = useState("");
  const [saving, setSaving] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [pageBusy, setPageBusy] = useState<string | null>(null);
  const [externalConflict, setExternalConflict] = useState<{
    action: "activate" | "play";
    entry: Pick<LibraryEntry, "id" | "title">;
    error: CommandError;
  } | null>(null);
  const [rememberExternalMods, setRememberExternalMods] = useState(false);
  const [search, setSearch] = useState(lastView.search);
  const [factionFilter, setFactionFilter] = useState<string | null>(lastView.factionFilter);
  const [sorting, setSorting] = useState<SortingState>(lastView.sorting);
  const [columnFilters, setColumnFilters] = useState<ColumnFiltersState>(lastView.columnFilters);

  useEffect(() => {
    lastView.search = search;
    lastView.factionFilter = factionFilter;
    lastView.sorting = sorting;
    lastView.columnFilters = columnFilters;
  }, [search, factionFilter, sorting, columnFilters]);

  const refresh = useCallback(async () => {
    try {
      setSnapshot(await listLibrary());
      setLoadError(null);
    } catch (error) {
      setLoadError(toCommandError(error));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const active = snapshot?.active_campaign ?? null;
  const activeEntry = snapshot?.entries.find((entry) => entry.id === active?.id) ?? null;
  const mutationsBlocked = snapshot === null || snapshot.health.state === "recovery_required";

  const openEdit = (entry: LibraryEntry) => {
    setEditing(entry);
    setEditTitle(entry.title ?? "");
    setEditAuthor(entry.author ?? "");
    setEditVersion(entry.version ?? "");
    setEditDesc(entry.desc ?? "");
  };

  const saveEdit = async () => {
    if (!editing) return;
    setSaving(true);
    try {
      await editPackageMetadata({
        id: editing.id,
        title: editTitle,
        author: editAuthor,
        version: editVersion,
        desc: editDesc,
      });
      notifications.show({ color: "green", message: `Metadata for ${editing.id} updated.` });
      setEditing(null);
      await refresh();
    } catch (error) {
      setOperationError(toCommandError(error));
    } finally {
      setSaving(false);
    }
  };

  const activate = async (entry: LibraryEntry) => {
    setBusyId(entry.id);
    setOperationError(null);
    try {
      await activatePackage(entry.id);
      notifications.show({ color: "green", message: `${entry.title ?? entry.id} is active.` });
      await refresh();
    } catch (error) {
      const commandError = toCommandError(error);
      if (commandError.code === "external_mods_conflict") {
        setRememberExternalMods(false);
        setExternalConflict({ action: "activate", entry, error: commandError });
      } else {
        setOperationError(commandError);
      }
    } finally {
      setBusyId(null);
    }
  };

  const retryWithExternalReplacement = async () => {
    if (!externalConflict) return;
    const { action, entry } = externalConflict;
    setBusyId(entry.id);
    setOperationError(null);
    try {
      const options = {
        replaceExternalMods: true,
        rememberExternalMods,
      };
      if (action === "activate") {
        await activatePackage(entry.id, options);
      } else {
        await playPackage(entry.id, options);
      }
      notifications.show({
        color: "green",
        message:
          action === "activate"
            ? `${entry.title ?? entry.id} is active.`
            : `${entry.title ?? entry.id} is launching.`,
      });
      setExternalConflict(null);
      await refresh();
    } catch (error) {
      const commandError = toCommandError(error);
      setExternalConflict(null);
      setOperationError(commandError);
    } finally {
      setBusyId(null);
    }
  };

  const play = async (entry: Pick<LibraryEntry, "id" | "title">) => {
    setBusyId(entry.id);
    setOperationError(null);
    try {
      await playPackage(entry.id);
      notifications.show({ color: "green", message: `${entry.title ?? entry.id} is launching.` });
      await refresh();
    } catch (error) {
      const commandError = toCommandError(error);
      if (commandError.code === "external_mods_conflict") {
        setRememberExternalMods(false);
        setExternalConflict({ action: "play", entry, error: commandError });
      } else {
        setOperationError(commandError);
      }
    } finally {
      setBusyId(null);
    }
  };

  const returnToVanilla = async () => {
    setPageBusy("restore");
    setOperationError(null);
    try {
      await restoreVanilla();
      notifications.show({ color: "green", message: "Returned to vanilla." });
      await refresh();
    } catch (error) {
      setOperationError(toCommandError(error));
    } finally {
      setPageBusy(null);
    }
  };

  const repair = async () => {
    setPageBusy("repair");
    setOperationError(null);
    try {
      await repairActive();
      notifications.show({ color: "green", message: "The active campaign was repaired." });
      await refresh();
    } catch (error) {
      setOperationError(toCommandError(error));
    } finally {
      setPageBusy(null);
    }
  };

  const reveal = async (entry: LibraryEntry) => {
    try {
      await revealPackage(entry.id);
    } catch (error) {
      setOperationError(toCommandError(error));
    }
  };

  const remove = async (entry: LibraryEntry) => {
    setBusyId(entry.id);
    setRemoving(null);
    setOperationError(null);
    try {
      await removePackage(entry.id);
      notifications.show({ color: "green", message: `${entry.id} removed.` });
      await refresh();
    } catch (error) {
      setOperationError(toCommandError(error));
    } finally {
      setBusyId(null);
    }
  };

  const columns = [
    columnHelper.accessor((entry) => entry.title ?? entry.id, {
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
    columnHelper.accessor("faction", {
      header: "Faction",
      cell: (info) => (
        <Badge variant="light" color={FACTION_COLORS[info.getValue()] ?? "gray"}>
          {FACTION_TITLES[info.getValue()] ?? info.getValue()}
        </Badge>
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
        const isActive = active?.id === entry.id;
        const busy = busyId === entry.id;
        const disabled = busy || pageBusy !== null || mutationsBlocked;
        return (
          <Group gap="xs" wrap="nowrap" justify="flex-end">
            <Button
              size="compact-sm"
              variant={isActive ? "default" : "light"}
              color="gray"
              disabled={isActive || disabled}
              leftSection={isActive ? <IconCircleCheck size={14} /> : undefined}
              onClick={() => activate(entry)}
            >
              {isActive ? "Active" : "Activate"}
            </Button>
            <Button
              size="compact-sm"
              variant="light"
              loading={busy}
              disabled={disabled}
              leftSection={<IconPlayerPlay size={14} />}
              onClick={() => play(entry)}
            >
              Play
            </Button>
            <Tooltip label="Edit metadata" openDelay={300}>
              <Button
                size="compact-sm"
                variant="subtle"
                color="gray"
                disabled={disabled}
                px={5}
                onClick={() => openEdit(entry)}
                aria-label={`Edit ${entry.id} metadata`}
              >
                <IconPencil size={16} />
              </Button>
            </Tooltip>
            <Tooltip label="Open package folder" openDelay={300}>
              <Button
                size="compact-sm"
                variant="subtle"
                color="gray"
                disabled={disabled}
                px={5}
                onClick={() => reveal(entry)}
                aria-label={`Open ${entry.id} folder`}
              >
                <IconFolder size={16} />
              </Button>
            </Tooltip>
            <Tooltip
              label={isActive ? "Return to vanilla before removing this package." : "Remove"}
              openDelay={300}
            >
              <span>
                <Button
                  size="compact-sm"
                  variant="subtle"
                  color="red"
                  disabled={isActive || disabled}
                  px={5}
                  onClick={() => setRemoving(entry)}
                  aria-label={`Remove ${entry.id}`}
                >
                  <IconTrash size={16} />
                </Button>
              </span>
            </Tooltip>
          </Group>
        );
      },
    }),
  ];

  const table = useReactTable({
    data: snapshot?.entries ?? [],
    columns,
    state: { sorting, columnFilters, globalFilter: search },
    onSortingChange: setSorting,
    onColumnFiltersChange: setColumnFilters,
    onGlobalFilterChange: setSearch,
    globalFilterFn: (row, _columnId, value: string) => {
      const entry = row.original;
      const haystack = `${entry.title ?? ""} ${entry.author ?? ""} ${entry.desc ?? ""} ${entry.id}`;
      return haystack.toLowerCase().includes(value.toLowerCase());
    },
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
  });

  useEffect(() => {
    setColumnFilters(factionFilter ? [{ id: "faction", value: factionFilter }] : []);
  }, [factionFilter]);

  const healthRepairable = snapshot?.health.issues.some((issue) => issue.repairable) ?? false;
  const errorRepairable = operationError ? REPAIRABLE_ERROR_CODES.has(operationError.code) : false;

  return (
    <Stack p="lg" gap="lg" h="calc(100vh - 50px)">
      <Group justify="space-between">
        <Title order={2}>Library</Title>
        <ImportWizard
          knownIds={new Set(snapshot?.entries.map((entry) => entry.id) ?? [])}
          activePackageId={active?.id ?? null}
          disabled={mutationsBlocked}
          onImported={refresh}
          pendingZip={pendingZip}
          onZipConsumed={onZipConsumed}
        />
      </Group>

      <Card withBorder>
        <Group justify="space-between" align="center">
          <Stack gap={3}>
            <Text size="xs" tt="uppercase" c="dimmed" fw={700}>
              Active campaign
            </Text>
            {active ? (
              <Group gap="sm">
                <Text fw={600}>{activeEntry?.title ?? active.id}</Text>
                <Badge variant="light" color={FACTION_COLORS[active.faction] ?? "gray"}>
                  {FACTION_NAMES[active.faction] ?? active.faction}
                </Badge>
                <Text size="xs" c="dimmed">
                  Revision {active.revision.slice(0, 12)}
                </Text>
              </Group>
            ) : (
              <Stack gap={0}>
                <Text fw={600}>Vanilla</Text>
                <Text size="xs" c="dimmed">
                  No custom campaign is active.
                </Text>
              </Stack>
            )}
          </Stack>
          {active && (
            <Group gap="xs">
              <Button
                leftSection={<IconPlayerPlay size={16} />}
                loading={busyId === active.id}
                disabled={pageBusy !== null || mutationsBlocked}
                onClick={() => play({ id: active.id, title: activeEntry?.title ?? null })}
              >
                Play
              </Button>
              <Button
                variant="default"
                leftSection={<IconRestore size={16} />}
                loading={pageBusy === "restore"}
                disabled={busyId !== null || mutationsBlocked}
                onClick={returnToVanilla}
              >
                Return to vanilla
              </Button>
            </Group>
          )}
        </Group>
      </Card>

      <MigrationBanner disabled={mutationsBlocked} onMigrated={refresh} />

      {snapshot && snapshot.health.state !== "ready" && (
        <Alert
          color={snapshot.health.state === "recovery_required" ? "red" : "yellow"}
          title={
            snapshot.health.state === "recovery_required"
              ? "Recovery required"
              : "Library needs attention"
          }
        >
          <Stack gap="xs">
            {snapshot.health.issues.map((issue) => (
              <Text size="sm" key={`${issue.code}:${issue.path ?? ""}`}>
                {issue.message}
              </Text>
            ))}
            {healthRepairable && (
              <Button
                size="xs"
                variant="light"
                color="yellow"
                w="fit-content"
                loading={pageBusy === "repair"}
                onClick={repair}
              >
                Repair active campaign
              </Button>
            )}
          </Stack>
        </Alert>
      )}

      {loadError && (
        <Alert color="red" title="Library could not be loaded">
          {loadError.message}
        </Alert>
      )}

      {operationError && (
        <Alert
          color="red"
          title="Operation failed"
          withCloseButton
          onClose={() => setOperationError(null)}
        >
          <Stack gap="xs">
            <Text size="sm">{operationError.message}</Text>
            {errorRepairable && (
              <Button
                size="xs"
                variant="light"
                color="red"
                w="fit-content"
                loading={pageBusy === "repair"}
                onClick={repair}
              >
                Repair active campaign
              </Button>
            )}
          </Stack>
        </Alert>
      )}

      <Group align="flex-end">
        <TextInput
          placeholder="Search title, author, description…"
          leftSection={<Text size="sm">⌕</Text>}
          value={search}
          onChange={(event) => setSearch(event.currentTarget.value)}
          w={320}
        />
        <Select
          placeholder="Faction"
          clearable
          data={SLOTS}
          value={factionFilter}
          onChange={setFactionFilter}
          w={140}
        />
      </Group>

      {snapshot === null && !loadError && (
        <Center>
          <Loader size="sm" />
        </Center>
      )}

      {snapshot !== null && table.getRowModel().rows.length === 0 && (
        <Text c="dimmed">No packages match. Drop a campaign zip to import it.</Text>
      )}

      {table.getRowModel().rows.length > 0 && (
        <Card withBorder p={0} flex={1} style={{ overflowY: "auto", minHeight: 0 }}>
          <Table highlightOnHover verticalSpacing="sm">
            <Table.Thead
              style={{
                position: "sticky",
                top: 0,
                zIndex: 1,
                background: "var(--mantine-color-body)",
              }}
            >
              {table.getHeaderGroups().map((headerGroup) => (
                <Table.Tr key={headerGroup.id}>
                  {headerGroup.headers.map((header) => {
                    const sorted = header.column.getIsSorted();
                    const canSort = header.column.getCanSort();
                    const ariaSort =
                      sorted === "asc" ? "ascending" : sorted === "desc" ? "descending" : "none";
                    return (
                      <Table.Th key={header.id} aria-sort={canSort ? ariaSort : undefined}>
                        {canSort ? (
                          <UnstyledButton
                            onClick={header.column.getToggleSortingHandler()}
                            w="100%"
                            py={4}
                            style={{ textAlign: "inherit" }}
                          >
                            {flexRender(header.column.columnDef.header, header.getContext())}
                            {sorted === "asc" ? " ↑" : sorted === "desc" ? " ↓" : ""}
                          </UnstyledButton>
                        ) : (
                          flexRender(header.column.columnDef.header, header.getContext())
                        )}
                      </Table.Th>
                    );
                  })}
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
        opened={externalConflict !== null}
        onClose={() => setExternalConflict(null)}
        title="Replace external Mods file?"
        size="md"
      >
        <Stack gap="sm">
          <Text size="sm">{externalConflict?.error.message}</Text>
          <Alert color="yellow" title="This replacement is permanent">
            StarVault keeps a recovery copy while this operation is running, so a failed activation
            can roll back. After activation commits, Return to vanilla removes the campaign file but
            cannot restore the external file it replaced.
          </Alert>
          <Checkbox
            label="Don't ask again; replace future external Mods conflicts automatically"
            checked={rememberExternalMods}
            onChange={(event) => setRememberExternalMods(event.currentTarget.checked)}
          />
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setExternalConflict(null)}>
              Cancel
            </Button>
            <Button
              color="yellow"
              loading={externalConflict !== null && busyId === externalConflict.entry.id}
              onClick={retryWithExternalReplacement}
            >
              Replace and {externalConflict?.action === "play" ? "play" : "activate"}
            </Button>
          </Group>
        </Stack>
      </Modal>

      <Modal
        opened={removing !== null}
        onClose={() => setRemoving(null)}
        title="Remove package?"
        size="sm"
      >
        <Stack gap="sm">
          <Text size="sm">
            Delete {removing?.id} and its files from the store. An active package can only be
            removed after you return to vanilla.
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

      <Modal opened={editing !== null} onClose={() => setEditing(null)} title="Edit metadata">
        <Stack gap="sm">
          <TextInput
            label="Title"
            value={editTitle}
            onChange={(event) => setEditTitle(event.currentTarget.value)}
          />
          <TextInput
            label="Author"
            value={editAuthor}
            onChange={(event) => setEditAuthor(event.currentTarget.value)}
          />
          <TextInput
            label="Version"
            value={editVersion}
            onChange={(event) => setEditVersion(event.currentTarget.value)}
          />
          <Textarea
            label="Description"
            value={editDesc}
            onChange={(event) => setEditDesc(event.currentTarget.value)}
            autosize
            maxRows={6}
          />
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setEditing(null)}>
              Cancel
            </Button>
            <Button loading={saving} onClick={saveEdit}>
              Save
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
