"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

const PAGES = [
  ["/docs", "Quickstart"],
  ["/docs/capabilities", "Capabilities"],
  ["/docs/delivery", "Delivery & signatures"],
  ["/docs/a2a", "A2A"],
] as const;

export function DocsNav() {
  const path = usePathname();
  return (
    <nav className="docs-nav" aria-label="Docs">
      <p>Docs</p>
      <ul>
        {PAGES.map(([href, label]) => (
          <li key={href}>
            <Link href={href} aria-current={path === href ? "page" : undefined}>
              {label}
            </Link>
          </li>
        ))}
      </ul>
    </nav>
  );
}
