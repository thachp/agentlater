// What a payment buys. The gateway charges for it (apps/gateway/src/billing.ts) and the site and
// capability descriptions quote it, so all read this one definition.

export const ATTEMPT_PACK = {
  attempts: 50_000,
  priceCents: 500,
  currency: "usd",
} as const;
