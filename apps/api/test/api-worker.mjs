// The built API Worker as Miniflare options (v4 shape), for every e2e suite that runs it.

import { randomBytes } from "node:crypto";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));

export const apiWorker = (gatewayPublicKey) => ({
  modulesRoot: root,
  modules: [
    { type: "ESModule", path: `${root}build/index.js` },
    { type: "CompiledWasm", path: `${root}build/index_bg.wasm` },
  ],
  compatibilityDate: "2026-10-27",
  durableObjects: { LEDGER: { className: "TaskLedger", useSQLite: true } },
  queueProducers: { READY: { queueName: "runlater-ready" } },
  queueConsumers: { "runlater-ready": { maxBatchTimeout: 1 } },
  analyticsEngineDatasets: { STATS: { dataset: "runlater_stats" } },
  bindings: { GATEWAY_PUBLIC_KEY: gatewayPublicKey, SECRETS_KEY: randomBytes(32).toString("hex") },
});
