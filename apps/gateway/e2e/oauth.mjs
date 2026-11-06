// The browser half of hosted MCP's OAuth, scripted: register a client, open the consent page,
// click Connect, and exchange the code. Used by journey.mjs and agent.e2e.mjs.

import { createHash, randomBytes } from "node:crypto";

export const REDIRECT = "http://127.0.0.1:9/callback";
const b64url = (b) => Buffer.from(b).toString("base64url");

/** Returns the token response plus what the consent page showed. Throws on any step that fails. */
export async function connectHosted(origin, clientName = "AgentLater journey") {
  const step = async (what, res, status) => {
    if (res.status !== status) throw new Error(`${what} returned ${res.status}: ${(await res.text()).slice(0, 200)}`);
    return res;
  };
  const reg = await step(
    "client registration",
    await fetch(`${origin}/oauth/register`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        client_name: clientName,
        redirect_uris: [REDIRECT],
        token_endpoint_auth_method: "none",
        grant_types: ["authorization_code", "refresh_token"],
        response_types: ["code"],
      }),
    }),
    201,
  );
  const { client_id } = await reg.json();
  const verifier = b64url(randomBytes(32));
  const query = new URLSearchParams({
    response_type: "code",
    client_id,
    redirect_uri: REDIRECT,
    code_challenge: b64url(createHash("sha256").update(verifier).digest()),
    code_challenge_method: "S256",
    state: "journey",
    resource: `${origin}/mcp`,
  });
  const page = await step("the consent page", await fetch(`${origin}/authorize?${query}`), 200);
  const html = await page.text();
  const handle = html.match(/name="handle" value="([^"]+)"/)?.[1];
  if (!handle) throw new Error("the consent page has no form");
  const cookie = page.headers.getSetCookie().map((c) => c.split(";")[0]).join("; ");
  const approve = await step(
    "approving consent",
    await fetch(`${origin}/authorize`, {
      method: "POST",
      redirect: "manual",
      headers: { Cookie: cookie },
      body: new URLSearchParams({ handle, action: "approve" }),
    }),
    302,
  );
  const back = new URL(approve.headers.get("location"));
  if (back.searchParams.get("state") !== "journey") throw new Error(`consent redirected to ${back}`);
  const token = await step(
    "the token exchange",
    await fetch(`${origin}/oauth/token`, {
      method: "POST",
      body: new URLSearchParams({
        grant_type: "authorization_code",
        code: back.searchParams.get("code"),
        redirect_uri: REDIRECT,
        client_id,
        code_verifier: verifier,
        resource: `${origin}/mcp`,
      }),
    }),
    200,
  );
  return { client_id, html, pageHeaders: page.headers, redirect: back, ...(await token.json()) };
}
