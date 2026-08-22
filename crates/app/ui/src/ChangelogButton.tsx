import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button, Modal } from "@mantine/core";
import Markdown from "react-markdown";

/// Changelog button for the app header; the modal renders the embedded
/// CHANGELOG.md as markdown.
export default function ChangelogButton() {
  const [changelog, setChangelog] = useState<string | null>(null);
  const [open, setOpen] = useState(false);

  return (
    <>
      <Button
        variant="subtle"
        size="compact-sm"
        onClick={async () => {
          setChangelog(await invoke<string>("changelog"));
          setOpen(true);
        }}
      >
        Changelog
      </Button>

      <Modal opened={open} onClose={() => setOpen(false)} title="" size="lg">
        <Markdown>{changelog ?? ""}</Markdown>
      </Modal>
    </>
  );
}
