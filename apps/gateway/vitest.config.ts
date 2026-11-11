import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: { alias: { "cloudflare:workers": new URL("./test/stubs/cloudflare-workers.ts", import.meta.url).pathname } },
  test: {
    include: ["test/**/*.test.ts"],
    // Console output, such as /a2a's line per request, is shown only for a test that fails.
    silent: "passed-only",
    // Bundle the OAuth provider so its `cloudflare:workers` import reaches the alias above.
    server: { deps: { inline: ["@cloudflare/workers-oauth-provider"] } },
  },
});
