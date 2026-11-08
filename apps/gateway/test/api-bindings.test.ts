import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { capabilities } from "../src/capabilities";

/** `METHOD /path/{}` → x-scope, for every operation in the API's contract. */
function operations(): Map<string, string | undefined> {
  const doc = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../../../packages/contracts/openapi.yaml"), "utf8");
  const ops = new Map<string, string | undefined>();
  let path = "";
  let key = "";
  for (const line of doc.split("\n")) {
    const p = /^  (\/\S*):$/.exec(line);
    const m = /^    (get|post):$/.exec(line);
    const s = /^      x-scope: (\S+)$/.exec(line);
    if (p) path = p[1]!.replace(/\{\w+\}/g, "{}");
    else if (m) ops.set((key = `${m[1]!.toUpperCase()} ${path}`), undefined);
    else if (s) ops.set(key, s[1]);
  }
  return ops;
}

describe("capability bindings", () => {
  const ops = operations();

  it("each call an operation in the API's contract, with the scope it requires", () => {
    for (const c of capabilities) {
      if (!c.api) continue;
      const key = `${c.api.method} ${c.api.path.replace(/\{\w+\}/g, "{}")}`;
      expect(ops.has(key), `${c.name}: ${key} is not in openapi.yaml`).toBe(true);
      expect(ops.get(key), c.name).toBe(c.api.scope);
    }
  });

  it("fill each path argument from an input with a pattern", () => {
    for (const c of capabilities) {
      for (const [, field] of c.api?.path.matchAll(/\{(\w+)\}/g) ?? []) {
        expect(c.input.required, c.name).toContain(field);
        expect(c.input.properties[field!]?.pattern, `${c.name}.${field}`).toBeTypeOf("string");
      }
    }
  });

  it("leave only get_payment_link to the gateway", () => {
    expect(capabilities.filter((c) => !c.api).map((c) => c.name)).toEqual(["get_payment_link"]);
  });

  it("cover a contract that gives every route but /health a scope", () => {
    for (const [op, scope] of ops) if (op !== "GET /health") expect(scope, op).toBeTypeOf("string");
  });
});
