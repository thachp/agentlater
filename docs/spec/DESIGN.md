# AgentLater — Design

Status: **current**, with [known gaps](#known-gaps) · Owner: Patrick Thach · Last updated: 2026-11-06 · What it does: [PRODUCT.md](PRODUCT.md) · How it's built: [ARCHITECTURE.md](ARCHITECTURE.md)

## In one line

**Most of AgentLater's interface is read by a model, so tool descriptions and error messages are design work, as much as the website is.**

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
| **Assume the session ends.** | The inbox cursor lives on AgentLater's side, because an agent can't keep one. Every result can wait for a later session. |
| **Context is the scarce resource.** | `read_inbox` returns 10 events by default and 50 at most. `list_tasks` leaves out response bodies. Hook events keep 16 KiB of a 256 KiB request. |
| **Say what it costs before it's spent.** | Tool descriptions state what each poll, wake and callback costs, and the plugin skill gives the arithmetic: a watch every 5 minutes for 6 hours is 73 attempts. Admission answers `402` up front rather than failing hours later. |
| **Repeating a call should be safe.** | `create_project` returns the same project. `idempotency_key` returns the same task. Crons and secrets replace by name. Delivery to the agent is at-least-once, like delivery to a destination. Three exceptions: `schedule_http` without an `idempotency_key` schedules again, `create_hook` on an existing name deliberately issues a new URL, because that's how a leaked one is rotated, and `rotate_signing_secret` issues a new signing secret every time, for the same reason. |
| **Errors say what to do next.** | An error has a stable `code`, a readable `message` and, when it's about one argument, a `field`. `project_not_found` says to call `create_project`; `budget_exhausted` points to `get_payment_link`. A model can fix its own mistake from the error alone. |
| **Refuse rather than quietly change the request.** | `{{secret:NAME}}` in a body is an error, not text sent as is. Asking to keep `Authorization` on a hook is a `400`, not silently dropped. |
| **Small literals, no languages.** | `"inbox"`, `"24h"`, `{{secret:NAME}}`, `{ path, in }`. Exact matching only: no regexes, expressions or templates, which are slow and easy to get wrong. |
| **Report what's known, and no more.** | `succeeded` means the destination returned 2xx, and the docs say so. Delivery is at-least-once. No promise of exact timing. |
| **Other people's text is data.** | Captured responses, watch bodies and hook requests come from elsewhere, and agents are told never to follow instructions in them. The warning is in the capture, `read_inbox` and hook descriptions and in the plugin skill, but not yet in `watch_http`'s description (see [known gaps](#known-gaps)). |
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

The start-of-session inbox check only works if agents know about it, so it's said where an agent might first look: the hosted server's instructions (`mcp.ts`), the `get_project` description and the plugin's `SKILL.md`. `/llms.txt` only says to read the inbox next session (see [known gaps](#known-gaps)). A new habit needs adding to all of them.

## Designing for people

### Surfaces

| Surface | Source | The one job |
| --- | --- | --- |
| Landing | `apps/web/app/page.tsx` | Show what it's for, and get it connected in a minute |
| Docs | `apps/web/app/docs` | Quickstart, every capability, delivery rules. Agents read these too. |
| Consent | `apps/gateway/src/consent.ts` | Say what's being connected and what it can do, then one click |
| Pay | `apps/web/app/pay` | Say what's being bought and for which project, then Stripe's form |
| Legal, 404 | `apps/web/app/{terms,privacy,not-found}` | Plain text in the same frame |

The consent page belongs to the gateway Worker and has a strict CSP (`default-src 'none'`), so it can't load the site's stylesheet, fonts or images. Its styles are inline, with the icon as inline SVG. Its colors are a hand-made copy of the site's tokens, so any brand change has to be copied there by hand (it still has the previous palette: see Known gaps).

### Moments that matter

- **Consent is where trust is won or lost.** It names the client and where the person will return to, warns that a loopback redirect could be any local program, and says plainly that there's no account and the connection is the project's only key.
- **Pay has to work for someone with no context.** It says what's being bought, that it goes to "the agent that sent you this link", that leftovers carry over, and what to do if the link has expired. Each failure state tells the person to go back to their agent.
- **The first screen answers "what is this?"** The headline is the offer ("Hand off the waiting."), and beside it one request goes from the agent to AgentLater to a saved result, with the agent free to disconnect in between. The strip below says the same thing as one sentence a person might type.

### Voice

Plain, specific and a little dry. AgentLater is trusted with things that happen while nobody is watching, so it doesn't oversell.

| Do | Don't |
| --- | --- |
| Say the number: "50,000 for $5", "1,000 delivery attempts over 7 days" | "Generous free tier", "affordable" |
| Say the limit of a promise: "A 2xx response means the destination accepted the request; AgentLater says so, and nothing more." | "Guaranteed delivery", "reliable" with nothing behind it |
| Short verbs in titles: "Do it later", "Get woken up" | "Leverage", "seamless", "supercharge", "platform" |
| Address the person about *your agent*; describe what the agent does | Blur the two, or call the agent a user in the same sentence as its person |
| Say what's absent: "no dashboard, no sign-up and no API key to paste" | Imply an account exists |
| Tell people what to do on failure: "ask your agent for a new one" | "Something went wrong" alone |
| Show code that really runs: the signature samples are executed in tests | Illustrative code that nobody has run |

Terms are used consistently: *delivery attempt*, *task*, *project*, *inbox*, *wake*, *hook*, *queue* (`lane` in the API and on `schedule_http`). Most facts stated on the site live in `lib/site.ts` and `lib/snippets.ts`, so they can be checked against the code; some numbers are still written directly in pages.

### Visual language

A dark header and hero over warm paper, near-black ink and one orange accent. One sans face carries everything; headlines are large, tight and heavy, with the accent on the word that matters. Sections alternate between the page and a soft band and change shape as you scroll (a strip, numbered steps, feature columns, tabs, a provider rail, prices, questions) rather than repeating one card. Source: [`apps/web/app/globals.css`](../../apps/web/app/globals.css).

**Color tokens.** Both themes follow `prefers-color-scheme`; there's no toggle. The header, hero, closing call to action and footer carry the class `.dark`, which sets the dark column's values in both themes.

| Token | Light | Dark | Use |
| --- | --- | --- | --- |
| `--bg` | `#fcf9f5` | `#18191b` | Page background |
| `--bg-soft` | `#f2eee7` | `#202225` | Alternating bands, the provider rail, callouts |
| `--bg-raise` | `#f0e9df` | `#24262a` | The Connect section |
| `--card` | `#fffdf9` | `#1b1d20` | The setup sheet, capability cards |
| `--chip` | `#fffdf9` | `#303236` | Secondary button, the hero's speech bubble |
| `--ink` | `#202020` | `#f5f2ee` | Text |
| `--ink-2` | `#45413d` | `#dbd7d2` | Ledes and instructions |
| `--muted` | `#64615d` | `#b9b7b3` | Secondary text, eyebrows |
| `--line`, `--line-strong` | `#ddd7ce`, `#bfb6aa` | `#44464b`, `#5b5e64` | Rules; borders of boxes |
| `--accent` | `#ea5827` | `#ff8a52` | Primary button fill, accent words in headlines, focus ring |
| `--on-accent` | `#21150e` | same | Text on an accent fill |
| `--accent-text` | `#ab401c` | `#ffb28a` | Small accent text: links, labels, icons, the selected tab |
| `--accent-soft`, `--accent-line` | `#fce4d4`, `#efbfa3` | `#34271f`, `#70513b` | Trial banner, status badge, selected provider, inline code |
| `--number` | `#2d3d31` | `#ff8a52` | Step numbers |
| `--ok`, `--ok-soft`, `--ok-line` | `#3e7754`, `#eff5eb`, `#b9d3bf` | `#8edda9`, `#22392e`, `#577f66` | Check marks, the hero's saved result |
| `--code-bg`, `--code-line` | `#232323`, none | `#141517`, `#55585f` | Code blocks, dark in both themes |
| `--code-ink`, `--code-muted` | `#f4f0eb`, `#b4ada6` | same | Code text and labels |
| `--selection`, `--selection-ink` | `#f9c6b3`, `#202020` | same | Selected text |

**Contrast, computed from the tokens.** All but the last pass WCAG AA for normal text (4.5:1):

| Pair | Light | Dark |
| --- | --- | --- |
| `--ink` on `--bg` | 15.52 | 15.76 |
| `--ink-2` on `--bg` | 9.64 | 12.28 |
| `--muted` on `--bg` | 5.87 | 8.79 |
| `--muted` on `--bg-soft` | 5.33 | 7.96 |
| `--muted` on `--bg-raise` | 5.11 | 7.57 |
| `--accent-text` on `--bg` | 5.75 | 10.04 |
| `--accent-text` on `--bg-soft` | 5.22 | 9.1 |
| `--accent-text` on `--accent-soft` | 4.94 | 8.23 |
| `--on-accent` on `--accent` | 5.03 | 7.65 |
| `--ok` on `--bg` | 5.04 | 10.95 |
| `--code-muted` on `--code-bg` | 7.09 | 8.24 |
| `--accent` on `--bg` (large headline words and the focus ring only) | 3.38 | 7.55 |

**Type.** DM Sans is self-hosted through `next/font`, so no reader's browser calls Google; the fallback is the system sans. The mono stack is for code and tool names.

| Role | Size | Notes |
| --- | --- | --- |
| Hero `h1` | `clamp(60px, 6.8vw, 94px)` | 650, `-0.045em`; the last word is accent on its own line |
| Section `h2` | `clamp(36px, 4.2vw, 58px)` | 650, `-0.035em` |
| Section `h3` | 23px | |
| Lede | 21px (hero), 19px (section intros), 18px (docs) | |
| Body | 16px / 1.6 | Fine print 13px, muted |
| Eyebrow | 12px, 650, uppercase, `0.2em` | Muted |
| Code | 13.5px mono / 1.6 | 12.5px in the setup sheet |

**Layout.** `.wrap` is at most 1320px wide with a 50px gutter each side (30px below 1000px, 24px below 700px); sections have 88px of padding (60px on phones); prose is at most 760px wide. Breakpoints are 1000px (tighter grids, docs nav moves to the top), 850px, 700px (one column, the header's links go behind a menu button) and 600px (the provider rail becomes a scrolling row). Corners: 16px for panels, 10px for code, 9px for buttons.

**Components.**

| Component | Class | Notes |
| --- | --- | --- |
| Button | `.button.primary`, `.secondary`, `.outline` | Accent fill with dark text, a chip, or an outline. Hover lifts it 2px. |
| Text link | `.textlink` | Bold, underlined by a rule, usually with an arrow |
| Site nav | `<SiteNav>` | Section links and Connect; a menu button on phones. Without JavaScript the links stay in view |
| Hero story | `.story` | A request, AgentLater, "your agent can disconnect", the saved result, joined by dotted paths |
| Band | `.band`, `.band-raise` | A soft or raised background across the page |
| Steps | `.steps`, `.trial` | Three numbered steps, then the trial banner |
| Features | `.features` | Four columns, each an icon, a title and a sentence |
| Tabs | `<Tabs>` | One panel at a time for long alternatives, with arrow-key support. Every panel is in the HTML; without JavaScript they all show. `rail` puts the tabs in a column beside the panel: the Connect section |
| Setup sheet | `.sheet`, `.copyfield`, `<CopyButton>` | Per provider: the one value a person has to copy, where to paste it, and a command where there is one |
| Prices | `.prices`, `.explainer` | Two columns under a rule, then what counts as an attempt |
| Questions | `.faq` | `<details>`, one per question |
| Icon | `<Icon>` | Line icons that take the text color |
| Code | `<Code>`, `.code` | Scrolls sideways and is focusable, with an optional label |
| Eyebrow | `.eyebrow` | Above each section heading |
| Callout | `.callout` | Accent left rule. Also carries payment states. |
| Capability | `.capability` | One per tool on the reference page; the name is mono |
| Table | `table`, `.table-scroll` | Hairlines only, wrapped so it scrolls on phones |

**Icon.** A clockwise arrow around clock hands, white on terracotta (`apps/web/app/icon.svg`). The plugin's listing icon is rendered from it.

**Motion.** Little: smooth scrolling to anchors, buttons lifting on hover, and a dot travelling down the hero story once, on load. All are off under `prefers-reduced-motion`.

### Accessibility baseline

Keep these when changing pages:

- A skip link to `#main`, and `lang` on the document.
- `:focus-visible` shows a 3px accent outline on links, summaries, buttons and code blocks.
- Code blocks have `tabIndex={0}`, so keyboard users can scroll them.
- `aria-current` marks the current docs page; navs have `aria-label`s.
- Payment states use `role="status"`, `role="alert"` and `aria-live`.
- Decorative images have an empty `alt`.
- Every layout holds at phone width with no sideways page scroll; wide content scrolls in its own box.

## Known gaps

Found while writing this down, from reading the code and computing contrast. The consent and landing pages were checked in a browser, in light and dark (landing also at phone width); the other pages haven't been reviewed that closely, and none with a screen reader.

| Gap | Where | Why it matters | Suggested fix |
| --- | --- | --- | --- |
| **Icon and accent are different oranges:** `#db4b27` and `#ea5827` | `icon.svg`, `globals.css` | Minor drift. The icon's white on orange is 4.12:1, fine for a graphic. | Decide whether it's deliberate |
| **The consent page still has the previous palette** | `apps/gateway/src/consent.ts` | It copies the site's tokens by hand, and the site's changed. | Copy the tokens above into it |
| **`/llms.txt` lacks the start-of-session inbox check** | `apps/gateway/src/discovery.ts` | An agent that only reads this page misses the habit. | Add the sentence |
| **`watch_http`'s description has no untrusted-data warning** | `packages/contracts/src/capabilities.ts` | It returns up to 16 KiB of someone else's response body. | Add the sentence |
| **No social preview** | `layout.tsx` metadata | Links shared in chat show no image or card. | Add Open Graph metadata and an image |
| **Two error shapes** | `apps/gateway/src/index.ts`: `{error: code, message}`; API and `/mcp`: `{error: {code, message, field?}}` | An agent that meets a gateway-level `429` or `404` gets a different shape, against "errors say what to do next". | Align them, checking first what Agent Auth clients expect |

## Not designed yet

- **A dashboard.** Out of scope for v1 ([PRODUCT.md](PRODUCT.md#explicitly-not-in-v1)). If it comes, it starts from these tokens, and its first job is to show what the agent already can.
- **A theme toggle.** The site follows the system.
- **Design files.** The code is the source of truth; there's no Figma library to keep in step.

## Feature design notes

The decisions behind each larger feature, kept as written: [INBOX.md](INBOX.md), [HOOKS.md](HOOKS.md), [HOOK_FILTERS.md](HOOK_FILTERS.md). A new feature of that size gets its own note: the problem, what was decided and rejected, and what was found while building. The rules go in PRODUCT.md.
