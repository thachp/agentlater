// Calls the private Rust API over its service binding. Every call carries a short-lived
// assertion signed with the gateway's Ed25519 key; the API trusts nothing else.
// Format and claims: packages/contracts/openapi.yaml (securitySchemes.assertion).

export type Scope = "project" | "schedule" | "read" | "cancel" | "billing";

const AUDIENCE = "runlater-api";
const LIFETIME_SECS = 60;
// PKCS#8 header for a raw 32-byte Ed25519 seed (RFC 8410).
const PKCS8_ED25519_PREFIX = "302e020100300506032b657004220420";

const b64url = (bytes: Uint8Array) =>
  btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

function hexBytes(hex: string): Uint8Array {
  const clean = hex.trim();
  if (!/^([0-9a-f]{2})+$/i.test(clean)) throw new Error("expected hex");
  return Uint8Array.from(clean.match(/../g)!.map((b) => parseInt(b, 16)));
}

/** Imports the gateway's signing key from its hex-encoded 32-byte seed. */
export async function importSigningKey(seedHex: string): Promise<CryptoKey> {
  const seed = hexBytes(seedHex);
  if (seed.length !== 32) throw new Error("ASSERTION_PRIVATE_KEY must be a 32-byte hex seed");
  return crypto.subtle.importKey(
    "pkcs8",
    hexBytes(PKCS8_ED25519_PREFIX + seedHex.trim()),
    { name: "Ed25519" },
    false,
    ["sign"],
  );
}

export async function signAssertion(
  key: CryptoKey,
  projectId: string,
  scopes: Scope[],
  nowSecs = Math.floor(Date.now() / 1000),
): Promise<string> {
  const claims = {
    aud: AUDIENCE,
    sub: projectId,
    scope: scopes.join(" "),
    iat: nowSecs,
    exp: nowSecs + LIFETIME_SECS,
  };
  const payload = b64url(new TextEncoder().encode(JSON.stringify(claims)));
  const sig = await crypto.subtle.sign("Ed25519", key, new TextEncoder().encode(payload));
  return `${payload}.${b64url(new Uint8Array(sig))}`;
}

/** An API error passed through to the agent unchanged: code, message and field. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly body: { error: { code: string; message: string; field?: string } },
  ) {
    super(body.error.message);
  }
}

export class ApiClient {
  constructor(
    private readonly api: Fetcher,
    private readonly key: CryptoKey,
    private readonly projectId: string,
  ) {}

  async call<T>(method: "GET" | "POST", path: string, scope: Scope, body?: unknown): Promise<T> {
    const res = await this.api.fetch(`https://api.internal${path}`, {
      method,
      headers: {
        "Content-Type": "application/json",
        "RunLater-Assertion": await signAssertion(this.key, this.projectId, [scope]),
      },
      body: body === undefined ? null : JSON.stringify(body),
    });
    const json = (await res.json().catch(() => null)) as T | ApiError["body"] | null;
    if (!res.ok) {
      const err = json as ApiError["body"] | null;
      throw new ApiError(
        res.status,
        err?.error?.code ? err : { error: { code: "api_unavailable", message: `API returned ${res.status}` } },
      );
    }
    return json as T;
  }
}
