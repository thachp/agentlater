// Hosts keep the capability list they activated with, so a new capability reaches existing
// hosts only through a migration that rewrites it. The newest such migration must grant
// exactly today's capabilities.

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";
import { authOptions } from "../src/auth";

const DIR = join(dirname(fileURLToPath(import.meta.url)), "../migrations");
/** What the Agent Auth plugin is told to give a new host. */
const [agentAuth] = authOptions({ baseURL: "https://gateway.test", database: undefined, secret: "not-a-real-secret" }).plugins!;
const hostCapabilityNames = (agentAuth as unknown as { options: { defaultHostCapabilities: string[] } }).options.defaultHostCapabilities;

it("the newest host-capabilities migration grants every capability", () => {
  const lists = readdirSync(DIR)
    .sort()
    .flatMap((f) => [...readFileSync(join(DIR, f), "utf8").matchAll(/SET "defaultCapabilities" = '([^']*)'/g)].map((m) => m[1]!));
  expect(lists.length).toBeGreaterThan(0);
  expect(JSON.parse(lists.at(-1)!)).toEqual(hostCapabilityNames);
});
