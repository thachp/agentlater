"use client";

import Link from "next/link";
import { useState } from "react";
import { Icon } from "./Icon";

const LINKS = [
  ["/#how", "How it works"],
  ["/#features", "Features"],
  ["/#pricing", "Pricing"],
  ["/docs", "Docs"],
] as const;

/** The header's links. On a phone they sit behind a menu button; without JavaScript they stay in view. */
export function SiteNav() {
  const [open, setOpen] = useState(false);
  const close = () => setOpen(false);

  return (
    <>
      <noscript>
        <style>{`.menu { display: none; } .nav { display: flex; position: static; flex-wrap: wrap; padding: 0; }`}</style>
      </noscript>
      <button
        type="button"
        className="menu"
        aria-label={open ? "Close menu" : "Open menu"}
        aria-expanded={open}
        aria-controls="site-nav"
        onClick={() => setOpen(!open)}
      >
        <Icon name={open ? "close" : "menu"} size={26} />
      </button>
      <nav id="site-nav" className={open ? "nav open" : "nav"} aria-label="Main">
        {LINKS.map(([href, label]) => (
          <Link key={href} href={href} onClick={close}>
            {label}
          </Link>
        ))}
        <Link href="/#connect" className="button primary small" onClick={close}>
          Connect
        </Link>
      </nav>
    </>
  );
}
