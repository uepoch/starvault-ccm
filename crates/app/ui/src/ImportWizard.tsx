import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Button,
  Collapse,
  Group,
  Modal,
  Progress,
  Stack,
  Stepper,
  Text,
  Textarea,
  TextInput,
} from "@mantine/core";
import { toCommandError } from "./errors";
import { FACTION_COLORS, SLOTS } from "./factions";
import { importReducer, initialImportState } from "./importState";
import { activatePackage, importApi, type ImportProgressEvent } from "./ipc";
import type { CommandError } from "./types";

interface ImportWizardProps {
  knownIds: Set<string>;
  activePackageId: string | null;
  disabled?: boolean;
  onImported: () => void | Promise<void>;
  pendingZip: string | null;
  onZipConsumed: () => void;
}

export default function ImportWizard({
  knownIds,
  activePackageId,
  disabled = false,
  onImported,
  pendingZip,
  onZipConsumed,
}: ImportWizardProps) {
  const [opened, setOpened] = useState(false);
  const [workflow, dispatch] = useReducer(importReducer, initialImportState);
  const [id, setId] = useState("");
  const [title, setTitle] = useState("");
  const [author, setAuthor] = useState("");
  const [version, setVersion] = useState("");
  const [desc, setDesc] = useState("");
  const [faction, setFaction] = useState("");
  const [warningsOpen, setWarningsOpen] = useState(false);
  const [importedId, setImportedId] = useState<string | null>(null);
  const [archivePath, setArchivePath] = useState<string | null>(null);
  const [activating, setActivating] = useState(false);
  const [actionError, setActionError] = useState<CommandError | null>(null);
  const [cleanupError, setCleanupError] = useState<CommandError | null>(null);
  const [closing, setClosing] = useState(false);
  const workflowRef = useRef(workflow);
  const disabledRef = useRef(disabled);
  const generationRef = useRef(0);
  const closingRef = useRef(false);
  workflowRef.current = workflow;
  disabledRef.current = disabled;

  const resetLocalState = useCallback(() => {
    dispatch({ type: "reset" });
    setId("");
    setTitle("");
    setAuthor("");
    setVersion("");
    setDesc("");
    setFaction("");
    setWarningsOpen(false);
    setImportedId(null);
    setArchivePath(null);
    setActionError(null);
    setCleanupError(null);
  }, []);

  const closeWizard = useCallback(async () => {
    if (closingRef.current) return;
    closingRef.current = true;
    setClosing(true);
    setCleanupError(null);
    generationRef.current += 1;
    const current = workflowRef.current;
    try {
      if (current.opId) {
        await importApi.cancel(current.opId);
      }
      setOpened(false);
      resetLocalState();
    } catch (error) {
      setCleanupError(toCommandError(error));
    } finally {
      closingRef.current = false;
      setClosing(false);
    }
  }, [resetLocalState]);

  const startAnalyze = useCallback(async (droppedPath?: string) => {
    if (disabledRef.current) return;
    const generation = generationRef.current + 1;
    generationRef.current = generation;
    let selected = droppedPath ?? null;
    if (!selected) {
      selected = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "Package", extensions: ["zip"] }],
      });
    }
    if (
      generationRef.current !== generation ||
      disabledRef.current ||
      !selected ||
      Array.isArray(selected)
    )
      return;

    const previous = workflowRef.current;
    if (previous.opId) {
      try {
        await importApi.cancel(previous.opId);
      } catch (error) {
        if (generationRef.current === generation) {
          setCleanupError(toCommandError(error));
        }
        return;
      }
    }
    if (generationRef.current !== generation) return;

    const opId = crypto.randomUUID();
    setArchivePath(selected);
    dispatch({ type: "analyze", opId });
    setActionError(null);
    setCleanupError(null);
    try {
      const operation = await importApi.analyze(opId, selected);
      if (generationRef.current !== generation) return;
      if (operation.state !== "Ready" || !operation.preview) {
        throw new Error("Import analysis did not return a preview.");
      }
      const preview = operation.preview;
      setId(preview.suggested_id);
      setTitle(preview.title ?? "");
      setAuthor(preview.author ?? "");
      setVersion(preview.version ?? "");
      setDesc(preview.desc ?? "");
      setFaction(preview.slot_guess === "unknown" ? "" : preview.slot_guess);
      dispatch({ type: "ready", preview });
    } catch (error) {
      if (generationRef.current !== generation) return;
      dispatch({ type: "failed", error });
    }
  }, []);

  useEffect(() => {
    if (!pendingZip) return;
    onZipConsumed();
    if (disabled) {
      notifications.show({
        color: "red",
        message: "Wait until the Library is ready before importing a package.",
      });
      return;
    }
    setOpened(true);
    void startAnalyze(pendingZip);
  }, [disabled, onZipConsumed, pendingZip, startAnalyze]);

  useEffect(() => {
    const unlisten = listen<ImportProgressEvent>("import-progress", (event) => {
      if (event.payload.op_id !== workflowRef.current.opId) return;
      const { files_done: done, files_total: total } = event.payload;
      dispatch({ type: "progress", value: total > 0 ? (done / total) * 100 : 0 });
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  useEffect(
    () => () => {
      generationRef.current += 1;
      const current = workflowRef.current;
      if (current.opId) {
        void importApi.cancel(current.opId).catch(() => undefined);
      }
    },
    [],
  );

  const startIngest = async () => {
    if (disabled || !workflow.opId || !id || !faction || activePackageId === id) return;
    const generation = generationRef.current;
    dispatch({ type: "ingest" });
    setActionError(null);
    try {
      const operation = await importApi.ingest({
        opId: workflow.opId,
        id,
        faction,
        title: title || null,
        author: author || null,
        version: version || null,
        desc: desc || null,
      });
      if (generationRef.current !== generation) return;
      if (operation.state === "Cancelled") {
        dispatch({ type: "cancelled" });
        notifications.show({ color: "yellow", message: "Import cancelled." });
        return;
      }
      if (operation.state !== "Completed" || !operation.revision) {
        throw new Error("Import did not return a completed revision.");
      }
      setImportedId(id);
      dispatch({ type: "completed", revision: operation.revision });
      notifications.show({
        color: "green",
        title: "Imported",
        message: `${id}@${operation.revision.slice(0, 12)}`,
      });
      await onImported();
    } catch (error) {
      if (generationRef.current !== generation) return;
      dispatch({ type: "failed", error });
    }
  };

  const cancelIngest = async () => {
    if (!workflow.opId) return;
    const generation = generationRef.current + 1;
    generationRef.current = generation;
    setCleanupError(null);
    try {
      await importApi.cancel(workflow.opId);
    } catch (error) {
      if (generationRef.current === generation) {
        setCleanupError(toCommandError(error));
      }
      return;
    }
    if (generationRef.current !== generation) return;
    dispatch({ type: "cancelled" });
  };

  const activateImported = async () => {
    if (disabled || !importedId) return;
    setActivating(true);
    setActionError(null);
    try {
      await activatePackage(importedId);
      await onImported();
      await closeWizard();
    } catch (error) {
      setActionError(toCommandError(error));
    } finally {
      setActivating(false);
    }
  };

  const replaceExisting = knownIds.has(id);
  const replacingActive = replaceExisting && activePackageId === id;
  const activeStep =
    workflow.state === "Idle"
      ? 0
      : workflow.state === "Analyzing"
        ? 1
        : workflow.state === "Ready" || (workflow.state === "Failed" && workflow.preview)
          ? 2
          : workflow.state === "Ingesting"
            ? 3
            : 4;

  return (
    <>
      <Button disabled={disabled} onClick={() => setOpened(true)} variant="light">
        Import package…
      </Button>

      <Modal
        opened={opened}
        onClose={() => void closeWizard()}
        title="Import package"
        closeButtonProps={{ "aria-label": "Close import wizard" }}
        fullScreen
      >
        <Stack gap="md" h="100%">
          <Stepper active={activeStep} size="sm">
            <Stepper.Step label="Select" />
            <Stepper.Step label="Analyze" />
            <Stepper.Step label="Confirm" />
            <Stepper.Step label="Ingest" />
          </Stepper>

          {workflow.error && (
            <Alert color="red" title="Import failed">
              {workflow.error.message}
            </Alert>
          )}

          {cleanupError && (
            <Alert color="red" title="Import cleanup failed">
              <Stack gap="xs">
                <Text size="sm">{cleanupError.message}</Text>
                <Button
                  size="xs"
                  variant="light"
                  color="red"
                  loading={closing}
                  w="fit-content"
                  onClick={() => void closeWizard()}
                >
                  Retry cleanup and close
                </Button>
              </Stack>
            </Alert>
          )}

          {workflow.state === "Idle" && (
            <Stack gap="sm" justify="center" flex={1}>
              <Button disabled={disabled} onClick={() => void startAnalyze()}>
                Choose campaign zip…
              </Button>
            </Stack>
          )}

          {workflow.state === "Analyzing" && (
            <Stack gap="xs" justify="center" flex={1}>
              <Text size="sm">Analyzing package…</Text>
              <Progress value={workflow.progress} animated />
            </Stack>
          )}

          {(workflow.state === "Ready" || workflow.state === "Failed") && workflow.preview && (
            <Stack gap="sm">
              {replacingActive ? (
                <Alert color="red" title="Return to vanilla first">
                  This package is active. Close the wizard, return to vanilla in Library, then
                  import it again to replace the installed revision.
                </Alert>
              ) : (
                replaceExisting && (
                  <Alert color="yellow" title="Already installed">
                    A package named {id} is already installed. Confirming replaces its current
                    revision entirely.
                  </Alert>
                )
              )}
              {workflow.preview.warnings.length > 0 && (
                <Alert color="yellow" title={`Warnings (${workflow.preview.warnings.length})`}>
                  <Collapse expanded={warningsOpen}>
                    <Stack gap="xs">
                      {workflow.preview.warnings.map((warning) => (
                        <Text key={warning} size="sm">
                          {warning}
                        </Text>
                      ))}
                    </Stack>
                  </Collapse>
                  <Button
                    variant="subtle"
                    size="compact-xs"
                    mt="xs"
                    onClick={() => setWarningsOpen((value) => !value)}
                  >
                    {warningsOpen ? "Hide" : "Show all"}
                  </Button>
                </Alert>
              )}
              <TextInput
                label="Package id"
                value={id}
                onChange={(event) => setId(event.currentTarget.value)}
              />
              <TextInput
                label="Title"
                value={title}
                onChange={(event) => setTitle(event.currentTarget.value)}
              />
              <TextInput
                label="Author"
                value={author}
                onChange={(event) => setAuthor(event.currentTarget.value)}
              />
              <TextInput
                label="Version"
                value={version}
                onChange={(event) => setVersion(event.currentTarget.value)}
              />
              <Textarea
                label="Description"
                value={desc}
                onChange={(event) => setDesc(event.currentTarget.value)}
                autosize
                minRows={2}
                maxRows={4}
              />
              <div>
                <Text size="sm" fw={500} mb={4}>
                  Faction
                  {workflow.preview.slot_guess === "unknown" &&
                    " (nothing detected, choose the faction this campaign was built for)"}
                </Text>
                <Group gap="xs">
                  {SLOTS.map((option) => (
                    <Button
                      key={option.value}
                      size="xs"
                      variant={faction === option.value ? "filled" : "default"}
                      color={FACTION_COLORS[option.value]}
                      onClick={() => setFaction(option.value)}
                    >
                      {option.label}
                    </Button>
                  ))}
                </Group>
              </div>
              <Group>
                <Button
                  disabled={
                    disabled || (workflow.state === "Ready" && (!id || !faction || replacingActive))
                  }
                  onClick={
                    workflow.state === "Failed"
                      ? () => void startAnalyze(archivePath ?? undefined)
                      : startIngest
                  }
                >
                  {workflow.state === "Failed"
                    ? "Analyze again"
                    : `Ingest (${workflow.preview.file_count} files)`}
                </Button>
                <Button variant="default" loading={closing} onClick={() => void closeWizard()}>
                  Cancel
                </Button>
              </Group>
            </Stack>
          )}

          {workflow.state === "Failed" && !workflow.preview && (
            <Stack gap="sm" justify="center" flex={1}>
              <Button disabled={disabled} onClick={() => void startAnalyze()}>
                Choose another campaign zip…
              </Button>
              <Button variant="default" loading={closing} onClick={() => void closeWizard()}>
                Close
              </Button>
            </Stack>
          )}

          {workflow.state === "Ingesting" && (
            <Stack gap="xs" justify="center" flex={1}>
              <Text size="sm">Ingesting package…</Text>
              <Progress value={workflow.progress} animated />
              <Button
                variant="light"
                color="red"
                size="xs"
                disabled={closing}
                onClick={cancelIngest}
                w="fit-content"
              >
                Cancel
              </Button>
            </Stack>
          )}

          {(workflow.state === "Completed" || workflow.state === "Cancelled") && (
            <Stack gap="sm" justify="center" flex={1} maw={480} mx="auto" w="100%">
              <Alert color={workflow.state === "Cancelled" ? "yellow" : "green"}>
                {workflow.state === "Cancelled"
                  ? "Import cancelled."
                  : `${importedId} imported at revision ${workflow.revision?.slice(0, 12)}.`}
              </Alert>
              {actionError && (
                <Alert color="red" title="Activation failed">
                  <Text size="sm">{actionError.message}</Text>
                </Alert>
              )}
              {importedId && (
                <Text size="sm" fw={500}>
                  Activate this campaign now?
                </Text>
              )}
              <Group justify="space-between" w="100%">
                <Button
                  variant="default"
                  onClick={() => void closeWizard()}
                  loading={closing}
                  w={importedId ? "48%" : "100%"}
                >
                  {importedId ? "Later" : "Done"}
                </Button>
                {importedId && (
                  <Button
                    w="48%"
                    loading={activating}
                    disabled={disabled}
                    onClick={activateImported}
                  >
                    Activate
                  </Button>
                )}
              </Group>
            </Stack>
          )}
        </Stack>
      </Modal>
    </>
  );
}
