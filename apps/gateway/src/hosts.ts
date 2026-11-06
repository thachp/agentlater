// The gateway answers on two domains during a rename, and hook URLs and connected clients keep
// using whichever one they were given. No redirects: webhook senders and MCP clients POST, and
// don't reliably follow them. OAuth matches a token's resource to the request's origin and Agent
// Auth checks JWT audiences against PUBLIC_URL, so a request has to see the URLs of its own host.

export interface HostEnv {
  PUBLIC_URL: string;
  /** Where hook URLs point: a host of their own. Defaults to PUBLIC_URL. */
  HOOKS_URL?: string;
  /** The gateway's other public URL. */
  ALT_PUBLIC_URL?: string;
  /** The hooks host that goes with ALT_PUBLIC_URL. */
  ALT_HOOKS_URL?: string;
}

const hostOf = (url: string | undefined) => (url ? new URL(url).host : null);

/** The env as this host should see it: unchanged unless the request came in on the other domain. */
export function envForHost<E extends HostEnv>(env: E, host: string): E {
  if (hostOf(env.ALT_PUBLIC_URL) !== host) return env;
  return { ...env, PUBLIC_URL: env.ALT_PUBLIC_URL, HOOKS_URL: env.ALT_HOOKS_URL };
}

/** Agent Auth's slug: clients that connected before the rename keep seeing the name they registered with. */
export const providerNameFor = (publicUrl: string) => (new URL(publicUrl).host.endsWith(".runlater.io") ? "runlater" : "agentlater");

/** Whether this host serves inbound hooks and nothing else. */
export function isHooksOnlyHost(env: HostEnv, host: string): boolean {
  const gateways = [hostOf(env.PUBLIC_URL), hostOf(env.ALT_PUBLIC_URL)];
  return [hostOf(env.HOOKS_URL), hostOf(env.ALT_HOOKS_URL)].some((h) => h === host && !gateways.includes(h));
}
