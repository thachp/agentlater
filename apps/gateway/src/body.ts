// Reading request bodies with a hard size limit, whatever Content-Length says, and the JSON
// answers of the gateway's own routes.

/** A schedule_http body can be 64 KiB of string; leave room for JSON-RPC and escaping. */
export const MAX_BODY_BYTES = 192 * 1024;

/** A JSON answer, never cached. */
export const json = (body: unknown, status = 200, headers?: Record<string, string>) => Response.json(body, { status, headers: { "Cache-Control": "no-store", ...headers } });
export const error = (status: number, code: string, message: string, headers?: Record<string, string>) => json({ error: code, message }, status, headers);
export const tooLarge = () => error(413, "too_large", "request body is too large");

/** The body, or null if it exceeds `limit` bytes. Counts bytes read, not Content-Length. */
export async function readLimited(request: Request, limit: number): Promise<Uint8Array | null> {
  if (!request.body) return new Uint8Array();
  const reader = request.body.getReader();
  const parts: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > limit) {
      await reader.cancel();
      return null;
    }
    parts.push(value);
  }
  const out = new Uint8Array(size);
  let offset = 0;
  for (const p of parts) {
    out.set(p, offset);
    offset += p.byteLength;
  }
  return out;
}
