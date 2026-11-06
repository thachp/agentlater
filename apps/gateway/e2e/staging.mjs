#!/usr/bin/env node
// Runs the agent journey against a deployed environment. deploy-env.yml runs it after the
// staging deploy (a failure stops production); monitor.yml runs it daily against production.
//
//   GATEWAY_URL=https://gateway-staging.agentlater.io \
//   RECEIVER_URL=https://runlater-receiver-staging.<account>.workers.dev \
//   RECEIVER_TOKEN=... node apps/gateway/e2e/staging.mjs

import { runJourney } from "./journey.mjs";

const { GATEWAY_URL, RECEIVER_URL, RECEIVER_TOKEN } = process.env;
if (!GATEWAY_URL || !RECEIVER_URL || !RECEIVER_TOKEN) {
  console.error("GATEWAY_URL, RECEIVER_URL and RECEIVER_TOKEN are required");
  process.exit(2);
}

try {
  const result = await runJourney({
    gatewayUrl: GATEWAY_URL.replace(/\/$/, ""),
    receiverUrl: RECEIVER_URL.replace(/\/$/, ""),
    receiverToken: RECEIVER_TOKEN,
    log: (m) => console.log(`journey: ${m}`),
  });
  console.log(JSON.stringify(result));
} catch (e) {
  console.error(`journey failed: ${e instanceof Error ? e.message : e}`);
  process.exit(1);
}
