import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { notifications } from "@mantine/notifications";
import type { ConflictDialogState } from "./ConflictDialog";
import { errConflict, errMessage } from "./errors";
import { FACTION_TITLES } from "./factions";

/** The single activation flow: invoke, notify, route conflicts to the
 * M5 dialog. Callers keep their own busy flags; the boolean says success. */
export function useActivate() {
  const [conflict, setConflict] = useState<ConflictDialogState | null>(null);

  const activate = useCallback(async (slot: string, id: string): Promise<boolean> => {
    try {
      await invoke("activate_campaign", { slot, id });
      notifications.show({
        color: "green",
        message: `${id} activated on ${FACTION_TITLES[slot] ?? slot}.`,
      });
      return true;
    } catch (e) {
      const info = errConflict(e);
      if (info) {
        setConflict({ info, retrySlot: slot, retryId: id });
      } else {
        notifications.show({ color: "red", title: "Activation failed", message: errMessage(e) });
      }
      return false;
    }
  }, []);

  return { activate, conflict, setConflict };
}
