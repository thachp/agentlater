import type { Metadata } from "next";
import Link from "next/link";
import type { ReactNode } from "react";
import { REPO_URL } from "../lib/site";
import "./globals.css";

export const metadata: Metadata = {
  metadataBase: new URL("https://runlater.io"),
  title: { default: "RunLater — HTTP requests later, for AI agents", template: "%s · RunLater" },
  description:
    "RunLater keeps an AI agent's work going after its session ends: signed HTTPS requests later, watches that act when a job finishes, webhooks, and an inbox of results the next session reads.",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body>
        <a className="skip" href="#main">
          Skip to content
        </a>
        <header className="site-header">
          <div className="wrap">
            <Link href="/" className="brand">
              <img src="/icon.svg" alt="" />
              RunLater
            </Link>
            <nav className="nav" aria-label="Main">
              <Link href="/docs">Docs</Link>
              <Link href="/docs/capabilities" className="hide-small">
                Capabilities
              </Link>
              <a href={REPO_URL}>GitHub</a>
            </nav>
          </div>
        </header>
        <main id="main">{children}</main>
        <footer className="site-footer">
          <div className="wrap">
            <span>RunLater — make an HTTP request later, and know whether it happened.</span>
            <span>
              <Link href="/docs">Docs</Link> · <Link href="/terms">Terms</Link> · <Link href="/privacy">Privacy</Link> ·{" "}
              <a href={REPO_URL}>Source</a>
            </span>
          </div>
        </footer>
      </body>
    </html>
  );
}
