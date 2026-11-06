import { CONTACT_EMAIL } from "../../../lib/site";

export const dynamic = "force-static";

const YEAR_MS = 365 * 24 * 60 * 60 * 1000;

// RFC 9116. Expires is set when the site is built, so it moves forward with each deploy.
export function GET(): Response {
  const expires = new Date(Date.now() + YEAR_MS).toISOString();
  const text = `Contact: mailto:${CONTACT_EMAIL}\nExpires: ${expires}\nPreferred-Languages: en\nCanonical: https://agentlater.io/.well-known/security.txt\n`;
  return new Response(text, { headers: { "Content-Type": "text/plain; charset=utf-8" } });
}
