// What the journey's requests say when the network fails them. Node's fetch throws a bare
// "fetch failed" and keeps the reason (a refused connection, a reset, a DNS failure, a timeout)
// in `error.cause`; these name the request and that reason. Nothing here retries.

/** The innermost cause: what the socket or resolver reported, rather than fetch's wrapper. */
function rootCause(e) {
  let root = e;
  for (;;) {
    const next = root?.cause ?? (root instanceof AggregateError ? root.errors[0] : undefined);
    if (!(next instanceof Error)) return root;
    root = next;
  }
}

const label = (e) => [e.code, e.message].filter(Boolean).join(" ");

/**
 * The method, host and path of a request, without its query or any credential: the path segment
 * that is a hook's secret is left out, and a token is never in a URL the journey sends.
 */
function where(input, init) {
  const req = input instanceof Request ? input : null;
  const url = new URL(req ? req.url : String(input));
  return `${init?.method ?? req?.method ?? "GET"} ${url.host}${url.pathname.replace(/\/hk_[^/]+/g, "/hk_…")}`;
}

/**
 * `fetch`, but a request the network fails throws a TypeError that names it and its cause, e.g.
 * "POST gateway.agentlater.io/capability/execute failed: ECONNRESET read ECONNRESET". An abort is
 * thrown as it was. Pass it where a library takes a `fetch` too.
 */
export async function request(input, init) {
  try {
    return await fetch(input, init);
  } catch (e) {
    if (!(e instanceof TypeError) || !URL.canParse(input instanceof Request ? input.url : String(input))) throw e;
    throw new TypeError(`${where(input, init)} failed: ${label(rootCause(e))}`, { cause: e });
  }
}

/** An error's message, followed by its root cause when the message does not already say it. */
export function describe(e) {
  if (!(e instanceof Error)) return String(e);
  const root = rootCause(e);
  return root === e || e.message.includes(root.message) ? e.message : `${e.message} (${label(root)})`;
}
