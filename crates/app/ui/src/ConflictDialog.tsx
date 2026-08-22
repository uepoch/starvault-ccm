import { Button, Group, Modal, Stack, Text } from "@mantine/core";
import { invoke } from "@tauri-apps/api/core";
import { notifications } from "@mantine/notifications";
import { errMessage, type ConflictInfo } from "./errors";

const FACTION_TITLES: Record<string, string> = {
  wol: "WoL",
  hots: "HotS",
  lotv: "LotV",
  nco: "NCO",
};

export interface ConflictDialogState {
  info: ConflictInfo;
  retrySlot: string;
  retryId: string;
}

/** The M5 conflict dialog: names both packages and the first clashing
 * Mods\ path; "Disable conflict" restores the other faction and retries. */
export default function ConflictDialog({
  state,
  onClose,
  onDone,
}: {
  state: ConflictDialogState | null;
  onClose: () => void;
  onDone: () => void;
}) {
  const busy = false;

  return (
    <Modal opened={state !== null} onClose={onClose} title="Dependency conflict" size="md">
      <Stack gap="sm">
        <Text size="sm">
          Activating <b>{state?.retryId}</b> would clash with <b>{state?.info.other_id}</b>{" "}
          (currently active on{" "}
          {FACTION_TITLES[state?.info.other_slot ?? ""] ?? state?.info.other_slot}
          ): both ship different content for{" "}
          <Text span ff="monospace" size="sm">
            {state?.info.target}
          </Text>
          {state && state.info.conflict_count > 1 && (
            <Text size="sm" c="dimmed" mt={4}>
              …and {state.info.conflict_count - 1} more file(s).
            </Text>
          )}
        </Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>
            Keep current
          </Button>
          <Button
            color="orange"
            disabled={busy}
            onClick={async () => {
              if (!state) return;
              const { info, retrySlot, retryId } = state;
              try {
                await invoke("restore_campaign", { slot: info.other_slot });
                notifications.show({
                  color: "green",
                  message: `${FACTION_TITLES[info.other_slot] ?? info.other_slot} restored to plain.`,
                });
                await invoke("activate_campaign", { slot: retrySlot, id: retryId });
                notifications.show({
                  color: "green",
                  message: `${retryId} activated on ${FACTION_TITLES[retrySlot] ?? retrySlot}.`,
                });
                onClose();
                onDone();
              } catch (e) {
                notifications.show({ color: "red", title: "Failed", message: errMessage(e) });
              }
            }}
          >
            Disable conflict &amp; activate
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}
