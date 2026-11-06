"use client";

import { useState } from "react";

const LABELS = { idle: "Copy", copied: "Copied", failed: "Select it" } as const;

export function CopyButton({ text, label }: { text: string; label: string }) {
  const [state, setState] = useState<keyof typeof LABELS>("idle");

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setState("copied");
    } catch {
      // The browser refused. The text is on screen, so say to select it by hand.
      setState("failed");
    }
    setTimeout(() => setState("idle"), 2000);
  }

  return (
    <button type="button" className="copy" onClick={copy} aria-label={label}>
      <span aria-live="polite">{LABELS[state]}</span>
    </button>
  );
}
