// The OAuth consent page hosted MCP clients open once. There is no account: approving creates a
// new owner, and its grant is that owner's identity from then on, as a key pair is for an
// Agent Auth agent. Each approval is a new project with its own trial.

import { AuthorizationError, type OAuthHelpers } from "@cloudflare/workers-oauth-provider";
import type { McpProps } from "./mcp";

export interface ConsentEnv {
  OAUTH_PROVIDER: OAuthHelpers;
  /** Per-IP limit on the OAuth endpoints, separate from Agent Auth registration's. */
  OAUTH_RATE: RateLimit;
}

const escape = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string);

/** The site's icon (apps/web/app/icon.svg), inline because the CSP allows no images. */
const ICON = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40" aria-hidden="true"><rect width="40" height="40" rx="10" fill="#db4b27"/><path d="M29 13a12 12 0 1 0 3 9M20 10v11l7 4M25 8h8v8" fill="none" stroke="#fffefa" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"/></svg>`;

function page(title: string, body: string, headers: Headers = new Headers(), status = 200): Response {
  headers.set("Content-Type", "text/html; charset=utf-8");
  headers.set("Cache-Control", "no-store");
  // No form-action: browsers apply it to the redirect after the POST, and that goes to the
  // client's own redirect URI (https, loopback or an app scheme). The handle's binding cookie
  // is what stops a forged post.
  headers.set("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'");
  headers.set("X-Frame-Options", "DENY");
  // Colors copy the site's tokens (apps/web/app/globals.css); this page can't load its stylesheet.
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${escape(title)} · AgentLater</title><style>
:root{color-scheme:light dark;--bg:#fcf9f5;--card:#fffdf9;--ink:#202020;--muted:#64615d;--line:#ddd7ce;--line-strong:#bfb6aa;--accent:#ea5827;--on-accent:#21150e;--accent-soft:#fce4d4}
@media (prefers-color-scheme:dark){:root{--bg:#18191b;--card:#1b1d20;--ink:#f5f2ee;--muted:#b9b7b3;--line:#44464b;--line-strong:#5b5e64;--accent:#ff8a52;--accent-soft:#34271f}}
body{margin:0;background:var(--bg);color:var(--ink);font:16px/1.6 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased;display:grid;place-items:center;min-height:100vh;padding:16px;box-sizing:border-box}
.shell{max-width:440px;width:100%}
.brand{display:flex;align-items:center;gap:10px;font-weight:700;font-size:20px;letter-spacing:-0.03em;margin:0 0 16px}.brand svg{width:28px;height:28px;flex:none}
main{background:var(--card);border:1px solid var(--line);border-radius:16px;padding:28px;box-sizing:border-box}
h1{font-size:22px;line-height:1.15;font-weight:650;letter-spacing:-0.02em;margin:0 0 14px;overflow-wrap:anywhere}p{margin:0 0 12px}.muted{color:var(--muted);font-size:14px}
code{font:0.9em ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--accent-soft);padding:0.1em 0.35em;border-radius:5px;overflow-wrap:anywhere}
.warn{border-left:3px solid var(--accent);padding:6px 12px;font-size:14px}
.actions{display:flex;flex-wrap:wrap;gap:10px;margin-top:20px}button{font:inherit;font-weight:650;border-radius:9px;padding:10px 18px;cursor:pointer;border:1px solid var(--line-strong);background:transparent;color:var(--ink)}
button.primary{background:var(--accent);border-color:var(--accent);color:var(--on-accent)}button:hover{filter:brightness(1.08)}button:focus-visible{outline:3px solid var(--accent);outline-offset:3px}
</style></head><body><div class="shell"><p class="brand">${ICON}AgentLater</p><main>${body}</main></div></body></html>`;
  return new Response(html, { status, headers });
}

function failure(message: string, status = 400): Response {
  return page("Can't connect", `<h1>Can't connect</h1><p>${escape(message)}</p>`, undefined, status);
}

/** GET shows the consent page; POST approves or declines it. */
export async function handleAuthorize(request: Request, env: ConsentEnv): Promise<Response> {
  const oauth = env.OAUTH_PROVIDER;
  try {
    if (request.method === "GET") {
      let authRequest;
      try {
        authRequest = await oauth.parseAuthRequest(request);
      } catch (e) {
        // The client sent a bad authorization request; its reason helps whoever set it up.
        return failure(`This sign-in request isn't valid${e instanceof Error ? `: ${e.message}` : "."}`);
      }
      const about = await oauth.describeConsent(authRequest);
      const { handle, headers } = await oauth.beginConsent(authRequest);
      const name = about.clientDomain ? `${about.clientName} (${about.clientDomain})` : about.clientName;
      const local = about.redirectIsLoopback
        ? `<p class="warn">This app is on your computer (<code>${escape(about.redirectHost)}</code>). Any local program could be listening there, whatever it calls itself.</p>`
        : "";
      return page(
        "Connect",
        `<h1>Connect ${escape(name)} to AgentLater?</h1>
<p>It will be able to schedule HTTP requests, crons and watches, and read their results. They run from a new AgentLater project with a free trial: 1,000 delivery attempts over 7 days.</p>
<p class="muted">There's no account. This connection is the project's only key: if it is removed, so is access to the project.</p>
<p class="muted">You'll return to <code>${escape(about.redirectHost)}</code>.</p>${local}
<form method="post"><input type="hidden" name="handle" value="${escape(handle)}">
<div class="actions"><button class="primary" name="action" value="approve">Connect</button><button name="action" value="deny">Cancel</button></div></form>`,
        headers,
      );
    }
    if (request.method !== "POST") return failure("Unsupported method.", 405);

    const form = await request.formData();
    const handle = String(form.get("handle") ?? "");
    if (form.get("action") !== "approve") {
      const { redirectTo, headers } = await oauth.denyConsent(request, handle);
      headers.set("Location", redirectTo);
      return new Response(null, { status: 302, headers });
    }
    const { request: authRequest, headers } = await oauth.approveConsent(request, handle);
    const id = [...crypto.getRandomValues(new Uint8Array(16))].map((b) => b.toString(16).padStart(2, "0")).join("");
    const props: McpProps = { owner: `mcp:${id}` };
    const { redirectTo } = await oauth.completeAuthorization({ request: authRequest, userId: id, metadata: {}, scope: [], props });
    headers.set("Location", redirectTo);
    return new Response(null, { status: 302, headers });
  } catch (e) {
    if (e instanceof AuthorizationError) return failure("This sign-in link has expired or was already used. Start again from your app.");
    console.error("consent failed", e);
    return failure("Something went wrong on AgentLater's side. Start again from your app.", 500);
  }
}
