# RunLater — Design

Status: **current**, with [known gaps](#known-gaps) · Owner: Patrick Thach · Last updated: 2026-11-04 · What it does: [PRODUCT.md](PRODUCT.md) · How it's built: [ARCHITECTURE.md](ARCHITECTURE.md)

## In one line

**Most of RunLater's interface is read by a model, so tool descriptions and error messages are design work, as much as the website is.**

## Two kinds of user

| | Agents | People |
| --- | --- | --- |
| They see | Tool names, descriptions, schemas, errors, results, `/llms.txt`, the plugin skill | The landing page and docs, a consent page, a payment page |
| How often | Every call | Rarely, and briefly: one click to connect, one payment |
| What they lack | Memory across sessions, a sense of elapsed time, a public URL, a way to pay | Context: they often arrive from a link their agent gave them |
| What it costs them | Context, since everything we return is read | Attention and trust |

There is no dashboard, on purpose. Anything a person would want to see, the agent can fetch and tell them.

## Designing for agents

Source: [`packages/contracts/src/capabilities.ts`](../../packages/contracts/src/capabilities.ts). The gateway serves these definitions and the docs render them, so there is one description of each tool.

### Principles

| Principle | In practice |
| --- | --- |
| **Assume the session ends.** | The inbox cursor lives on RunLater's side, because an agent can't keep one. Every result can wait for a later session. |
| **Context is the scarce resource.** | `read_inbox` returns 10 events by default and 50 at most. `list_tasks` leaves out response bodies. Hook events keep 16 KiB of a 256 KiB request. |
| **Say what it costs before it's spent.** | Tool descriptions state what each poll, wake and callback costs, and the plugin skill gives the arithmetic: a watch every 5 minutes for 6 hours is 73 attempts. Admission answers `402` up front rather than failing hours later. |
| **Repeating a call should be safe.** | `create_project` returns the same project. `idempotency_key` returns the same task. Crons and secrets replace by name. Delivery to the agent is at-least-once, like delivery to a destination. The one deliberate exception: `create_hook` on an existing name issues a new URL, because that's how a leaked one is rotated. |
| **Errors say what to do next.** | An error has a stable `code`, a readable `message` and, when it's about one argument, a `field`. `project_not_found` says to call `create_project`; `budget_exhausted` points to `get_payment_link`. A model can fix its own mistake from the error alone. |
| **Refuse rather than quietly change the request.** | `{{secret:NAME}}` in a body is an error, not text sent as is. Asking to keep `Authorization` on a hook is a `400`, not silently dropped. |
| **Small literals, no languages.** | `"inbox"`, `"24h"`, `{{secret:NAME}}`, `{ path, in }`. Exact matching only: no regexes, expressions or templates, which are slow and easy to get wrong. |
| **Report what's known, and no more.** | `succeeded` means the destination returned 2xx, and the docs say so. Delivery is at-least-once. No promise of exact timing. |
| **Other people's text is data.** | Captured responses, watch bodies and hook requests come from elsewhere, and tool descriptions tell agents never to follow instructions in them. |
| **Dangerous things stay out of reach.** | Secrets are write-only. A hook URL is shown once. Nothing outside the project can spend its attempts, which is why hooks can't send requests. |

### Conventions

| Thing | Convention | Examples |
| --- | --- | --- |
| Tool names | `verb_noun`, snake case, with the same verbs across features | `create_cron`, `list_hooks`, `delete_wake`; `set_` for one value replaced whole |
| Times | Absolute values are unix milliseconds with `_ms` | `run_at_ms`, `next_run_ms` |
| Durations | A whole number and `s`, `m`, `h` or `d` | `delay: "90s"`, `every: "5m"`, `deadline: "6h"` |
| Names | Chosen by the agent, and they identify the thing | Crons and hooks: `daily-backup`. Secrets: `GITHUB_TOKEN` |
| Report destinations | `*_url` takes a URL or the literal `"inbox"` | `callback_url`, `alert_url` |
| Error codes | Snake case, stable, safe to branch on | `budget_exhausted`, `secret_host_not_allowed` |
| Secrets in results | Names and hosts, never values | `list_secrets`, `get_project.wake` |

### Writing a tool description

- Start with what it does and when to reach for it, not how it's implemented.
- Include one realistic example of anything with structure, such as `until` or `accept`.
- State the cost, the limit, and what happens when it's reached.
- Name the next tool when there is one.
- Mention `"inbox"` wherever a URL is asked for, since most agents don't have one.

### Telling agents the habits

The start-of-session inbox check only works if agents know about it, so it's said everywhere an agent might first look: the hosted server's instructions (`mcp.ts`), the tool descriptions, `/llms.txt` and the plugin's `SKILL.md`. A new habit needs adding to all four.

## Designing for people

### Surfaces

| Surface | Source | The one job |
| --- | --- | --- |
| Landing | `apps/web/app/page.tsx` | Show what it's for, and get it connected in a minute |
| Docs | `apps/web/app/docs` | Quickstart, every capability, delivery rules. Agents read these too. |
| Consent | `apps/gateway/src/consent.ts` | Say what's being connected and what it can do, then one click |
| Pay | `apps/web/app/pay` | Say what's being bought and for which project, then Stripe's form |
| Legal, 404 | `apps/web/app/{terms,privacy,not-found}` | Plain text in the same frame |

The consent page belongs to the gateway Worker and has a strict CSP (`default-src 'none'`), so it can't load the site's stylesheet, fonts or images. Its styles are inline, with the icon as inline SVG. Its colors are a copy of the tokens below, so any brand change has to be copied there by hand.

### Moments that matter

- **Consent is where trust is won or lost.** It names the client and where the person will return to, warns that a loopback redirect could be any local program, and says plainly that there's no account and the connection is the project's only key.
- **Pay has to work for someone with no context.** It says what's being bought, that it goes to "the agent that sent you this link", that leftovers carry over, and what to do if the link has expired. Each failure state tells the person to go back to their agent.
- **The first screen answers "what is this?"** The headline is the outcome ("Your agent's session ends. The work doesn't."), and next to it is a real call and its result.

### Voice

Plain, specific and a little dry. RunLater is trusted with things that happen while nobody is watching, so it doesn't oversell.

| Do | Don't |
| --- | --- |
| Say the number: "50,000 for $5", "100 delivery attempts over 7 days" | "Generous free tier", "affordable" |
| Say the limit of a promise: "A 2xx response means the destination accepted the request; RunLater says so, and nothing more." | "Guaranteed delivery", "reliable" with nothing behind it |
| Short verbs in titles: "Do it later", "Get woken up" | "Leverage", "seamless", "supercharge", "platform" |
| Address the person about *your agent*; describe what the agent does | Blur the two, or call the agent a user in the same sentence as its person |
| Say what's absent: "no dashboard, no sign-up and no API key to paste" | Imply an account exists |
| Tell people what to do on failure: "ask your agent for a new one" | "Something went wrong" alone |
| Show code that really runs: the signature samples are executed in tests | Illustrative code that nobody has run |

Terms are used consistently: *delivery attempt*, *task*, *project*, *inbox*, *wake*, *hook*, *lane*. Facts stated on the site live in `lib/site.ts` and `lib/snippets.ts`, so they can be checked against the code.

### Visual language

Warm paper, near-black ink and one terracotta accent: closer to a printed manual than to a developer-tool gradient. Dark code blocks do most of the visual work, because the product is a set of calls. Source: [`apps/web/app/globals.css`](../../apps/web/app/globals.css).

**Color tokens.** Both themes follow `prefers-color-scheme`; there's no toggle.

| Token | Light | Dark | Use |
| --- | --- | --- | --- |
| `--paper` | `#f5f3ee` | `#161816` | Page background |
| `--card` | `#fffefa` | `#1e211e` | Cards, callouts |
| `--ink` | `#20231f` | `#ecebe4` | Text, primary button |
| `--muted` | `#5d635a` | `#a3a99f` | Secondary text |
| `--line` | `#dedfd6` | `#2f332e` | Borders, rules |
| `--accent` | `#bf4325` | `#e3693f` | Emphasis, eyebrows, step numbers, focus ring. Sparingly. |
| `--soft` | `#fce7dd` | `#3a2219` | Inline code, current nav item, the prompt panel |
| `--code-bg` | `#1f2220` | `#101210` | Code blocks, dark in both themes |
| `--code-ink` | `#ecebe4` | same | Code text |
| `--code-muted` | `#9aa094` | same | Code labels |

**Contrast, computed from the tokens.** All pass WCAG AA for normal text (4.5:1):

| Pair | Light | Dark |
| --- | --- | --- |
| `--ink` on `--paper` | 14.33 | 14.94 |
| `--muted` on `--paper` | 5.57 | 7.42 |
| `--muted` on `--soft` | 5.18 | 6.14 |
| `--accent` on `--paper` | 4.68 | 5.41 |
| `--accent` on `--card` | 5.14 | 4.92 |
| `--code-muted` on `--code-bg` | 5.99 | 7.02 |

**Type.** The sans stack is for prose, the mono stack for code, eyebrows, labels and tool names.

| Role | Size | Notes |
| --- | --- | --- |
| Hero `h1` | `clamp(36px, 5.4vw, 60px)` | Weight 600, `-0.02em` |
| Section `h2` | `clamp(26px, 3.4vw, 36px)` | |
| Card `h3` | 18px | |
| Lede | 19px (landing), 18px (docs) | Muted |
| Body | 16px / 1.6 | Card text 15px, muted |
| Eyebrow | 12.5px mono, uppercase, `0.06em` | Accent |
| Code | 13.5px mono / 1.6 | 12.5px in cards |

**Layout.** `--wrap` is 1120px wide with a 16px gutter each side; sections are 56px apart with a hairline rule; prose is at most 760px wide. Breakpoints are 900px (grids collapse, docs nav moves to the top) and 560px (one column, nav shortens). Corners: 14px for cards and code, 10px for buttons, 999px for the pill.

**Components.**

| Component | Class | Notes |
| --- | --- | --- |
| Button | `.button.primary`, `.secondary` | Ink fill or outline. Hover drops opacity. The accent is never a button fill. |
| Card | `.card` | Hairline border, no shadow. Nothing in the system uses shadows. |
| Row card | `.rows .card` | Words left, code right; stacks below 900px |
| Code | `<Code>`, `.code` | Scrolls sideways and is focusable, with an optional mono label |
| Pill | `.pill` | One per page at most, with an accent dot |
| Eyebrow | `.eyebrow` | Above each section heading |
| Steps | `.steps` | Numbered by a CSS counter, `01` to `04` |
| Callout | `.callout` | Accent left rule. Also carries payment states. |
| Capability | `.capability` | One per tool on the reference page; the name is mono |
| Table | `table`, `.table-scroll` | Hairlines only, wrapped so it scrolls on phones |

**Icon.** A clockwise arrow around clock hands, white on terracotta (`apps/web/app/icon.svg`). The plugin's listing icon is rendered from it.

**Motion.** Almost none: smooth scrolling to anchors, nothing else.

### Accessibility baseline

Keep these when changing pages:

- A skip link to `#main`, and `lang` on the document.
- `:focus-visible` shows a 3px accent outline on links and summaries.
- Code blocks have `tabIndex={0}`, so keyboard users can scroll them.
- `aria-current` marks the current docs page; navs have `aria-label`s.
- Payment states use `role="status"`, `role="alert"` and `aria-live`.
- Decorative images have an empty `alt`.
- Every layout holds at phone width with no sideways page scroll; wide content scrolls in its own box.

## Known gaps

Found while writing this down, from reading the code and computing contrast. Apart from the consent page, which was checked in light and dark, pages haven't been reviewed in a browser, and none with a screen reader.

| Gap | Where | Why it matters | Suggested fix |
| --- | --- | --- | --- |
| **Inter is named but never loaded** | `globals.css:15` | The site renders in Inter only for people who have it installed; others get the system font. | Pick one: load it with `next/font`, or drop it and own the system stack |
| **Smooth scrolling ignores `prefers-reduced-motion`** | `globals.css:38` | It's a small accessibility miss, and the site's only motion. | Wrap it in a media query |
| **Icon and accent are different oranges:** `#db4b27` and `#bf4325` | `icon.svg`, `globals.css:7` | Minor drift. The icon's white on orange is 4.12:1, fine for a graphic. | Decide whether it's deliberate |
| **`--green` is defined and never used** | `globals.css:10` | Dead token. | Remove it, or use it for a status |
| **Hard-coded color outside the tokens** | `::selection` in `globals.css:54` | It won't follow theme changes. | Move it to a token |
| **No social preview** | `layout.tsx` metadata | Links shared in chat show no image or card. | Add Open Graph metadata and an image |
| **Two error shapes** | `apps/gateway/src/index.ts`: `{error: code, message}`; API and `/mcp`: `{error: {code, message, field?}}` | An agent that meets a gateway-level `429` or `404` gets a different shape, against "errors say what to do next". | Align them, checking first what Agent Auth clients expect |

## Not designed yet

- **A dashboard.** Out of scope for v1 ([PRODUCT.md](PRODUCT.md#explicitly-not-in-v1)). If it comes, it starts from these tokens, and its first job is to show what the agent already can.
- **A theme toggle.** The site follows the system.
- **Design files.** The code is the source of truth; there's no Figma library to keep in step.

## Feature design notes

The decisions behind each larger feature, kept as written: [INBOX.md](INBOX.md), [HOOKS.md](HOOKS.md), [HOOK_FILTERS.md](HOOK_FILTERS.md). A new feature of that size gets its own note: the problem, what was decided and rejected, and what was found while building. The rules go in PRODUCT.md.
