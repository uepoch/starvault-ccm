import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Alert,
  Button,
  Group,
  Progress,
  SegmentedControl,
  Stack,
  Stepper,
  Text,
  TextInput,
} from "@mantine/core";
import { open } from "@tauri-apps/plugin-dialog";

interface ImportPreview {
  suggested_id: string;
  title: string | null;
  author: string | null;
  version: string | null;
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
  const [progress, setProgress] = useState<ProgressEvent | null>(null);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [id, setId] = useState("");
  const [slot, setSlot] = useState("unknown");
  const [result, setResult] = useState<string | null>(null);
  const opRef = useRef<string | null>(null);
  opRef.current = opId;

  useEffect(() => {
    const unlisten = listen<ProgressEvent>("import-progress", (event) => {
      if (event.payload.op_id === opRef.current) setProgress(event.payload);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  if (!opened) {
    return (
      <Button onClick={() => setOpened(true)} variant="light">
        Import package…
      </Button>
    );
  }

  const reset = () => {
    setOpened(false);
    setStep(0);
    setOpId(null);
    setProgress(null);
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
      });
      setResult(rev === null ? "cancelled" : `imported as ${id}@${rev.slice(0, 12)}`);
      setStep(4);
      onImported();
    } catch (e) {
      setError(String(e));
    }
  };

  const cancelIngest = async () => {
    if (opId) await invoke("import_cancel", { opId }).catch(() => {});
  };

  const replaceExisting = knownIds.has(id);

  return (
    <Stack gap="md">
      <Group justify="space-between">
        <Text fw={600}>Import package</Text>
        <Button variant="subtle" color="gray" onClick={reset}>
          Close
        </Button>
      </Group>

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

      {step === 0 && <Button onClick={startAnalyze}>Choose campaign zip…</Button>}

      {step === 1 && (
        <Stack gap="xs">
          <Text size="sm">Extracting…</Text>
          <Progress
            value={progress ? (progress.files_done / Math.max(progress.files_total, 1)) * 100 : 0}
            animated
          />
          {progress && (
            <Text size="xs" c="dimmed" ff="monospace">
              {progress.current_file}
            </Text>
          )}
        </Stack>
      )}

      {step === 2 && preview && (
        <Stack gap="sm">
          {replaceExisting && (
            <Alert color="yellow" title="Already installed">
              A package named `{id}` is already installed. Confirming replaces it entirely.
            </Alert>
          )}
          {preview.warnings.map((w) => (
            <Alert key={w} color="orange" title="Warning">
              {w}
            </Alert>
          ))}
          <TextInput label="Package id" value={id} onChange={(e) => setId(e.currentTarget.value)} />
          <TextInput
            label="Title"
            value={preview.title ?? ""}
            readOnly
            description={preview.author ? `by ${preview.author}` : undefined}
          />
          <div>
            <Text size="sm" fw={500} mb={4}>
              Slot
              {preview.matched_pattern
                ? ` (matched "${preview.matched_pattern}")`
                : " — no guess, choose one"}
            </Text>
            <SegmentedControl data={SLOTS} value={slot} onChange={setSlot} />
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
        <Stack gap="xs">
          <Text size="sm">Ingesting…</Text>
          <Progress
            value={progress ? (progress.files_done / Math.max(progress.files_total, 1)) * 100 : 0}
            animated
          />
          <Group justify="space-between">
            {progress && (
              <Text size="xs" c="dimmed" ff="monospace">
                {progress.current_file}
              </Text>
            )}
            <Button variant="light" color="red" size="xs" onClick={cancelIngest}>
              Cancel
            </Button>
          </Group>
        </Stack>
      )}

      {step === 4 && (
        <Stack gap="sm">
          <Alert color={result === "cancelled" ? "yellow" : "green"}>{result}</Alert>
          <Button onClick={reset}>Done</Button>
        </Stack>
      )}
    </Stack>
  );
}
