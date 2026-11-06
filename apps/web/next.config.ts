import type { NextConfig } from "next";

// A static marketing and docs site: no forms, no user content, no third-party scripts except
// Stripe's on /pay.
const securityHeaders = [
  {
    key: "Content-Security-Policy",
    value:
      "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; " +
      "font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
  },
  { key: "X-Content-Type-Options", value: "nosniff" },
  { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
  { key: "Permissions-Policy", value: "camera=(), microphone=(), geolocation=(), payment=()" },
  { key: "Strict-Transport-Security", value: "max-age=31536000" },
];

// /pay runs Stripe's Embedded Checkout (Stripe.js), which needs Stripe's script, frames and API.
// From https://docs.stripe.com/security/guide#content-security-policy.
const payHeaders = [
  {
    key: "Content-Security-Policy",
    value:
      "default-src 'self'; script-src 'self' 'unsafe-inline' https://js.stripe.com https://*.js.stripe.com https://checkout.stripe.com; " +
      "style-src 'self' 'unsafe-inline'; img-src 'self' data: https://*.stripe.com https://*.link.com; font-src 'self'; " +
      "frame-src https://js.stripe.com https://*.js.stripe.com https://hooks.stripe.com https://checkout.stripe.com https://link.com https://*.link.com; " +
      "connect-src 'self' https://api.stripe.com https://checkout.stripe.com https://merchant-ui-api.stripe.com https://link.com https://*.link.com; " +
      "object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
  },
  { key: "Permissions-Policy", value: 'camera=(), microphone=(), geolocation=(), payment=(self "https://js.stripe.com" "https://*.js.stripe.com" "https://checkout.stripe.com")' },
  { key: "Referrer-Policy", value: "no-referrer" },
];

const config: NextConfig = {
  poweredByHeader: false,
  images: { unoptimized: true },
  // Capability definitions are TypeScript source shared with the gateway.
  transpilePackages: ["@runlater/contracts"],
  // The old domain and www go to the one canonical site. Path and query carry over, and browsers
  // keep the fragment, which is where a /pay link holds its data.
  async redirects() {
    return [
      {
        source: "/:path*",
        has: [{ type: "host", value: "(www\\.)?runlater\\.io" }],
        destination: "https://agentlater.io/:path*",
        permanent: true,
      },
      {
        source: "/:path*",
        has: [{ type: "host", value: "www\\.agentlater\\.io" }],
        destination: "https://agentlater.io/:path*",
        permanent: true,
      },
    ];
  },
  async headers() {
    // Later entries override earlier ones for the same header.
    return [
      { source: "/:path*", headers: securityHeaders },
      { source: "/pay", headers: payHeaders },
    ];
  },
};

export default config;
