import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { notifications } from "@mantine/notifications";
import {
  Alert,
  Badge,
  Button,
  Collapse,
  Group,
  Modal,
  Progress,
  SegmentedControl,
  Stack,
  Stepper,
  Text,
  Textarea,
  TextInput,
} from "@mantine/core";
import { open } from "@tauri-apps/plugin-dialog";

interface ImportPreview {
  suggested_id: string;
  title: string | null;
  author: string | null;
  version: string | null;
  desc: string | null;
  slot_guess: string;
  matched_pattern: string | null;
  warnings: string[];
  file_count: number;
}

interface ProgressEvent {
  op_id: string;
  phase: "extract" | "ingest";
  files_done: number;
  files_total: number;
  current_file: string;
}

const SLOTS = [
  { label: "WoL", value: "wol" },
  { label: "HotS", value: "hots" },
  { label: "LotV", value: "lotv" },
  { label: "NCO", value: "nco" },
];

export default function ImportWizard({
  knownIds,
  onImported,
}: {
  knownIds: Set<string>;
  onImported: () => void;
}) {
  const [opened, setOpened] = useState(false);
  const [step, setStep] = useState(0);
  const [opId, setOpId] = useState<string | null>(null);
  const [percent, setPercent] = useState(0);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [id, setId] = useState("");
  const [title, setTitle] = useState("");
  const [desc, setDesc] = useState("");
  const [slot, setSlot] = useState("");
  const [result, setResult] = useState<string | null>(null);
  const [warningsOpen, setWarningsOpen] = useState(false);
  const [importedId, setImportedId] = useState<string | null>(null);
  const [activating, setActivating] = useState(false);
  const opRef = useRef<string | null>(null);
  opRef.current = opId;

  useEffect(() => {
    const unlisten = listen<ProgressEvent>("import-progress", (event) => {
      if (event.payload.op_id !== opRef.current) return;
      const p = event.payload;
      setPercent(p.files_total > 0 ? (p.files_done / p.files_total) * 100 : 0);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  const reset = () => {
    setOpened(false);
    setStep(0);
    setOpId(null);
    setPercent(0);
    setPreview(null);
    setError(null);
    setResult(null);
  };

  const startAnalyze = async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Package", extensions: ["zip"] }],
    });
    if (!selected) return;
    const newOpId = crypto.randomUUID();
    setOpId(newOpId);
    setStep(1);
    setError(null);
    try {
      const p = await invoke<ImportPreview>("import_analyze", {
        opId: newOpId,
        path: selected,
      });
      setPreview(p);
      setId(p.suggested_id);
      setTitle(p.title ?? "");
      setDesc(p.desc ?? "");
      setSlot(p.slot_guess === "unknown" ? "" : p.slot_guess);
      setStep(2);
    } catch (e) {
      setError(String(e));
    }
  };

  const startIngest = async () => {
    if (!opId || !id || !slot) return;
    setStep(3);
    try {
      const rev = await invoke<string | null>("import_ingest", {
        opId,
        id,
        slot,
        title: title === "" ? null : title,
        desc: desc === "" ? null : desc,
      });
      if (rev === null) {
        notifications.show({ color: "yellow", message: "Import cancelled." });
        setResult("cancelled");
      } else {
        notifications.show({
          color: "green",
          title: "Imported",
          message: `${id}@${rev.slice(0, 12)}`,
        });
        onImported();
        setImportedId(id);
        setResult(`imported as ${id}`);
      }
      setStep(4);
    } catch (e) {
      setError(String(e));
    }
  };

  const cancelIngest = async () => {
    if (opId) await invoke("import_cancel", { opId }).catch(() => {});
  };

  const replaceExisting = knownIds.has(id);

  return (
    <>
      <Button onClick={() => setOpened(true)} variant="light">
        Import package…
      </Button>

      <Modal opened={opened} onClose={reset} title="Import package" fullScreen>
        <Stack gap="md" h="100%">
          <Stepper active={step} size="sm">
            <Stepper.Step label="Select" />
            <Stepper.Step label="Analyze" />
            <Stepper.Step label="Confirm" />
            <Stepper.Step label="Ingest" />
          </Stepper>

          {error && (
            <Alert color="red" title="Error">
              {error}
            </Alert>
          )}

          {step === 0 && (
            <Stack gap="sm" justify="center" flex={1}>
              <Button onClick={startAnalyze}>Choose campaign zip…</Button>
            </Stack>
          )}

          {step === 1 && (
            <Stack gap="xs" justify="center" flex={1}>
              <Text size="sm">Extracting…</Text>
              <Progress value={percent} animated />
            </Stack>
          )}

          {step === 2 && preview && (
            <Stack gap="sm">
              {replaceExisting && (
                <Alert color="yellow" title="Already installed">
                  A package named `{id}` is already installed. Confirming replaces it entirely.
                </Alert>
              )}
              {preview.warnings.length > 0 && (
                <Alert color="yellow" title={`Warnings (${preview.warnings.length})`}>
                  <Collapse expanded={warningsOpen}>
                    <Stack gap="xs">
                      {preview.warnings.map((w) => (
                        <Text key={w} size="sm">
                          {w}
                        </Text>
                      ))}
                    </Stack>
                  </Collapse>
                  <Button
                    variant="subtle"
                    size="compact-xs"
                    mt="xs"
                    onClick={() => setWarningsOpen(!warningsOpen)}
                  >
                    {warningsOpen ? "Hide" : "Show all"}
                  </Button>
                </Alert>
              )}
              <TextInput
                label="Package id"
                value={id}
                onChange={(e) => setId(e.currentTarget.value)}
              />
              <TextInput
                label="Title"
                placeholder={preview.title ? undefined : "Nothing detected — name it yourself"}
                value={title}
                onChange={(e) => setTitle(e.currentTarget.value)}
              />
              <Textarea
                label="Description"
                placeholder="Nothing detected — describe it yourself"
                value={desc}
                onChange={(e) => setDesc(e.currentTarget.value)}
                autosize
                minRows={2}
                maxRows={4}
              />
              <div>
                <Text size="sm" fw={500} mb={4}>
                  Faction
                  {preview.matched_pattern ? ` (matched "${preview.matched_pattern}")` : ""}
                </Text>
                {preview.slot_guess !== "unknown" ? (
                  <Badge size="lg" variant="light">
                    {SLOTS.find((s) => s.value === preview.slot_guess)?.label}
                  </Badge>
                ) : (
                  <>
                    <Text size="xs" c="dimmed" mb={4}>
                      No detection possible — pick the faction this package was built for; it will
                      load via that campaign's menu.
                    </Text>
                    <SegmentedControl data={SLOTS} value={slot} onChange={setSlot} />
                  </>
                )}
              </div>
              <Group>
                <Button disabled={!id || !slot} onClick={startIngest}>
                  Ingest ({preview.file_count} files)
                </Button>
                <Button variant="default" onClick={reset}>
                  Cancel
                </Button>
              </Group>
            </Stack>
          )}

          {step === 3 && (
            <Stack gap="xs" justify="center" flex={1}>
              <Text size="sm">Ingesting…</Text>
              <Progress value={percent} animated />
              <Button variant="light" color="red" size="xs" onClick={cancelIngest} w="fit-content">
                Cancel
              </Button>
            </Stack>
          )}

          {step === 4 && (
            <Stack gap="sm" justify="center" flex={1} maw={480} mx="auto" w="100%">
              <Alert color={result === "cancelled" ? "yellow" : "green"}>{result}</Alert>
              {importedId && slot && (
                <>
                  <Text size="sm" fw={500}>
                    Activate on {SLOTS.find((s) => s.value === slot)?.label} now?
                  </Text>
                  <Group>
                    <Button
                      loading={activating}
                      onClick={async () => {
                        setActivating(true);
                        try {
                          await invoke("activate_campaign", { slot, id: importedId });
                          notifications.show({
                            color: "green",
                            message: `${importedId} activated on ${slot}.`,
                          });
                          onImported();
                          reset();
                        } catch (e) {
                          notifications.show({
                            color: "red",
                            title: "Activation blocked",
                            message: String(e),
                          });
                        } finally {
                          setActivating(false);
                        }
                      }}
                    >
                      Activate
                    </Button>
                  </Group>
                </>
              )}
              <Button variant="default" onClick={reset}>
                {importedId ? "Later" : "Done"}
              </Button>
            </Stack>
          )}
        </Stack>
      </Modal>
    </>
  );
}
