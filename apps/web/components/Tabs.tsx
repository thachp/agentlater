"use client";

import { useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

export type Tab = { label: string; detail?: string; content: ReactNode };

/**
 * Keeps long alternatives to one screen. Every panel is in the server-rendered HTML, so agents
 * and readers without JavaScript still get all of them. `rail` puts the tabs in a column beside
 * the panel.
 */
export function Tabs({ tabs, label, rail = false }: { tabs: Tab[]; label: string; rail?: boolean }) {
  const [active, setActive] = useState(0);
  const base = useId();
  const refs = useRef<(HTMLButtonElement | null)[]>([]);

  function onKeyDown(event: KeyboardEvent) {
    const last = tabs.length - 1;
    const next =
      event.key === "ArrowRight" || event.key === "ArrowDown" ? (active === last ? 0 : active + 1)
      : event.key === "ArrowLeft" || event.key === "ArrowUp" ? (active === 0 ? last : active - 1)
      : event.key === "Home" ? 0
      : event.key === "End" ? last
      : null;
    if (next === null) return;
    event.preventDefault();
    setActive(next);
    refs.current[next]?.focus();
  }

  return (
    <div className={rail ? "tabs rail" : "tabs"}>
      <noscript>
        <style>{`.tab-list { display: none; } .tab-panel[hidden] { display: block; }`}</style>
      </noscript>
      <div
        className="tab-list"
        role="tablist"
        aria-label={label}
        aria-orientation={rail ? "vertical" : undefined}
        onKeyDown={onKeyDown}
      >
        {tabs.map((tab, i) => (
          <button
            key={tab.label}
            ref={(el) => {
              refs.current[i] = el;
            }}
            type="button"
            role="tab"
            id={`${base}-tab-${i}`}
            aria-selected={i === active}
            aria-controls={`${base}-panel-${i}`}
            tabIndex={i === active ? 0 : -1}
            onClick={() => setActive(i)}
          >
            {tab.label}
            {tab.detail ? <small>{tab.detail}</small> : null}
          </button>
        ))}
      </div>
      {tabs.map((tab, i) => (
        <div
          key={tab.label}
          className="tab-panel"
          role="tabpanel"
          id={`${base}-panel-${i}`}
          aria-labelledby={`${base}-tab-${i}`}
          hidden={i !== active}
        >
          {tab.content}
        </div>
      ))}
    </div>
  );
}
