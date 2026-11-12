import type { Metadata } from "next";
import { DM_Sans } from "next/font/google";
import Link from "next/link";
import type { ReactNode } from "react";
import { SiteNav } from "../components/SiteNav";
import { GATEWAY_URL, REPO_URL } from "../lib/site";
import "./globals.css";

// Downloaded at build time and served from this site, so no reader's browser calls Google.
const sans = DM_Sans({ subsets: ["latin"], variable: "--font-sans" });

export const metadata: Metadata = {
  metadataBase: new URL("https://agentlater.io"),
  title: { default: "AgentLater — HTTP requests later, for AI agents", template: "%s · AgentLater" },
  description:
    "AgentLater keeps an AI agent's work going after its session ends: signed HTTPS requests later, watches that act when a job finishes, webhooks, and an inbox of results the next session reads.",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" className={sans.variable}>
      <body>
        <a className="skip" href="#main">
          Skip to content
        </a>
        <header className="site-header dark">
          <div className="wrap">
            <Link href="/" className="brand">
              <img src="/icon.svg" alt="" />
              AgentLater
            </Link>
            <SiteNav />
          </div>
        </header>
        <main id="main">{children}</main>
        <footer className="site-footer dark">
          <div className="wrap">
            <Link href="/" className="brand">
              <img src="/icon.svg" alt="" />
              AgentLater
            </Link>
            <p>HTTP requests later. Outcomes you can check.</p>
            <nav aria-label="Footer">
              <Link href="/docs">Docs</Link>
              <Link href="/privacy">Privacy</Link>
              <Link href="/terms">Terms</Link>
              <a href={REPO_URL}>GitHub</a>
              <a href={`${GATEWAY_URL}/llms.txt`}>llms.txt</a>
            </nav>
          </div>
        </footer>
      </body>
    </html>
  );
}
