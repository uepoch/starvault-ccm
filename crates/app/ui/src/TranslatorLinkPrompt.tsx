import { useEffect, useState } from "react";
import { Button, Group, Loader, Modal, Stack, Text } from "@mantine/core";
import { toCommandError } from "./errors";
import { resolveTranslatorLink } from "./ipc";
import type { CommandError, ImportSource, TranslatorLinkTarget } from "./types";

interface TranslatorLinkPromptProps {
  instanceId: string | null;
  disabled: boolean;
  onDismiss: () => void;
  onDownload: (source: ImportSource) => void;
  onActivate: (entry: { id: string; title: string | null }) => void | Promise<void>;
}

const formatMegabytes = (size: number) =>
  `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(size / 1_000_000)} MB`;

export default function TranslatorLinkPrompt({
  instanceId,
  disabled,
  onDismiss,
  onDownload,
  onActivate,
}: TranslatorLinkPromptProps) {
  const [target, setTarget] = useState<TranslatorLinkTarget | null>(null);
  const [error, setError] = useState<CommandError | null>(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let current = true;
    setTarget(null);
    setError(null);
    if (instanceId) {
      void resolveTranslatorLink(instanceId)
        .then((resolved) => {
          if (current) setTarget(resolved);
        })
        .catch((reason: unknown) => {
          if (current) setError(toCommandError(reason));
        });
    }
    return () => {
      current = false;
    };
  }, [attempt, instanceId]);

  let body = (
    <Group gap="sm">
      <Loader size="sm" />
      <Text size="sm">Checking translation…</Text>
    </Group>
  );

  if (error) {
    body = (
      <Stack gap="sm">
        <Text size="sm">{error.message}</Text>
        <Group justify="flex-end">
          {error.retryable ? (
            <>
              <Button variant="default" onClick={onDismiss}>
                No
              </Button>
              <Button onClick={() => setAttempt((value) => value + 1)}>Try again</Button>
            </>
          ) : (
            <Button onClick={onDismiss}>OK</Button>
          )}
        </Group>
      </Stack>
    );
  } else if (target?.kind === "download" && instanceId) {
    body = (
      <Stack gap="sm">
        <Text size="sm">
          You're going to download {target.filename} ({formatMegabytes(target.size)}). Do you want
          to continue?
        </Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onDismiss}>
            No
          </Button>
          <Button
            disabled={disabled}
            onClick={() => {
              onDismiss();
              onDownload({
                kind: "translator",
                instanceId,
                expectedSize: target.size,
              });
            }}
          >
            Yes
          </Button>
        </Group>
      </Stack>
    );
  } else if (target?.kind === "installed") {
    const name = target.title ?? target.package_id;
    body = target.active ? (
      <Stack gap="sm">
        <Text size="sm">{name} is already installed and active.</Text>
        <Group justify="flex-end">
          <Button onClick={onDismiss}>OK</Button>
        </Group>
      </Stack>
    ) : (
      <Stack gap="sm">
        <Text size="sm">{name} is already installed. Do you want to activate it?</Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onDismiss}>
            No
          </Button>
          <Button
            disabled={disabled}
            onClick={() => {
              onDismiss();
              void onActivate({ id: target.package_id, title: target.title });
            }}
          >
            Yes
          </Button>
        </Group>
      </Stack>
    );
  }

  return (
    <Modal
      opened={instanceId !== null}
      onClose={onDismiss}
      title="Install translated campaign"
      size="md"
    >
      {body}
    </Modal>
  );
}
