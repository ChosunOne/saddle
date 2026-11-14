# Saddle: design plan

## 1. What this is

A coding-agent harness in Rust. The distinguishing feature is the plugin boundary: plugins are WASI
components, they run in an instance whose authority comes from a declared scope set, and the host
refuses to instantiate a component that imports a scope which was not granted to it.

pi and opencode are the parity benchmarks. They define the feature inventory, not the architecture.
Extensions in both run in-process with the agent's own authority, so neither has a security model for
extensions. That gap is the reason this project exists.

Security rests on declared scopes rather than per-use approval prompts. A human reviews a policy file.
The agent does not get to negotiate.

## 2. Status

v0 is in place: the capability crate, the policy engine with its audit log, the exec sandbox, and a
model loop that sends every built-in tool call through the gate. `saddle` opens it in a full-screen
terminal UI, and `saddle run "<prompt>"` runs one turn without it. The plugin loader starts v1. Sections 3 to 6 are settled and can be built against. Section 8 lists work that was
considered and deferred.

## 3. Settled decisions

### Model

**D1.** Rust host, WASI component model for plugins. A component declares its imports, and the import
set is the authority declaration.

**D2.** Parity means the extension-point inventory of pi and opencode, mapped one-to-one onto scopes.
The extension API and the capability taxonomy are the same artifact.

**D3.** No per-use approvals. Policy is declared and reviewed by a person. It originally lived in
the repository; D42 moved it outside the tree, and D63 defines the current layout.

**D4.** Scopes are a small closed enumeration. Each hook a plugin can register is a grant.

**D5.** `exec` runs commands in a kernel sandbox derived from the principal's other grants (D18, D19).
The child sees the principal's filesystem roots and originally had no network (D79 supersedes that
with a policy-checked proxy), so `exec` lets a principal run programs without widening its reach over
files or hosts. A manifest pairing `exec` with `fs-read` or
`fs-write` is the normal case.

**D6.** The host has no subagent feature. A plugin builds subagents from `model-invoke` plus the tool
scopes already granted. An agent started through `exec` cannot exceed `exec`.

**D7.** Plugin output enters the context as tagged data. It never becomes policy. Writing the system
prompt is root, held by the host alone.

**D8.** A produced tool result is immutable. A plugin may append a correction as a new fact carrying
provenance, never rewrite the original.

**D9.** Two tiers. The manifest sets a ceiling, and the policy file grants a subset of it.

**D10.** Every allow and every deny is logged with the resolved arguments and the rule that fired.

### ABI and loading

**D11.** Scopes are WIT interfaces. Abilities a plugin calls are imports from `saddle:host@0.1.0`.
Hooks the host calls are exports in `saddle:plugin@0.1.0`. Before instantiation the loader checks both
sets against the table below and refuses anything the table does not list. Each plugin's Linker is
also built from its grant set, so an import the check missed still fails to link.

| in the component | treatment |
| --- | --- |
| `saddle:host/{exec, fs-read, fs-write, transcript-read, context-contribute, model-invoke, persist}` import | needs that scope |
| `saddle:plugin/tool` export | needs `tool` |
| `saddle:plugin/observer` export | needs `transcript-read` |
| `saddle:plugin/veto` export | needs `veto` |
| `saddle:plugin/ui` export | needs `ui` |
| `wasi:http/outgoing-handler` import | needs `net` |
| `wasi:http/types` import | ambient |
| `wasi:sockets/*` import | refused |
| `wasi:filesystem/{types, preopens}` import | ambient; the preopen list is always empty |
| `wasi:cli/environment` import | ambient; environment and arguments are empty |
| `wasi:cli/{exit, stdin, stdout, stderr, terminal-*}`, `wasi:io/*`, `wasi:clocks/{monotonic-clock, wall-clock}`, `wasi:random/*` imports | ambient; stdout and stderr go to the debug log |

`saddle:*` interfaces must carry a `0.1.x` version and `wasi:*` interfaces a `0.2.x` version.

**D12.** Plugins reach files only through `saddle:host/fs-read` and `saddle:host/fs-write`. These are
path operations on named roots (`read-file`, `list-dir`, `write-file`, …), and each call passes the
policy engine. `wasi:filesystem` cannot carry the two scopes for three reasons: it puts reads and
writes in one interface, std-based guests import it without opening files, and calls through
wasmtime's implementation never reach the policy engine, which makes D10 logging impossible. Plugin
authors use the SDK wrappers in place of `std::fs`.

**D13.** A plugin instance lives for one call. The host prepares one `InstancePre` per plugin per
session and creates a fresh `Store` and instance for every call. The call's model budget and epoch
deadline live in the store and are dropped with it. State a plugin keeps between calls goes through
`persist`. A `lifetime = "session"` manifest field is the planned addition if a parity feature, such
as an LSP client, needs a long-lived instance.

**D14.** The host pins `wasmtime`, `wasmtime-wasi` and `wasmtime-wasi-http` to one exact version
(`=X.Y.Z`) and upgrades on purpose. Rust plugins build for the `wasm32-wasip2` target with the
`wit-bindgen` macro; any toolchain that emits a component works, because the host reads only the
component's type. WASI 0.2 is the target unless 0.3 is final in the pinned wasmtime. Every import name
contains its package version, so the D11 check also checks the ABI version, and an unsupported version
is refused with a message naming the supported one. Before 1.0 the host supports one minor line at a
time.

**D15.** Crates:

- `saddle-cap`: scopes, roots, grants, policy and manifest parsing, validation, the import check, and
  the exec lowering. The import check is `check_component(granted, imports, exports)` over
  interface-name strings. No wasmtime dependency.
- `saddle-policy`: per-call decisions and the audit log.
- `saddle-sandbox`: applies a sandbox spec to the current process and execs the target; runs a
  sandboxed child from the host side and enforces its timeout. It also builds a standalone
  `saddle-sandbox` binary, which its own tests use as the helper.
- `saddle`: the binary, built-in tools, trust store, model providers, the model loop and the TUI.
- `saddle-wit`, `saddle-loader`, `saddle-sdk`: added at v1 for the WIT files and host bindings,
  component loading, and the guest-side crate.

### Principals and files

**D16.** Policy names roots, never paths. The host owns the name→path table and resolves each name to
a canonical path once per session, and the audit log records the resolution. Built-in roots:

- `workspace`: the git toplevel above the start directory, or the start directory;
- `tmp`: a per-session directory the host creates;
- `config` and `data`: per-plugin directories under the XDG directories (v1).

Extra roots, such as toolchain directories, are defined in `$XDG_CONFIG_HOME/saddle/config.toml` and
never in the repository policy. A root that contains the saddle state directory or config directory is
refused. Callers address files as `(root, relative-path)`. The host resolves the relative path beneath
the root with no `..` and no symlink escape, using cap-std, which uses `openat2` with
`RESOLVE_BENEATH`. Built-in tools receive absolute paths from the model; the host maps each to its
containing root and denies paths outside every root.

A symlink can still point at another path inside the same root, such as `innocent` pointing at
`.git/hooks/pre-commit`. The host follows the link one component at a time, including a dangling link
whose target does not exist yet, and decides on both the requested path and the resolved one. Either
denial refuses the call. A link to an absolute path is refused.

**D17.** The agent is a principal named `agent`, with its own section in the policy file. Built-in
tools pass through the same policy engine, audit log and exec sandbox as plugins. The import check
does not apply to native code.

### exec

**D18.** The host runs every command through a helper process. It starts itself as `saddle __sandbox`
and writes a sandbox spec to the helper's stdin. The helper reopens stdin as `/dev/null`, sets
`no_new_privs`, applies landlock and a seccomp filter to itself, and calls `execve`. A helper process
avoids the async-signal-safety limits of `pre_exec`. Landlock ABI 3 (Linux 6.2) is a hard requirement,
because earlier ABIs cannot restrict `truncate(2)`; on an older kernel every exec request fails with
that reason. The `landlock` crate runs with `CompatLevel::HardRequirement`, since its default
best-effort mode drops rules on older kernels without reporting it. ABI 6 signal scoping is applied
when the kernel supports it. `exec` is unavailable on macOS in v1.

**D19.** The child sandbox contains:

- Filesystem: the principal's `fs-read` roots read-only, its `fs-write` roots read-write, the extra
  sandbox roots from the user's `config.toml`, `/usr`, `/bin`, `/sbin`, `/lib*`, `/etc` and `/proc`
  read and execute, and a few `/dev` nodes. `$HOME` is not granted. `TMPDIR` points at the `tmp` root.
- Network: none. seccomp blocks `socket()` for `AF_INET`, `AF_INET6`, `AF_UNIX`, `AF_NETLINK` and
  `AF_PACKET`, and blocks `io_uring_setup`, `io_uring_enter` and `io_uring_register`, since io_uring
  can open sockets without calling `socket`. `socketpair` stays allowed. Blocking `AF_UNIX` keeps the
  child away from `/var/run/docker.sock` and `$SSH_AUTH_SOCK`; the child also can't reach gpg-agent or
  ssh-agent.
- Environment: an allowlist of `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LANGUAGE`, `LC_*`, `TERM`
  and `TZ`, plus the names the `exec` grant lists. The host sets `TMPDIR`. The provider API key held
  by the host is never passed.
- uid: the user's own. Landlock blocks ptrace of processes outside the sandbox, and ABI 6 blocks
  signals to them.
- Process: cwd is the workspace root, stdin is `/dev/null`, the child leads its own process group, and
  the host kills the group when the command exits, when its wall-clock timeout passes, or when the
  user cancels the turn.

**D20.** Landlock grants cannot exclude a path beneath a granted root. Two consequences follow for a
command run under `exec`:

- It can write the paths that D23 reserves. Until mount namespaces are in use, the host snapshots the
  reserved paths in the workspace before each `exec`, compares them afterwards, restores any change,
  and logs each restored path as a denial.
- It can read and write paths that an `exclude` pattern removes from a grant. Exclusions bind the
  built-in tools and plugin host calls only. A secret that must stay unreadable belongs outside every
  root.

### Policy

**D21.** `intercept` is removed. Observing messages and tool results as they arrive is a push form of
`transcript-read`, delivered through the `saddle:plugin/observer` export. A plugin responds to what it
saw through `context-contribute`, which appends a new fact with provenance beside the original. The
`veto` scope lets a plugin deny a pending tool call with a reason; the denial is logged and shown to
the model as a tagged fact. A veto can only narrow what happens, so it is compatible with D7. Changing
a pending call's arguments is a rewrite and is forbidden.

**D22.** The policy is a TOML file whose location is D42's concern; it contains grants only:
no deny rules, no ordering, no expressions. Anything not granted is denied. A grant may carry a `why`
saying what it is for; it is optional (D41). Exclusions sit inside the grant they narrow, so no rule
overrides another.

```toml
version = 1

[agent]
fs-read  = [{ root = "workspace", why = "reads the repo" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the repo" }]
exec     = { why = "runs builds and tests", env = ["CARGO_TERM_COLOR"] }

[plugin.web-search]
net          = [{ host = "api.search.example", why = "search backend" }]
model-invoke = { tokens-per-session = 200000, why = "summarises results" }
```

- A decision is a lookup, so evaluation is total.
- An allow is explained by the grant's TOML path and its `why`, as in
  `plugin.web-search.net[0]: search backend`. That path is the rule recorded under D10.
- A deny is explained by a generated sentence, as in "plugin.web-search has no net grant covering
  evil.example".
- Loading resolves every root, rejects unknown keys, and checks each plugin's grants against its
  manifest ceiling (D9).

**D23.** Superseded by D44. It was the human gate: the host kept the SHA-256 of the policy file and
of every plugin component and manifest in `$XDG_STATE_HOME/saddle/trust.json`, and no session started
until a hash matched one the user had approved with `saddle trust`, which printed the change in
effective grants. That covered a cloned repository shipping its own grants, and the agent editing the
policy through its `fs-write` grant. Both of those are gone: the policy is no longer in the tree
(D42), and nothing the agent runs can write it, so there was no gate left to keep. D44 records the
deletion. What survives from this decision is the reserved-path list below.

The host reserves `.git/config` and anything under `.git/hooks`, including the same paths under
`.git/modules`, and refuses writes to them under every grant. (The policy file was on this list until
D42 moved it out of the tree, at which point the kernel took over.) Git
hooks and settings such as `core.fsmonitor` and `core.sshCommand` run later as the user, outside any
sandbox. D20 covers writes from `exec`.

Rewriting instructions cannot be granted (D7). The original no-network rule for `exec` (D19) is
superseded by D79: a command's network connections now pass the same policy gate as host requests.

**D24.** The audit log is JSON Lines at `$XDG_STATE_HOME/saddle/audit/<session-id>.jsonl`. Each record
holds `seq`, timestamp, session, kind, principal, scope, resolved arguments, decision, rule,
explanation, and `prev`, the SHA-256 of the previous line. On close, the session's final hash is
appended to `$XDG_STATE_HOME/saddle/sessions.jsonl`. Plugins and sandboxed children cannot reach the
log, because D16 keeps the state directory outside every root. Records hold arguments, not payloads: a write
records path, byte count and content hash, and a request records method and URL with query values
redacted. `saddle audit verify` walks the chain and detects edits and truncation of a session log. The
chain does not stop the user's own account from rewriting a whole log.

### Context and presentation

**D25.** The host stores full provenance (principal, scope, call id, and the fact being corrected, if
any) as metadata on each transcript entry. The model sees an inline wrapper only on content that came
from a plugin: tool results, contributions and veto reasons. User messages and built-in tool results
are untagged. The wrapper's delimiter includes a random per-session value, as in
`<plugin-output-7f3a plugin="web-search">`, and the host strips that delimiter from the content so a
plugin cannot close the tag early.

**D26.** Contribution slots, as WIT records:

- tool spec, through the `tool` scope: an identifier rendered with a plugin prefix
  (`web-search__query`), a description capped at about 1,000 characters, and a JSON parameter schema.
  Tool descriptions are the known MCP poisoning channel, which is why they get the cap and a tag.
- `note`, through `context-contribute`: length-capped text, appended as a tagged fact in a section
  after the system prompt, never inside it.
- `diagnostic`, through `context-contribute`: `(root, path)`, a line range, a severity enum and a
  message, for cases such as LSP errors after an edit.

Plugins cannot add constraints to the policy engine. The narrowing case is `veto` (D21).

**D27.** `ui` grants rendering a fixed set of widgets into a frame the host draws and labels with the
plugin's name: text spans with styles from a closed enum, list, key-value table, progress bar and
status line. All text passes through an escape stripper, since raw ANSI would let a plugin move the
cursor over host output, write the clipboard (OSC 52) or plant links (OSC 8). v1 has no input widgets:
with no approval prompts in the product (D3), a prompt-shaped widget could only be impersonating one.
When parity brings in dialogs, they render inside the labelled frame.

**D28.** A `net` grant names a hostname, a `*.suffix`, or `*` for any public host (D40), allows https
only unless the grant sets `scheme = "http"`, and may restrict the port. The host enforces it in its own send function behind
`wasmtime-wasi-http`'s outgoing-request hook, so the host makes every connection and logs every
request. Raw `wasi:sockets` is not offered (D11). The host resolves DNS itself, refuses loopback,
RFC 1918, link-local and `169.254.169.254` unless a grant names that address, and connects to the
address it checked, which closes DNS rebinding. Grants do not take CIDR ranges. Child processes
originally had no network (D19); D79 supersedes that rule with a policy-checked proxy.

### Model providers

**D29.** Providers are configured in the user's `config.toml`, never in the repository policy, so a
cloned repository cannot point the session at another endpoint or pick which key it sends. The
built-in providers are Synthetic and OpenAI, both over OpenAI-compatible chat completions; a provider
defined in `config.toml` can use that format or the Anthropic Messages API. There is no default
provider: the first run opens the provider list, and `/login` checks the key by listing the
provider's models. D91 supersedes saving that choice as the default: the choice belongs to this
conversation, and only ctrl+s in the model list saves a default. The host holds the conversation in one
neutral form and renders it for each request. A block the loop does not interpret, such as an
Anthropic thinking block, is kept and sent back unchanged in its original position. Reasoning from
an OpenAI-compatible thinking model goes back as `reasoning_content` on every assistant message that
arrived with some; D96 supersedes the earlier rule that kept it only within the turn that produced
it.

- A key comes from the provider's environment variable or, without one, from
  `$XDG_STATE_HOME/saddle/auth.json` (mode 0600), which `/login` in the TUI writes. The state
  directory is outside every root, so neither the agent nor a sandboxed command can read the store.
  Every variable that may hold a provider key is removed from the environment a sandboxed child
  inherits, even when an `exec` grant lists it.
- D91 supersedes the original `/login` and `/model` config writes: only saving a default or adding
  a server writes `config.toml`. Those edits keep its comments and refuse an invalid result.
- A tool call in a reply that hit the output limit or was declined may be incomplete. The host does
  not run it and returns an error result in its place.
- Each model request is logged with its model, stop reason and token counts, or with its error.

**D30.** The first run writes two files into `$XDG_CONFIG_HOME/saddle`. `config.toml` names the
toolchain directories it finds as roots, and grants sandboxed commands write access only to caches
such as `~/.cargo/registry`; write access to `~/.cargo/bin` would let a command plant a program that
later runs outside the sandbox. `policy.toml` is the policy for any workspace without a
`saddle.toml`. It needs no trust: the config directory sits outside every root (D16), so neither a
cloned repository nor the agent can supply or change it. A workspace's `saddle.toml` replaces the
default entirely and must be trusted. The session-start record names the policy file used and its
hash.

**D31.** Each conversation is saved, in `$XDG_STATE_HOME/saddle` (**superseded by D53**: it was JSON
Lines under `sessions/`, and it is now a SQLite tree; the intent below is unchanged): a record naming
the workspace, the conversation items in order, and a turn record before each
prompt naming the audit log that covers it. The state directory is outside every root (D16), so the
agent cannot read earlier conversations through its tools. A conversation can span several runs of
saddle and so several audit logs; the turn records tie them together. `/resume` and `saddle -c`
continue a conversation from the current workspace. On load, a tool call left without a result by
a crash gets an error result, so the history is valid to send again.

**D34.** The window compaction and the status line use is the model's real context limit, not a
default: a guess of 128k is wrong for most models served over these APIs, and being wrong is
expensive in both directions — too small compacts a conversation that still fits, too large lets the
request fail. The number comes from, in order:

- `context-window` under `[providers.<name>]`, the user's own answer, which beats everything;
- what this endpoint stated for this model in its `/models` listing;
- a table of windows published for well-known model families, matched on the longest family name in
  the lowercased model id, so a fresh login is not wrong by default;
- `DEFAULT_CONTEXT_WINDOW` (128k), only when nothing knows the model.

The listing is the only place a server states its own limits, so it is the source, and asking for it
is not tied to `/login`: a session asks when it opens. Its answers are cached in
`$XDG_STATE_HOME/saddle/models.json`, keyed by base URL and then model id, because one model id can
mean different things at two endpoints. The state directory is outside every root (D16), so the cache
is neither readable nor writable by the agent or a sandboxed command. A listing replaces what that
endpoint said before; an entry it omits this time keeps its earlier window, and an entry stating no
window (or a placeholder such as 0) adds nothing rather than answering zero. Servers disagree on the
field name, so all the ones seen in use are read (`context_length`, `context_window`,
`max_model_len`, `max_input_tokens`, `top_provider.context_length`, `meta.n_ctx`).

A session that never lists models would otherwise open on nothing but the cache or the default, so
it asks for itself: the TUI spawns the request when it opens and settles the window again when the
answer lands, updating the header and the loop together; a headless `saddle run` asks before the
first request, where there is no dialog to keep live. Only `context-window` in `config.toml` can
outrank the answer, and both paths re-settle through the same function, so a config that sets one
keeps it. `/login` and `/model` still list, so a fresh login is right in the same way.

The window is settled when the provider is built, and read from there by the loop, the header and
`saddle check`, so no two of them can disagree. The provider remembers which of the sources answered
and `saddle check` prints both the number and the source, which is the quickest way to see why a
session opened on the window it did. `saddle check` does not itself list models: it stays cheap and
works offline, so it reports what a session would open with.

**D35.** A session keeps its own accounting, and selecting in the transcript is saddle's job.

- Saddle handles the mouse itself, so it captures it (`[ui] mouse = false` hands it back to the
  terminal, which brings back its own selection and loses the wheel, the drag and the "more lines"
  badge). Capturing is the only way to have both scrolling and selecting, because a terminal that
  reports the mouse stops selecting on a drag. So the selection is saddle's: a drag anchors on a
  character of the drawn transcript and extends with it, the selected characters are drawn on a
  background of their own, and the button coming up copies the text. Coordinates are in transcript
  space rather than screen space, so a selection survives scrolling, and a drag pulled past an edge
  scrolls a line at a time, which is how a selection grows past one screen. The copy is OSC 52,
  which needs no X or Wayland connection; a terminal that refuses it leaves the highlight as the
  only feedback, and shift+drag still reaches the terminal's own selection in most of them.
- Text is copied per line with trailing padding trimmed, because transcript lines are padded to the
  frame's width for their shaded blocks, and a copy full of spaces is not what was selected.
- A transcript records what each reply cost, as a `usage` record in the same file as its items
  (D31). Loading sums them, so a resumed session opens on the tokens it has already spent rather
  than on zero, and takes the estimate of what its next request would carry from the same function
  compaction uses, so the status line and the compaction trigger agree (D33). A transcript written
  before this record existed has none and opens on zero, which is the truth for it.
- The status line shows `cache <read>/<write>` only where the provider counts writes, which is
  Anthropic's cache creation tokens. A provider that reports reads alone — OpenAI, DeepSeek and the
  local servers, which cache without asking — shows `cache <read>`, because a denominator that can
  never move is noise rather than information.

**D36.** A session shows what it has spent and what is left, and the status line is the user's.

- **Prices** come from, in order: a `[providers.<name>.pricing]` table, in dollars per million
  tokens as model cards publish them; then the `/models` listing, where the server states one
  (OpenRouter publishes `pricing.prompt` and `pricing.completion` as decimal strings per token);
  then nothing. Nothing is not zero: a model nobody priced shows no cost rather than a free one, and
  the config's table exists because most servers state no prices at all. A cache price the server
  leaves out is charged at the input price, so an unpriced cache errs high instead of pretending
  those tokens were free. Prices are stored per token and never rounded until they are displayed.
- A **usage endpoint** is the other place a provider states what is left, and where a credit-based
  provider keeps the balance that headers never carry. It is named by `usage-url` under
  `[providers.<name>]`, fetched at the start of a session and again after each turn, and read into
  the same `{quota}` field as the headers. There is no standard shape, so the JSON is walked and a
  figure is recognised by the words in its path — `remaining_tokens`, `tokens_left`, `total_cost`,
  `credits.remaining` — and a figure that is not found stays absent instead of becoming a zero.
  `saddle check` prints what the page said, or says that it held nothing recognisable, which is the
  way to find out whether a provider's shape is one this reads.
- **Price units.** A listing states a price per token or per million tokens, a million apart, so the
  unit is taken from a stated one (`unit`, `per`, or a `per_million` object) and otherwise from the
  magnitude: no real model charges more than a thousandth of a dollar per token or less than a
  hundredth of a dollar per million. A `[providers.<name>.pricing]` table is per million, as model
  cards publish, unless it says `unit = "token"`. Synthetic writes prices per token as strings with
  a currency symbol (`"$0.0000006"`) and names the cache prices in the plural
  (`input_cache_reads`), so a symbol and a thousands separator are stripped and both spellings are
  read. A model that states zeros is free, which is no price rather than a price of zero.
- When a usage page holds nothing the parser recognises, `saddle check` prints the paths its numbers
  are at rather than the numbers themselves: the field names are what teaches the parser a new shape,
  and the figures are the user's business.
- **Quota** comes from the response headers of the request that was just answered, because that is
  the only place a provider states what is left: `x-ratelimit-*` (OpenAI-compatible, which names a
  limit beside the remainder) and `anthropic-ratelimit-*` (which names the resource, then the field,
  and often states only one of the pair). Whichever parts a server sent are shown; a server that
  sends none never calls the sink, and the status line keeps its length. The headers are read before
  the body is taken, on the request thread, and handed to the sink that already carries notes, so
  the quota arrives while the reply streams rather than after it.
- **Cost** is accumulated per reply from the price in force, and recorded in the transcript's usage
  record (D35) so a resumed session keeps the total it had. It is the price times the tokens the
  server reported, which is the provider's own arithmetic, not a local guess at tokenisation.
- A figure nobody can answer is left out entirely rather than guessed, and that is silent by design:
  the field renders as nothing and its separator goes with it, so a provider that prices nothing
  produces the line that existed before these fields did. `{cost}` shows `$0.00` as soon as a price
  is known, which is what makes "nothing spent yet" distinguishable from "no price", and `saddle
  check` reports which source answered the price or that none did. No note is printed in a session
  for either gap: the default template names both fields, so a note would fire in every session with
  a provider that states neither, and the fix is a line of config that `check` names exactly.
- **The status line is a template**, `[ui] status`, of `{field}` placeholders: `{provider}`,
  `{model}`, `{input}`, `{output}`, `{cache}`, `{cachehit}`, `{ctx}`, `{window}`, `{percent}`,
  `{cost}`, `{quota}`, `{workspace}`, with `{{` and `}}` for literal braces. It is parsed when the config is read and
  again when `/settings` saves it, so a typo is reported rather than drawn. A field with no answer
  renders as nothing and the separator before it is held: if a later field has a value the separator
  moves in front of that one, which is why the default line reads the same whether the cost, the
  quota, both or neither are known. The default is the line the UI has always drawn, and two fields
  added to it.

**D37.** A prompt runs until the model stops calling tools. There is no request limit by default.

- The loop's cap was 50 model requests, which is a guess about how long a task takes, and it fails
  at the wrong moment: a refactor that needs 60 requests stopped mid-way and asked the user to send
  another message to continue. A limit the user wants is a limit the user sets, through `--max-steps`,
  `max-steps` in `config.toml`, or `/settings`, and one that is set still stops the loop and says so
  rather than looping in silence.
- The loop is bounded in the ways that actually matter and were already there: the turn ends when a
  reply calls no tool, `esc` cancels between requests, compaction keeps the context inside the
  window (D33), and an answer that would exceed the window is retried once after compacting. A
  model stuck repeating one call is therefore bounded by the user's attention and the audit log, not
  by a number in the config.
- `max-steps = 0` and an emptied `/settings` field both mean no limit, since a prompt allowed zero
  requests could not do anything at all.

**D38.** A turn stops when the model keeps asking the same question and getting the same answer.

- With the request cap gone (D37) the loop relies on the model making progress, so it is the loop
  that notices when none is made. A call is the same when its tool and its serialised arguments
  match; an answer is the same when its success flag and its content match, by hash. A batch that
  repeats the previous batch on both counts `REPEATS` (three) times running stops the turn with a
  note, and the conversation is left valid to continue.
- Both halves are needed. Comparing calls alone would cut short the legitimate case: polling a build,
  or listing a directory that is filling up, issues one identical call until it changes. Comparing
  the answers means the detector only fires when the model cannot learn anything new from repeating
  itself, which is the definition of the loop worth stopping.
- The comparison is deliberately blunt — no argv or path semantics, nothing that inspects a tool's
  arguments — so it cannot become a second, half-right implementation of a tool's meaning, and a new
  tool is covered the day it is added. A diff is not part of the answer, because the model never
  sees one (D24).
- Stopping is not a denial and is not audited as one: the note goes to the UI, and the turn ends
  like any other that did not answer, so a queued prompt returns to the editor. The user sees the
  count and can send something different.

**D39.** The agent reaches the network through a tool the host runs, never through `exec`.

- Giving `exec` sockets would hand network access to every program a command can start, and to every
  Makefile a plugin can write — the failure D19 exists to prevent. So the capability is a tool:
  `fetch`, one GET, where the **host** makes the connection after the engine has decided, and the
  decision and the redacted URL go to the audit log like any other call (D10, D24).
- It is the eighth built-in tool and it is not privileged: it passes the same engine, the same audit
  log, and it is denied by default because a policy without a `net` grant denies everything. A
  `net` grant names a hostname or `*.suffix`, and `https` only unless the grant sets a scheme, so
  reaching the network is something a person wrote down per host.
- D28's address rules are enforced in the tool rather than the engine, because they are about the
  address and only the tool resolves one: loopback, RFC 1918, link-local, unique-local, CGNAT, the
  unspecified address and `169.254.0.0/16` are refused unless the URL is a literal address that a
  grant named. DNS rebinding is closed by giving ureq a resolver that returns only the addresses
  that were checked, so the address approved is the address dialled.
- A URL is parsed here rather than by a library, because the parts are what a grant matches on:
  userinfo is refused rather than stripped (`https://user@host/` reads as one host and means
  another), an IPv6 literal keeps its brackets and its colons are not a port, and any scheme other
  than http or https is refused before anything is sent.
- This does not give the *harness* network access. A sandboxed agent already runs under seccomp with
  no sockets (D19), so implementing this tool changes what saddle can do, not what the agent running
  inside a sandbox can do; that needs a grant in the harness's own profile, which is a human's file.

**D40.** A `net` grant may name every host as `*`, and filtering stays the preferred form.

- `host = "*"` matches any host at all, on the grant's scheme and port like any other grant: https
  and 443 unless the grant says otherwise, so a wildcard does not open plain http or an odd port by
  accident. It exists because a task often cannot know its hosts in advance — a package registry, a
  documentation site, whatever a build prints — and a policy that must be re-trusted for each one
  teaches people to paste grants without reading them, which is worse than one grant that says what
  it means.
- The reason a wildcard is tolerable is that the address rules are not in the grant (D28, D39): the
  `fetch` tool still refuses loopback, RFC 1918, link-local, unique-local, CGNAT and the metadata
  address, and a wildcard **does not exempt them**. The exemption for a private address is decided
  from the grants rather than from the shape of the URL — a grant has to spell the address out — so
  `*` reaches the public internet and not the machine the agent is running on. That distinction is
  why the check moved from "the URL is an IP literal" to "a grant names this host": the first form
  would have made `*` mean the local network the moment someone typed `http://127.0.0.1/`.
- `*.example.com` remains the better grant wherever the hosts are known, because it bounds the
  damage of a prompt injection to one domain. `saddle check` prints a wildcard as
  `https://*:443`, so it is visible where a person reads the policy.
- A refusal for a local server says so in its own words, before the ordinary denial: a wildcard
  grant can cover the scheme and the port, so "no net grant covering http://127.0.0.1:8080" would
  leave the actual reason unsaid. The tool prints the grant that would allow it, with the host as
  written, the port when it is not the scheme's default, and the scheme when it is http. `localhost`
  is recognised without a lookup, and any other name is only resolved once the policy has allowed
  the request, so a denied host is never looked up.

**D41.** A grant's `why` is optional.

- It was required (D22), on the argument that a person approving a policy should see why each grant
  exists. Writing one for every grant of a large policy is also the kind of friction that produces
  `why = "needed"`, which carries no more information than an empty field and looks authoritative
  while doing so.
- So a grant without one is accepted, an empty or whitespace-only one counts as absent, and nothing
  prints a dangling marker for it: `saddle check` shows `agent.net[0] = https://*:443` with no `#`,
  and an allow is explained by its rule alone (`agent.exec`) rather than by `agent.exec: `. A deny
  is unaffected, since denials are generated sentences and never quoted a `why`.
- It stays worth writing, and it shows where the policy is read: in the grants `saddle check`
  prints, and beside each allow in the audit log (D10). A policy
  with reasons is easier to re-review months later; a policy without them is still enforceable, and
  the decision is not to make that the tool's problem.

**D42.** A workspace's policy lives in the config directory, never in the tree.

- `$XDG_CONFIG_HOME/saddle/policies/<key>.toml`, where the key is the workspace's canonical path
  with `/` written as `%`. The first session in a workspace creates it empty, as a header; it is
  layered on the global policy rather than replacing it (D43), so it holds only what differs for
  this directory.
- This replaces "a `saddle.toml` at the workspace root, gated by a trust hash" (D22, D23). The
  reason is the hole D20 was compensating for: landlock grants a subtree and cannot carve a path out
  of one, so a workspace file is writable by any command the agent runs, and the snapshot/restore
  that guarded it is a race a `setsid` child wins by writing after the restore. The config directory
  is outside every root (D16), so the kernel refuses those writes outright — demonstrated, not
  argued: a command writing the policy gets `Permission denied` and the file is unchanged. The
  protection moved from a compensating control to an enforcement boundary.
- A `saddle.toml` in the tree is therefore not read. It is reported once, naming the file that does
  apply, because silently ignoring a file that looks authoritative is worse than either reading it
  or refusing.
- The trust gate is gone (D44): nothing the agent runs can write the file, so there is nothing for a
  hash to detect. `saddle check` prints what is in force, which is what a person reads before
  changing a grant.
- The cost is that a policy is no longer declared in the repository, so it cannot be reviewed in the
  same diff as the code and is not shared by copying the tree. That is the trade: authority may not
  live where the governed process can write.

**D43.** A workspace's policy is layered on the global one, and is the authority for that directory.

- The global `policy.toml` is the base for every workspace. A workspace's file under `policies/`
  holds what differs: a grant it does not mention keeps the global value, and one it does mention
  replaces that grant for the scope — whether that makes it **narrower or wider**. It is created as a
  header with no grants, and an empty file and no file mean the same thing.
- Widening is deliberate, and safe for one reason: the file is in the config directory (D42), which
  no root contains, so nothing the agent can run writes it — the kernel refuses. It is a person's
  declaration about one directory, the same trust level as the global file, so there is no reason to
  let it only subtract. A check that refuses a wider grant would be enforcing a rule about a
  document no governed process can author.
- The first version of this decision did refuse widening, on the argument that a project file is
  written by whoever clones the repository. That was true of the pre-D42 design, where the policy
  sat in the tree; after D42 the premise was gone and the check only made the workspace file a
  second-class citizen. It was removed rather than left as a reassuring-looking no-op.
- What is still refused is a grant that cannot be resolved for the session at all — an unknown root,
  a malformed host, a bad plugin id — exactly as in a single-file policy. Layering changes which
  file a grant comes from, not what a grant may say.
- `saddle check` prints both files in the order they apply and the layered result, so a change to
  either file shows up as what it does, and reading it is the moment a wider grant is meant to be
  considered.

**D44.** `saddle trust` and the trust store are deleted.

- D23's gate stopped being a gate during D42/D43 and did not say so: the session stopped consulting
  the store, so `saddle trust` printed a diff and recorded a timestamp while refusing nothing. A
  security feature that looks like a gate and is not one is worse than none, because it invites
  reliance it cannot bear.
- What it used to defend is now defended by construction: the policy file is in the config directory,
  which no root contains (D42), so the kernel refuses every write from the agent, from a sandboxed
  command and from a `setsid` survivor. There is no hash to compare because there is nothing to
  detect, and a review step that changes no outcome is ceremony.
- The review itself survives as `saddle check`: it prints both policy files with their hashes, the
  roots, and the effective layered grants, which is what a person needs to read before editing a
  grant. `trust.json` is simply no longer read; an existing file is left alone, and deleting it is
  harmless.
- Changing a grant means editing the file; a running session reads it again when a grant is changed
  from `/permissions` (D47), and the next one reads it at open.

**D45.** There is a permissions editor, and the agent cannot open it.

- `/permissions`, or the `Permissions for this project` row in `/settings`, lists the grants the
  session is running with and edits two things: the exclusion list of a filesystem grant, and
  whether a scope with no arguments is on. Everything else is listed and locked, with a sentence
  saying where it is changed: `exec`, a `net` host, a `token` budget and the plain scopes have
  arguments, and a menu cannot ask for them without becoming a form as fiddly as the file.
- The line is deliberate: a keypress that hands over `exec` or a host is indistinguishable from an
  accidental one, and the file is where a grant is written down where a person can read it. The
  editor does the routine work — excluding a path — and leaves the capability decisions to the file.
- It is a human's tool. No tool exposes it, nothing the model can call reaches it, and the system
  prompt tells the model not to ask for a grant as part of a task (D17). The editor writes the
  workspace's own policy file (D42) and never the global one: a change made while working in a
  project is a change to that project.
- Every change is validated with the same `Policy::parse` a session uses before it is written, so a
  bad pattern leaves the file as it was and the message in the dialog rather than a policy that will
  not load. Exclusion patterns are also checked for the mistakes worth catching early: `..`, a bare
  `/`, and an unbalanced `[`.
- A change takes effect at once: the session reads the policy again and the model is told (D47).

**D46.** A grant is added from inside the application by typing it, not by opening the file.

- `a` in the permissions editor opens a one-line prompt; what it accepts is on screen while it is
  typed: `net <host>[:port]`, `root <name>`, `read <name>`, `write <name>`, `exec`, `tokens <n>`,
  with an optional `# why` that becomes the grant's reason (D41). Nothing is added by a keypress
  alone: the line is typed, which is the difference between choosing a grant and happening to press
  a key on a screen.
- The forms are validated before anything is written, and each refusal names the fix: a root that
  this session does not have is refused with the list of roots it does have, since adding a root is a
  `config.toml` change and not a policy one (D30); a host goes through the same `HostPattern` the
  policy parser uses; a second `exec` or a second budget is refused rather than silently doubled. The
  result is then parsed with the same `Policy::parse` a session uses, so a file is never left
  unloadable.
- A file of nothing but comments gets its `version = 1` line when the first grant is added, which is
  what makes the created-but-empty workspace policy (D43) usable without opening it.
- This is the deliberate answer to "how do I add a network grant from inside": type `net
  api.example.com` and read the line `saddle check` shows. The editor still does not offer a menu of
  capabilities to toggle (D45); it offers a prompt in which the person states the grant, which is the
  smallest thing that removes the need to hunt for the file without making a capability a keypress.

**D47.** A grant changed from the editor takes effect at once, and the model is told.

- The permissions editor writes the file, then the agent thread reads the policy again, rebuilds the
  engine, and regenerates the grants section of the system prompt. The prompt is sent with **every**
  request, so replacing that section is the notification: the next request carries the new grants,
  and a call refused under the old ones may now be allowed. Nothing is injected into the conversation
  as a fake user message.
- This is why the prompt is split into a base and a grants section: `system_prompt` is
  `base_prompt` followed by `grants_section`, and a reload replaces only the second. The base is
  everything about the workspace, the roots and the instruction files, none of which changes here.
- The reload happens on the agent thread, in the same queue as `SetProvider` and `SetMaxCompaction`,
  so a change made during a turn waits for the turn to end. A policy is never swapped underneath a
  request that is already in flight.
- The new file is parsed and layered exactly as at open, so a reload of a broken file fails and the
  session keeps the grants it had, saying so rather than running on a policy half read. The audit log
  gets a `policy` record with the path and both hashes, which is what makes the change part of the
  record instead of a silent mutation of the session's authority.
- The system prompt's own sentence about this was updated with it: it used to say the policy could
  not change during a session, which was true until this decision.

**D48.** `fetch` may send headers, and a header value never leaves the request.

- The tool takes an optional `headers` object of name to value, so a keyed endpoint is reachable —
  the provider allowance (`/v2/quotas`) needs an `Authorization` header and no amount of URL
  rewriting gets around that. Values are carried to the request and nowhere else: not into the audit
  log, not into the tool result, and not into the conversation.
- The audit record carries the header **names** only (`"headers":["authorization"]`). That answers
  the question a record should answer — what kind of request was this — without writing a secret into
  a file that outlives the session. The same reasoning as the existing query redaction (D24).
- What a header may not do is change where the request goes: `host`, `forwarded`,
  `x-forwarded-for`, `x-forwarded-host`, `x-real-ip` and `via` are refused, because the grant was
  checked against the URL and a header that redirects it would make that check theatre. A name must
  be a token, a value may not carry a control character (a newline would split the request), and a
  value is bounded at 8 KiB.
- The redirect that follows is explicit: a secret belongs in a header, never in a query string,
  because a query string is part of the URL and the URL is recorded. `saddle check`'s diagnostic
  prints the *paths* of the numbers in a usage page and not the numbers for the same reason.

**D49.** A usage page shows one figure: the allowance that governs the work.

- Synthetic's `/v2/quotas` reports three pools at once: `planWeeklyLimit` (the plan's percentage),
  `subscription` (500 chat requests per five-hour window) and `search.hourly` (web searches). Only
  the first decides whether a week of work is possible; the other two are refilled faster than they
  can be spent and are noise on a status line. The page is read for the weekly figure, and the pools
  it does not show are left absent rather than shown as zero.
- The shape is read at its exact paths. A word-matching reader would take whichever pool sorts
  first, and here that is `search` — so a status bar built on words would have shown the *web-search*
  allowance as the chat allowance, with a figure plausible enough that nobody would question it.
  Word matching stays as the fallback for a provider nobody has described yet.
- When a plan states no weekly figure, the request pool is used: `subscription.requests` is what has
  been *spent* of `subscription.limit`, not what is left, because it reads 0 against a cap of 500
  with a refill hours away — had it meant what is left, the account would be locked out at the moment
  the page reports it working (D49).
- The fallback path derives the same way when it can: `requests_used` with `requests_limit` and no
  `requests_remaining` gives the difference, and usage with no cap states nothing rather than zero.
  That is why `limit` and `cap` are matched bare: that is what a cap is called next to its pool.
- `Quota` keeps fields for a token count, a request pool, a money balance and a percentage because
  the source decides which are filled: the headers of some providers state one, an endpoint another.
  A figure that is absent is not a figure of zero.
- A weekly plan states a percentage rather than a count, so `Quota` carries `percent`.
- The status bar shows the allowance as `94% ↺1h` — the figure, then a countdown marked `↺`. The bar
  has one row for everything, so a figure that needs a sentence belongs in `saddle check`, which
  prints `94% of the plan left, resets in 1h`. A refill is counted from the clock the frame is drawn
  with, which is passed into the renderer rather than read inside it, so the same state renders the
  same line twice. Rounding is up, so the last minute reads `1m` while there is still time rather
  than `0m`. A provider that spells the reset as a duration (`6m0s`) is not converted: it is already
  short, and turning it into an instant would mean guessing when it was measured.

**D50.** A cache miss is the user's news, not the model's.

- A provider that caches the prompt prefix bills the reused part at a fraction of the input price.
  When the prefix changes — the system prompt is reloaded (D47), the tool list moves, an earlier
  message is rewritten — every request of that turn reads the whole prompt again at full price.
  That is several times the cost of the same turn and it is invisible: the token counts are similar,
  the answer is similar, and only the bill says anything. So a turn whose reply reads nothing from a
  cache that has demonstrably been working says so: `cache miss: 10.0k tokens read at full price`.
- It goes through `Sink::note`, which reaches the transcript in the TUI and stderr in line mode, and
  never through `Item`, which is what a request is built from. The model is not told that its prompt
  was re-read, any more than it is told what the status bar says: a notice is a remark to the person
  at the terminal, and putting it in the conversation would both waste tokens and invite the model to
  comment on infrastructure it cannot change.
- A cold start is not a miss. The first request writes the cache it will read next time, so it reads
  nothing and nothing is wrong; only a request that reads nothing *after* caching has been seen
  working is worth a line. A provider that never caches (`warm` never set) is never warned about,
  and a prompt below the 1024-token floor providers cache is not either.
- One line per turn. A turn that misses at every step is one story, not ten: the rest of the turn's
  requests almost always miss for the same reason, and the hit rate on the status bar carries the
  ongoing shape of it.
- No new audit record. Each reply's usage is already in the log, so a miss is in the record by
  arithmetic; an extra record would say the same thing twice and give a second place to keep in step.
- `{cachehit}` on the status bar is `hit 96%`, the cached share of everything sent, computed from the
  session totals. It is left out while nothing has been cached at all, rather than claiming `hit 0%`
  on every request of a provider that does not cache, and rounded down so the line never claims a
  better share than was measured.

**D51.** `read` returns a page, not a file.

- An uncapped `read` is a hole in the context budget: a 4 MB generated file or a log went into the
  conversation verbatim, cost real money, and could end the turn. So a page is at most 2000 lines
  **and** at most 50 KiB, whichever comes first, and `offset`/`limit` page through the rest. The line
  cap alone would not do, because one line can be a minified bundle; the byte budget alone would cut
  off a long file at an arbitrary place with no way to ask for the next part.
- The note that ends a cut page is the instruction: `[showing lines 1-2000 of 5000; call read again
  with offset=2001 for more]`. `offset` is 1-based, which is what `grep` reports, so a match and a
  page are in the same coordinates.
- Lines are not numbered in the body, unlike the other agents. The model edits by matching text
  exactly, and a column of numbers in front of every line is an invitation to copy one into `edit`'s
  `old` and fail. The trade is that a page does not say where it starts; the note does.
- A file that fits on one page comes back byte for byte, trailing newline included. Splitting it
  into lines would drop that newline, and `edit` matches exactly, so the file would be read without
  the character it ends with and then never match.
- One line longer than the whole budget is handed over truncated with a note saying how long it is,
  rather than refused: the point is to bound the context, and a minified file still has an answer in
  it. A page never ends mid-character.

**D52.** `saddle run --json` writes one JSON object per line on stdout.

- It is the machine-readable form of the same events the TUI gets: `session` first, then `text` as
  the answer streams, `call` with the arguments as the model sent them, `result` with `ok` and the
  content, `reply` with the usage and the cost, `note`, `quota`, `compaction`, `usage` for the turn's
  totals, and `turn_end` naming how it ended. A caller that embeds saddle — which is the point of
  `saddle-sdk` (D15) — parses that instead of prose.
- Nothing else goes to stdout, and the host's own errors keep the `saddle:` prefix on stderr with a
  non-zero exit, so a stream of valid objects plus a clean exit is the success signal. `--json`
  implies line mode even on a terminal: a caller that asks for JSON does not want a full-screen UI.
- The encoder escapes control characters, so model text cannot move a cursor or write a clipboard
  (D27), and the stream stays one object per line whatever the model sends.
- `end` is a fixed word per outcome (`answered`, `cancelled`, `failed`, `cut`, `step_limit`,
  `repeating`, `declined`), because a caller switches on it and a reworded string would silently
  break that.

**D53.** Conversations are saved in SQLite, as a tree.

- A conversation is not a list. `/tree` continues from an earlier prompt, which means the message it
  branched at has more than one continuation, and a JSON Lines file cannot say that without copying
  everything. So the store is one SQLite database in the state directory: `sessions` rows, and
  `nodes` rows with a `parent` pointer, each node being one item, one usage record, one turn marker
  or one compaction. The conversation is the path from the root to the session's current leaf; the
  other branches stay in the table.
- One database file, not one per session: a fork and the session it came from are then in the same
  place, and `list` is a query rather than a directory scan. The workspace is a column, so a listing
  is still per workspace and one workspace's conversations are not offered in another.
- A fork copies the nodes it keeps into a new session and leaves the original alone, so both can be
  resumed and neither edits the other's history. Copying rather than sharing also means the tree
  cannot grow a node with two parents.
- `/clone` is the case with no point to choose: it copies the whole active branch into a new
  session. `/tree` covers the other case, continuing from a point inside the session (D54).
- What the copied branch spent comes with it. It really was spent, and a resumed session that opened
  on zero would understate the cost of the conversation it is continuing.
- SQLite is bundled, so building or running saddle needs no system library, and a message is
  committed or it is not — where a torn last line used to be possible. Two connections are open in a
  process (the agent thread appends, the UI thread lists), which is what WAL and a busy timeout are
  for.
- The JSON Lines files earlier builds wrote are imported once, on first open, and renamed
  `.jsonl.imported` rather than deleted: an import that got something wrong must not be the thing
  that loses a conversation. A body that does not parse is skipped on load for the same reason.
- The database is in the state directory, outside every root (D16), so the agent still cannot read
  earlier conversations with its own tools. Nothing about the move widens what the agent can reach.

**D54.** `/tree` moves within the session, and escape twice opens it.

- The tree lists **every message** in the session, not just the prompts, in the order they were
  made, and dims the ones the next request would not carry. Listing only the active branch is the
  tempting implementation and it strands work: after a rewind to the start the active branch is
  empty, so that view shows nothing at all and the conversation has no way back. Everything stays
  listed, so everything stays reachable.
- Choosing a **prompt** puts its full text back in the editor and moves the conversation to the
  message *before* it. Sending it again — edited or not — then makes another branch, which is the
  point of the feature. Choosing anything else (a reply, a tool result, a summary) continues after
  it with an empty editor.
- Nothing is deleted by a rewind. The nodes after the new leaf stay in the database, marked as not
  active, and choosing one puts the conversation back onto that branch. Losing work to a
  navigation key is the failure this rule exists to prevent.
- Two escapes on an **empty** editor open it, as in pi. With text in the editor escape is not a
  gesture, so a half-written prompt is never lost; while a turn runs escape is cancel, as before;
  and any other key breaks the pair. There is no setting for it: pi needs `doubleEscapeAction`
  because it has a configurable keybinding layer, and saddle does not. `/tree` is the same door.
- A row says what the message did, and a reply that only calls tools is the common case rather than
  an edge: in a working session there is one per tool round trip, and every one of them has no text
  part at all. So a reply with no text reads `calls read`, or `calls edit, bash` — the tool names,
  up to three — and tool results read `2 tool results, 1 failed`. A thinking model that answered
  with reasoning and nothing else reads `thought, said nothing`. `(no text)` on all of them would
  make the tree a wall of identical rows that say nothing, which is the state this replaced.
- The controls are pi's: up/down, page up/page down, home/end, enter, escape and ctrl+c to close,
  ctrl+u for the prompts only and ctrl+a for everything, ctrl+left and alt+left for the previous
  prompt and ctrl+right and alt+right for the next, ctrl+x to copy the whole selected message.
- `/clone` is the separate thing: it copies the active branch into a new session, so the copy can
  be worked on independently. A branch point inside one session and a second session are different
  answers to "I want to try something else", and both are cheap to keep.

**D55.** A conversation can be named and deleted; the schema migrates rather than resetting.

- `/name [name]` names the conversation, and with no argument clears the name. A name replaces the
  first prompt as the title in `/resume`, because that is what a person recognises their own work
  by once there are thirty conversations. Clearing it puts the prompt back rather than leaving an
  empty row.
- The picker renames with ctrl+r and deletes with ctrl+d **twice**, the second press confirming:
  deleting is not undoable, and a single key is one slip away. Any other key cancels the pending
  delete. The rename is edited in the prompt, which already has an editor, rather than growing the
  picker one.
- Deleting refuses the conversation this session is writing to. `/clear` first, then delete: the
  alternative is a session appending to a session that no longer exists.
- `/session` reports the id, the name, how many messages are on the branch, the tokens and the cost.
  That is what makes an id in an audit log or a bug report mean something.
- The schema is versioned and migrated in place. Version 2 adds `sessions.name` with an
  `ALTER TABLE`, run only when the file is older than the build, and written so a second run cannot
  fail. A database from a **newer** build is still refused rather than guessed at: a wrong guess
  here costs a conversation. Nothing is ever recreated, because the file on disk is the user's work.

**D56.** `patch` changes several files in one call, and one denial refuses all of them.

- The format is apply-patch's — `*** Begin Patch`, `*** Add File:`, `*** Update File:` (with
  `*** Move to:`), `*** Delete File:`, `*** End Patch` — because models already know it. A rename and
  its call sites become one request rather than one per file, which is what the round-trip cost was
  actually paying for.
- Authority is unchanged and unreduced. Every path is resolved and **decided** before anything is
  written, each through the same engine as `write`, and a rename is decided on both ends. One denial
  refuses the whole patch, and the tool says so: a half-applied change is a state the user did not
  ask for and the model cannot reason about. Verified through the binary: a patch whose second file
  is excluded creates nothing at all.
- Matching is strict. A hunk's `-` and context lines must appear exactly once in the file; zero
  matches and two matches both fail, with the `@@` marker in the message and nothing written. A hunk
  that removes nothing is refused, since it would match anywhere.
- The new contents of every file are computed in memory first, then written. Two operations on one
  path are refused rather than ordered.

**D57.** The reasoning effort is a live setting, not a line of config.

- `/thinking [level]` sets `default`, `low`, `medium` or `high`, and with no argument cycles; shift+tab
  cycles too, as in pi. `default` sends nothing at all, so a provider that has no such field is not
  sent one it would reject.
- It applies to this session. It is not written to `config.toml`: a change of mind while working is
  not necessarily a preference, and `/settings` is where a preference is saved.
- The list that cycles and the list `/thinking` accepts are the same constant, so the two cannot
  drift apart and leave a level that can be set but never reached by cycling.

**D58.** Images are attached to a prompt, and their type comes from their bytes.

- `@path` on an image attaches it as a picture rather than failing as text. The media type is
  decided by the magic bytes — PNG, JPEG, GIF, WebP — not by the extension, because a `.png` that is
  really a JPEG would otherwise be sent as a PNG and rejected by the provider. The pixel size is read
  out of the header the same way, so the transcript can say what it is.
- An attachment is an `Item::Attached { text, images }`, its own variant rather than a flag, so every
  place that matches on an item has to decide what a picture means there: the two renderers send it,
  compaction describes it in one line, the store previews it, `/tree` marks it. A flag would have
  been one `if` in the renderers and a silent omission everywhere else.
- The same carries through a **tool result**: `read` on a PNG hands the picture to the model, so
  looking at a screenshot is one call rather than a detour. `ToolResult` has `images`, and it is the
  renderers that deal with the two APIs disagreeing:
  - Anthropic accepts an image block inside `tool_result`, so it travels with the text. But it
    rejects an **error** result whose content is not text, so the images of a failed call are moved
    after the results in the same user message, captioned with the call id.
  - A Chat Completions `role: "tool"` message has no image field at all, so the picture travels in a
    user message right after it, headed `Image(s) from the tool result:`. The tool message gets
    `"(see the attached image)"` when the call produced no words, since the API requires content.
- An attachment to a *prompt* is still its own item variant rather than a flag, so every place that
  matches on an item has to decide what a picture means there.
- Anthropic gets `{"type":"image","source":{"type":"base64",…}}` beside the text block; OpenAI gets a
  second content part with a `data:` URL. Both are the wire shapes those APIs document.
- One image is limited to 5 MiB before base64, and the refusal says the size: a request the provider
  will reject is better refused where the message can be specific.
- Images are kept in the transcript as base64 in the row's JSON. That is larger than a file
  reference and needs no path to still exist later, which matters because the state directory is the
  only thing saddle owns.

**D59.** A picture is drawn where the terminal can, and described where it cannot.

- Two protocols are implemented: the Kitty graphics protocol and iTerm2's OSC 1337. Sixel is not:
  it needs the image quantised to a palette, which is a decoder, and the two implemented protocols
  cover the terminals people actually run.
- The protocol is detected once, at startup, from `[ui] images` if set and otherwise from the
  environment (`KITTY_WINDOW_ID`, `TERM`, `TERM_PROGRAM`). Detection is allowed to be wrong in one
  direction only: anything unrecognised gets the text fallback rather than an escape that would print
  garbage into the transcript.
- The picture is drawn **after** the frame, at the row its marker line was drawn on. Ratatui cannot
  express "the terminal draws this rectangle itself", so the layout reserves blank rows and the
  escapes are written to stdout afterwards. The marker line `[image shot.png 1280×720]` stays either
  way: it is the fallback, it is what a copy of the transcript contains, and it is what the model was
  told about.
- Each image carries an id so the terminal caches the data rather than receiving it every frame. A
  kitty image larger than 4096 base64 characters is continued across several escapes, which is what
  the protocol requires.
- **A large picture is retransmitted when its band changes**, because every placement is `a=T`,
  which transmits and displays in one action. The cost is the whole file again each time: a 45 KiB
  screenshot is 60 KB of base64 in 15 escapes and is unnoticeable, while a 4.5 MiB photograph is
  6.3 MB in 1549 escapes for every row it scrolls. Kitty's cheaper way is to transmit once and then
  re-place with `a=p,i=<id>` and a new source rectangle; that is the optimisation to reach for if
  large pictures feel slow over a remote connection, and it is not done yet because it depends on
  the terminal keeping the data under that id.
- **No cell box for kitty when the picture fits.** Without `c`/`r` kitty draws at the image's own
  pixel size, so one pixel of the file is one pixel of screen. The layout still reserves rows, but
  from the picture's height rather than a fixed number, and as a hint only: the terminal decides the
  real size when it paints. iTerm2 does take a cell size, with `preserveAspectRatio`, because its
  protocol has no pixel-size mode.
- The spec says what `c`/`r` do, and it is not what an earlier draft of this record claimed. Kitty:
  "The image will be scaled (enlarged/shrunk) as needed to fit the specified area … If only one of
  either `r` or `c` is specified, the other one is computed based on the source image aspect ratio …
  When both are specified the image is **letterboxed/pillarboxed** to prevent distortion"
  (`docs/graphics-protocol.rst`, *Controlling displayed image layout*). Kitty does not stretch. The
  64×64 icon smeared across eighty columns that made us drop the cell box was observed in WezTerm,
  which implements the protocol itself; whatever it does with a box, the box is still the wrong tool
  for a picture that already fits. What the letterboxing rule does mean is that the **assumed cell
  size is load-bearing**: a source band and the cell box it is placed in must have the same shape, or
  kitty leaves a margin and the picture looks like it has slipped against the text. `tui/image.rs`
  asserts that equality in a test rather than trusting the arithmetic.
- **A picture is erased before it is redrawn.** Kitty keeps an image at the cell it was drawn on,
  and ratatui only rewrites characters — it has no idea a picture is under them. So a scroll moved
  the text but not the image, and the next frame drew a second copy at the new position: smearing.
  Each frame now deletes the ids it drew last time (`a=d,d=i`) and draws again, which is cheap
  because the ids are stable per transcript entry. A frame where nothing moved sends nothing, so an
  ordinary keystroke does not flicker.
- A picture is drawn only inside the transcript area, because the terminal would happily paint it
  over the editor and the status line — the other half of the smearing. Its rows are reserved in the
  layout, so the boundary is decided there and not by the escape.
- **A picture is placed while any of its rows is on screen, not while its marker line is.** The
  marker is a caption, and the first version of this had the layout skip any picture whose marker was
  outside the view, which meant scrolling a picture's caption off the top made the picture vanish
  although nearly all of it was still visible — the crop worked downwards and not upwards. The
  geometry now lives in one function, `image::band`, which both the layout and the drawing pass call:
  the picture occupies the line after its marker and `rows` lines following, and the band is the
  intersection with the visible lines.
- **A picture is shown in part, like text.** The rule "the whole picture must fit, or it is not
  drawn" is what made a second image vanish: two 320×200 screenshots need 23 transcript rows and a
  real transcript has 16, so the one below the fold was never drawn even though it was in the model's
  context. Now the picture occupies its rows and the drawing pass shows the intersection with the
  window: kitty is sent the source band (`y` and `h` in pixels) for the rows that are on screen, and
  scrolls through the rest as it comes into view. iTerm2 has no source rectangle, so it is given the
  visible rows and shrinks the whole picture into them, which is honest rather than cropped.
- It *fits* by being scaled, not by being dropped. `c`/`r` are sent only when the picture is too big
  for the room it has, with the box computed from the aspect ratio. A picture that fits is drawn one
  pixel to one pixel; a 1920×1080
  screenshot is shrunk to the transcript instead of vanishing. Dropping it instead is what made two
  large images look like "only one renders": two 320×200 pictures need 23 transcript rows, and a
  real transcript has 16.
- Cell sizes are assumed (10×20 pixels) because no terminal reports them. That decides the layout and
  the scale factor, and the terminal decides the final pixels when it paints.
- `saddle check` reports which protocol was chosen and what decided it. A terminal that draws nothing
  and a terminal that was never asked look identical on screen, so the answer has to be somewhere a
  person can read it.
- Nothing here decodes pixels: the terminal scales the picture to the cell box. Saddle therefore
  needs no image library, which is the reason this is a feasible feature at all.

**D60.** A workspace that puts credentials in reach needs an allowance for the session id that will
use it.

- **No directory is declared invalid.** Saddle does not decide where a workspace may be. It reports
  what a workspace puts in reach and asks the person to allow the session, by id, that wants to use
  it. A rule about paths would be one more hardcoded guess about how someone organises a machine.
- Two directories are named, because they are where credentials live on every Unix: the home
  directory, and the config directory (`$XDG_CONFIG_HOME`, or `~/.config`). A workspace is reported
  when it **is** one of them or **contains** one — a root at `/`, `/home` or `~/src` (with
  `XDG_CONFIG_HOME` unset) contains them just as surely as home itself. A project inside home is not
  reported, because it contains neither.
- **The warning cannot be answered where it is printed.** There is no keypress, no `--force` and no
  prompt at the refusal: it names the session id and prints the exact command, and the allowance is
  written by a separate invocation — `saddle session allow <id> --workspace <path>` — run by a person
  in their own terminal. Anything that can be answered at the point of refusal is answered by reflex;
  an allowance costs a second command and a copy of an id, which is the whole mechanism.
- **The allowance is for one session id and one workspace.** `--session <id>` names a session, and
  the allowance records that id and workspace (canonical paths, so `/home/me/` is the same
  permission). It is not a property of the workspace: a different id is still asked, so nothing is
  allowed merely because some other session was. It outlives the process that used it, so it can be
  picked again later (D61), and `saddle session revoke` is how it is removed. A global `allow-home`
  flag was tried first and discarded — a permanent whitelist keyed to nothing stops being read, and
  it cannot be consulted at the point that matters anyway, since the session does not exist until
  the root has been chosen.
- **An id has to be findable**, which is why the refusal records the request before it exits.
  `saddle session pending` lists the ids, how long ago each asked, and the workspace, so a message
  that has scrolled away is recoverable. The request is a record, not a live process: a refused
  session exits. Picking an existing allowance is D61.
- The cost of saying no is nothing: the check runs before the provider is chosen and before a
  session is opened, so a refusal leaves no session, no audit log and no network call. `check` prints
  the same report and succeeds, because it is the review tool rather than the gate. `saddle tool`
  is gated too, since it runs as the agent (D17).
- Nothing the agent runs can grant itself an allowance, and that is load-bearing: the allowance
  directory is in the state directory, which no root may contain (D16), so `write` is refused by the
  policy and a child is refused by landlock. A child that runs a copy of `saddle` from inside its
  workspace fails at the first file it needs — `XDG_STATE_HOME` is not passed to children at all
  (D19), and `$HOME/.local/state` is outside the sandbox.
- `allow-sensitive = true` in the user's config skips the check entirely, for a machine where the
  home directory holds nothing secret — a disposable container, say. It is the user's file, outside
  every root and outside the workspace (D42), so nothing the agent runs can set it.
- The provider keys get the same treatment by construction rather than by asking: `auth.json` is in
  the state directory, so the `read` tool is refused by the policy before a file is opened and a
  sandboxed child is refused by landlock. `tests/cli.rs` asserts both layers, and that the key never
  appears in either refusal.
- Most tools keep working, because a few directories outside the workspace are read **and execute**
  for children by default: `~/.cargo` and `~/.rustup` for the toolchain, and `~/.config/git` for
  git's XDG config, since a confined `git commit` cannot run without it. This is the `[sandbox] read`
  list in `config.toml`, generated on first run from the directories that exist. Two consequences,
  and they are why the list stays short:
  - An entry is read by the *whole* child, and exclusions do not bind `exec` (D22). A directory in
    this list must be safe to read in its entirety; never add one that holds a token.
  - Only directories can be roots, so the classic `~/.gitconfig` cannot be granted this way. A
    confined git that needs it gets `GIT_CONFIG_GLOBAL` pointed inside the workspace.
- This is why the report exists at all: with `.config` or home as the root, exclusion rules stop
  helping, because a root cannot be narrowed from the inside (D20).

**D61.** An allowance can be picked instead of remembered, and an id is a secret.

- Copying a 16-character id between two terminals is the kind of friction that makes a person keep a
  text file of ids, which is worse than the problem. So an allowance **outlives the process** that
  used it, `saddle session list [filter]` shows them (id, age, workspace, filtered by substring on
  either), and `--session` accepts an id, a **unique prefix**, or a number from the list. An
  ambiguous prefix is an error naming the candidates rather than a guess, so a typo cannot silently
  select the wrong session.
- The refusal lists the ids that already cover the workspace, and where a person is at a terminal it
  *asks*: the answer is a number, an id, or a prefix. An empty answer stops, as the refusal would
  have. This is not answering the warning — nothing is approved here. Every id offered was allowed
  earlier by a person with `saddle session allow`, and the prompt only saves copying one. A picker is
  never shown when stdin is not a terminal, because reading a line there would eat a prompt.
- Because an allowance now survives the process, the id is a **bearer permission**, not a label: it
  has to be unguessable. Ids were `{millis:x}-{pid:x}`, which is a few thousand candidates from the
  clock alone; they are now 16 hex characters from `/dev/urandom` (and a `time…` fallback, marked as
  such, if there is no random source).
- An id can be reused, so it can no longer name the things that belong to one run. The audit log and
  the private temporary directory are named `{id}-{run}` with a fresh random run token each time:
  two processes sharing an id must not fight over one file, and a reused id must not append to a
  chain that a previous process closed.
- The cost is that an allowance accumulates until it is revoked, which is the opposite of what D60
  first did. That is the trade: a permission a person chose once and can see, pick and remove, in
  exchange for not being expired silently by a process exiting.
- **Withdrawing takes the same short forms as using.** `saddle session revoke` accepts a number from
  `session list`, an id, or a unique prefix, and says what it removed and which workspace it covered;
  a revocation must not be harder to type than the permission it withdraws. It drops the allowance
  and any pending request for that id. `session list` prints the revoke command under the list, so
  the way out is visible where the permissions are.

**D62.** A rule is an input; the enumerated list is the authority.

- A sandboxed command should not need saddle to know the name of every tool. The default is an
  `[[sandbox.include]]` rule: walk a base directory at startup, keep every directory that does not
  match an `except` pattern, and give landlock the **list**. A toolchain saddle has never heard of
  works, and a directory that was not there at startup is withheld rather than granted.
- **A directory that contains a withheld directory is not granted itself.** A landlock rule covers
  everything beneath its directory, so granting `~/.config` grants `~/.config/saddle` however the
  pattern reads, and `~/.config/gh` would be reachable again. Such a directory is *expanded*
  instead: its children are considered in its place. This is the rule that makes the pattern
  honest, and it is the one place a bug would be silent, so it has its own tests.
- **Files are enumerated too, and a withheld file expands its directory.** Expanding a directory to
  honour an exclusion would otherwise take its regular files with it — `~/.gitconfig` would stop
  being readable the moment anything else in home is excluded. Landlock accepts a rule on a regular
  file, so each file directly in an expanded directory gets its own rule. The converse matters just
  as much and was the first version's bug: only withheld *directories* were forcing their parent to
  expand, so withholding a file inside a granted directory was silently undone. `~/.cargo` is
  granted for the toolchain, and `cargo login` writes a crates.io token to
  `~/.cargo/credentials.toml` — withholding the file without expanding the directory left the token
  readable through the directory's rule. Two lists, therefore: everything withheld, and the subset
  of it that is a directory.
- The decision needs **two passes**: a directory is only known to contain an exclusion after the
  whole walk has been read. The first pass collects what is withheld, the second emits the rules.
  One pass granted `~/.config` before it had seen `~/.config/saddle`.
- `depth` bounds the **search**, not the grant: a rule on a directory is recursive, so a deeper
  walk only finds more exclusions. Two levels is the default, which covers `~/.cargo`,
  `~/.config/git` and `~/.config/jj` at that depth.
- **The list is what `check` prints**, under `exec reads`. A pattern that says "everything under
  home except these" is not something a person can review; the resolved set is, and it is the thing
  the kernel enforces. The audit log's session record still names the grants rather than this list,
  which is a gap worth closing if a rule ever expands to something surprising.
- The state and config directories are withheld whatever the pattern says: they are passed to the
  enumeration as `protected` and cannot be re-enabled by a rule, because no root may contain them
  (D16). A pattern that would name one is not an error, it is ignored — the user's `except` list is
  not a place to lose a security property.
- Defaults are chosen so an unconfigured machine works and its credentials do not: `base = "~"`,
  `depth = 3`, and `except` covering the usual credential stores — keys and secret managers
  (`.ssh`, `.gnupg`, `.pki`, `.password-store`, `.local/share/keyrings`, `.local/share/kwalletd`,
  `.config/op`, `.vault-token`); cloud and containers (`.aws`, `.azure`, `.config/gcloud`, `.kube`,
  `.docker`, `.config/containers`, `.terraform.d`, `.oci`, `.s3cfg`, `.boto`, `.config/rclone`,
  `.config/doctl`, `.config/hcloud`); forges and git credential helpers (`.config/gh`,
  `.config/glab-cli`, `.config/hub`, `.config/tea`, `.netrc`, `.git-credentials`,
  `.config/git/credentials`); package registries (`.npmrc`, `.yarnrc`, `.yarnrc.yml`, `.pypirc`,
  `.config/pip`, `.pip`, `.gem/credentials`, `.nuget`, `.m2/settings.xml`,
  `.gradle/gradle.properties`, `.composer`, `.config/composer`, `.hex`, `.cargo/credentials`,
  `.cargo/credentials.toml`); databases (`.pgpass`, `.my.cnf`, `.mylogin.cnf`); mail (`.msmtprc`,
  `.offlineimaprc`, `.fetchmailrc`, `.authinfo`); shell and repl history (`.bash_history`,
  `.zsh_history`, `.python_history`, `.node_repl_history`, `.psql_history`, `.mysql_history`),
  because a secret pasted on a command line is kept there; and browser and mail profiles
  (`.mozilla`, `.thunderbird`, `.config/google-chrome`, `.config/chromium`), because a session
  cookie is a credential.
- **A toolchain directory is not a root.** Roots are nameable: any policy in a workspace can write
  `fs-read` or `fs-write = [{ root = "cargo" }]`, and that is how a workspace could grant itself
  write on `~/.cargo` and plant a program in `~/.cargo/bin`, which runs outside the sandbox the
  next time it is called. `[sandbox] read`/`write` used to accept only root names, which is the only
  reason `cargo`, `cargo-registry` and `rustup` were ever roots; they now accept a path, so the
  default config declares no roots at all and the writable caches are named directly
  (`~/.cargo/registry`, `~/.cargo/git`). A policy naming `cargo` now fails to parse, which is the
  point.
- **Rules over one base are merged, not enumerated separately.** Two rules that both walk `~` would
  otherwise *union*: the built-in rule grants `~/.config/jj`, and a person's `except` for the same
  directory in a second rule would do nothing, because landlock grants when any rule does. The
  `except` lists of every rule sharing a base are combined and enumerated once, so adding a pattern
  narrows rather than adds.
- `depth` is three rather than two because the stores that matter are not all near the top:
  `.local/share/keyrings` is three levels down, and a walk that stops at `~/.local/share` never
  enumerates it, so the pattern matches nothing and the keyring is reachable through the granted
  `.local`. A pattern that matches nothing is silent in both directions, so a test builds a tree
  containing **every** path the defaults name, runs the walk, and asserts each one is unreachable —
  which also asserts that the toolchain paths stay reachable. A regex-only test passed while that
  bug was live.
- The list errs towards naming stores a machine may not have, because an unmatched pattern is
  harmless and a missing one is not. It is a **starting point**, not a boundary: a machine keeps
  tokens wherever it likes, and the only place a secret is truly out of reach is outside the base.
  `.gitconfig` is deliberately readable — git cannot run without it — and it can hold a
  credential helper or a token embedded in a URL, which is the argument for keeping a token in a
  helper rather than in the file.
- Not withheld, and why: `.bashrc`/`.zshrc`/`.profile` can export a key, but they are also what makes
  a shell usable, and a blocked one breaks commands in ways that look like bugs; `.config/Code` and
  other editor state hold a sync token but are large and often needed by extensions. Both are one
  line away for a person who wants them. **These are a starting point, not a
  security boundary** — a machine keeps its secrets in whatever directory it likes, and the honest
  place for a secret is outside every base. A person can add a pattern in one line, and that
  exclusion is real for `exec` as well, which is what D22 could not offer when grants were a fixed
  list of roots.
- An unreadable or missing base grants nothing rather than failing the session, so a config carried
  between machines does not break one that lacks the directory.

**D63.** One file for the base, a file per workspace for overrides, and a flag for one run.

- **`config.toml` is the only file that holds the base.** `policy.toml` and `config.toml` existed
  for a reason that no longer holds — provider settings and grants were written by different code —
  and two files whose grant semantics differ is a thing a person has to learn twice. `[agent]` in
  `config.toml` is the base every workspace inherits.
- **A workspace's own grants are their own file**, `policies/--<path>--.toml`, named the way pi
  named a session directory: the leading and trailing `--` wrap the encoded path and its separators
  become `-`, so `/home/me/source/saddle` is `--home-me-source-saddle--.toml` and a directory listing
  reads like a list of paths. `%` as the separator was tried first and was unreadable. The encoding
  is lossy (`a-b` and `a/b` collide) so the file records the path it is for, and `check` prints the
  one in force. They are *not* a table keyed by path
  inside the global file: that was the first attempt and it is wrong in two ways. The global file
  would accrue a key for every directory ever opened, growing without bound and mixing one
  directory's authority into a file named after nothing; and the authority for a directory belongs in
  a file a person can find, name and delete. The layering rule is unchanged (D43): a key the
  workspace lists replaces the base value for that key, and an absent key is inherited.
- **An absent `[agent]` is "not configured yet", and the built-in grants apply.** An `[agent]` table
  that is present and empty is a decision, and is taken as written. Without that, the first person
  to write a two-line `config.toml` would silently lose every grant.
- **`--grant` and `--revoke` change the grants for one run.** `--grant fs-read:scratch` adds a grant,
  `--grant exec` sets a scope that takes no parameters, `--grant fs-read:scratch:.env*` carries an
  exclusion, and `--revoke net` drops a scope. Nothing is written, so there is no file to review and
  no allowance to withdraw: the override belongs to the process, and the session intro says what
  changed so it is not invisible either.
- **An override is applied to the merged grants, not to the workspace's own.** A workspace-level
  edit can only shadow a key; `--revoke exec` has to remove a grant that came from the base, which
  means merging first and overriding second. The engine is then built from that document and not
  re-layered from the file, or `check` and the engine would describe different policies — that was
  the first version's bug.
- The hash in the audit record names the **grants**, not the file, so a provider change is not a
  policy change.
- **A grants file says which directory it governs.** Its first line is
  `workspace = "/home/me/project"`, written when saddle creates it. The name is only a readable
  encoding and is lossy — `~/a-b` and `~/a/b` both give `--home-me-a-b--` — so the name may not be
  trusted for this: a session **refuses** a grants file whose `workspace` names a different
  directory, describing both paths. Applying one directory's grants to another is how a `net *`
  written for one project would silently cover a second. A file with no line is treated as claiming
  nothing and is used as before.
- **The migration is temporary and deliberately small**, because there is one user and one
  workspace. `config::migrate` does two things: move `policy.toml`'s `[agent]` into `config.toml`
  (renaming the original `.imported`) before the defaults are written, and rename a grants file
  still carrying the old `%` separator to the `--` form **while stamping its `workspace` line in the
  same step** — the `%` name is the only one that decodes exactly, and doing the rename and the
  stamp as separate passes is what made the line arrive one run late. It is written to be deleted in
  the next version; an earlier draft tried to migrate states that only existed in intermediate
  commits, which was work for no user.

**D64.** A skill is instructions, never authority, and its body is loaded rather than advertised.

- **Skills are the Agent Skills format**, which is what a skill written for another harness already
  is: a directory holding `SKILL.md` with frontmatter, discovered by a walk of a skills directory.
  Saddle does not invent a format for this any more than it invents one for instruction files.
- **The prompt carries the name, the description and the directory; the body is a tool call.** A
  directory of ten skills costs a few lines of prompt instead of all of their text, and the model
  decides what to load from the description, which is why a skill without one is not loaded at all:
  the description *is* the routing signal.
- **Loading was the `skill` tool, not "read SKILL.md"** — superseded by D67, which removed the tool
  once the reason for it (a host read of a directory no root reaches) was gone. As it stood: one
  user tier is the config directory, which
  no root and no sandbox entry may reach (D16), so `read` on a skill there is denied while `bash`
  can reach it only because D62 enumerates `~` for children. Telling the model to read a file that
  the gate refuses would be a prompt that does not work. The other tier, `~/.agents`, became
  readable by the tools by default in D66, which makes `read` an alternative for a *bundled* file
  there — but the tool is still how a body is advertised and loaded by name, and it is the only way
  to load a skill from the config directory. The tool takes a **name** and serves the text discovery
  already admitted, so it cannot become a general file reader, and the directory it returns is how
  the model finds the bundled files: with `read` and `ls` inside the workspace, with `bash` outside.
- **`allowed-tools` is ignored, and the person is told so.** Under the specification it is an
  experimental pre-approval list; a pre-approval written by whoever wrote the skill is a widening of
  authority from a second source, which is the failure this project exists to prevent (§5). What a
  skill may do is what the policy allows. A hostile skill can therefore only ask for something the
  grants already permit, and every call it causes is in the transcript and the audit log.
- **Two tiers, and each is read the way its trust warrants.** The user tier (now `~/.agents/skills`
  alone; see D66, D67) is the user's own directory, read through the same gate as `read`. The
  project tier (every `.agents/skills` in the workspace, per D68) is
  written by whoever wrote the repository, so the file is read **through the policy as the agent**
  and a skill the grants refuse is refused and noted, never advertised. The walk that finds a
  `SKILL.md` is the host's and decides nothing, as the existence checks for instruction files do;
  what the policy decides is reading the file. More specific wins, as with instruction
  files: a project skill replaces a user one of the same name and the note says which it replaced.
- **A skill installed by linking it in is a skill.** The walk follows symlinks, unlike the file
  tools' walk: `~/.agents/skills/x -> ~/somewhere/x` is the usual way to share one, and discovery is
  a host read of the user's own directory rather than a path the model chose. Skipping links (as the
  first version did, by testing `file_type().is_dir()`, which does not follow) dropped a linked
  skill with no note at all — a silent absence, which is the worst shape for a bug. A visited
  directory is remembered through `canonicalize`, so a link that loops terminates, and the reported
  directory is the link's target so a skill's own references resolve.
- **Discovery happens once, at open**, like the instruction files: the conversation was built from
  what was there when it started. Both what was advertised and each body loaded are audit records
  (`kind: skill`, with byte count and hash), so a person can see what entered the context and when
  (D10). A skill is a name, not a path: names are held to the specification's alphabet, so there is
  nothing for a traversal to traverse.

**D65.** A tool's path is a location under a root; a sandbox list is not a grant.

- **Three sets, three jobs, and they are not interchangeable.** *Roots* (`workspace`, `tmp`,
  `[roots]`) are the namespace the file tools decide over: the model names a path, the host maps it
  to the root that contains it, and the grant on that root decides, with its exclusions. A path
  outside every root is refused — not denied by a rule, but unaddressable, and the record says
  `outside every root`. *`[sandbox] read`/`write`* and the include rules are what a **child
  process** reaches under landlock: enumerated at startup, one list per process, no per-call
  decision and no per-call record. *Host-read content* is what the harness fetches itself:
  the global instruction file (D32), an attachment, a URL through `fetch` (D39). A skill's body used
  to be here; D67 made it a `read`.
- **What places a need is who chooses the path.** The model chooses → a root and a grant, because
  the decision is a lookup over grants naming that root. The harness chooses → a host read, because
  there is no model-chosen path to decide. That is why a skill's body was served by a tool until
  D67, rather than by making `~/.agents` a root — and why the answer, once `read` could reach it,
  was to delete the tool.
- **Shared read-only content must not become a root.** A root is nameable by any policy in a
  workspace, so a root for `~/.agents` would let a repository's grants file write
  `fs-write = [{ root = "agents" }]` and rewrite the skills every workspace loads — authority over
  shared files handed to whatever was last cloned. The same reason keeps toolchain directories as
  paths in `[sandbox]` rather than roots (D62).
- **`[sandbox] read` must not widen a tool.** It is one allow-list for a whole process with no
  per-call decision, so wiring it into the file tools would make a directory readable with no
  grant, no exclusion and no record of each access — a second source of authority for a decision,
  which is the failure §5 exists to prevent. Verified as the boundary: `[sandbox] read = ["~/notes"]`
  puts `~/notes` in `exec reads` and `bash` reads it, while `read` still refuses it as outside
  every root. `tests/cli.rs` pins that.
- **`bash` reaches more than the tools, and the model is told.** The enumeration is `~` minus the
  withheld stores; the tools see the roots. So "the tool refused it" does not mean "it is
  unreachable", and a command is the way to read such a file. The system prompt says this in those
  words, and `check` prints the two sets under separate headings.
- **Not done, on purpose:** listing or serving a skill's bundled files from the `skill` tool, which
  would read inside the skill's directory without a grant (§8).

**D66.** A `[sandbox]` entry may name the tools that reach it; a directory can be readable by the
tools without being a root or part of the session.

- **The gap.** Three sets existed: roots (the tools' namespace), `[sandbox]` (a child's reach), and
  host-read content. A directory the user owns and wants the agent to read — `~/.agents`, a notes
  directory — fell between them: readable by `bash` through the enumeration, refused by `read` as
  outside every root. A root is the wrong answer, because a root is nameable by any workspace policy
  (D62, D65). So the category is a *fourth* thing, and it is declared where a root cannot be: in
  `[sandbox]`, which lives only in the user's `config.toml`.
- **`~/.agents` is a built-in default, read-only**: `read`, `ls`, `find` and `grep`, added unless
  `tools-defaults = false`. That directory is where skills are installed, so a skill's own
  `references/` and scripts have to be readable by the tools that read; it was the case the property
  exists for, and making every machine write the entry first would have been a default that is
  really a chore. Read-only because a skill is instructions and nothing needs to write it. An entry
  for the same directory **adds** to the default rather than replacing it: two claims over one
  directory are one claim, and the tools are a union, so writing `tools = ["write"]` there yields
  all five. Subtracting would need a second syntax for "none of these", which is what an absent
  entry already means.
- **"Absent" means on, for both defaulted flags.** `UserConfig.sandbox` is `#[serde(default)]`, so a
  config with no `[sandbox]` table got `SandboxConfig`'s *derived* `Default` — where both
  `include-defaults` and `tools-defaults` were `false`. A hand-written config therefore silently
  lost the built-in include rule (D62) as well as this default. The `Default` impl now matches the
  serde field defaults, so a missing table and an empty one mean the same thing.
- **The shape.** An entry is a path, as before, or a table adding `tools`:
  `{ path = "~/.agents", tools = ["read", "ls", "find", "grep"] }`. A bare path keeps its old
  meaning exactly — indirect reach only — so nothing that exists today widens. The tools are named
  individually rather than as a read/write level, because that is how the gate is asked: the tool
  name is the thing to check. `write` and `edit` in the list allow writing there.
- **No combination is validated.** Listing `edit` on a path only `read` reaches for a command is
  allowed and simply fails at the tool or at the kernel; `bash` in the list is accepted even though
  a command's reach comes from the entry itself, not from this list. Validating those pairs would
  encode an opinion about how a person intends to use a directory.
- **A tool name is validated, because there is no backstop behind it.** The file tools run in
  saddle, not in the sandbox: landlock confines the `exec` child, so a host-side `write` has nothing
  between it and the file but this list. A misspelt name would therefore read as a permission that
  silently does nothing, so it is refused when the config loads.
- **One entry, one claim, checked like a root.** An entry may not name, contain or sit inside the
  state or config directory (D16). This was missing for `[sandbox]` generally — `read =
  ["~/.config/saddle"]` handed a command the policy file — and it matters more now that an entry can
  grant tool access. A default include rule over `~` prunes these instead, because a pattern is not
  a claim.
- **A link inside a tool-visible directory is followed, and may land on another declared entry.**
  A skill is installed by linking it in from wherever it lives, so `~/.agents/skills/x ->
  ~/.pi/agent/skills/x` is the normal arrangement — and a link to an absolute path, which a root
  always refuses, is exactly what that is. Resolution is done by the host for these directories, and
  the target must lie inside *some* declared tool path: so a link may point at another directory the
  user listed, and never at something undeclared, or `~/.agents/x -> ~/.config/saddle/config.toml`
  would hand the tools the policy that governs them. The refusal names what to add, since the fix is
  one more entry rather than a looser rule. A missing file is resolved through its parent, so a
  write beneath a linked directory works, and the resolved location is what the tool then uses —
  returning the unresolved one made cap-std walk the link again and refuse it.
- **The reserved paths still hold** (D23). A named write bypasses the engine, so `gate` checks
  `Reserved` itself: a hook or `.git/config` under a tool-visible directory is refused whatever the
  entry lists, because it runs code as the user later.
- **Not a root, and not nameable.** Found by path prefix like a root, but held apart from `Roots`,
  so no policy can name one, `check` prints them under `tool paths` rather than `roots`, and the
  prompt says they are outside the workspace and not part of it. Internally each gets a generated
  name (`sandbox-1`) that skips any name a root uses, so a `Location` works unchanged and the name
  never leaks: the audit records the absolute path.
- **Decided without the engine.** A tool-visible directory is not covered by a grant, so there is no
  rule to look up; the decision is the config entry, recorded as an ordinary `kind: decision` record
  with the path, the outcome and an explanation naming the entry. The engine is unchanged, and no
  grant can produce this outcome — which is what keeps it one config file rather than a second
  authority a workspace could reach.

**D67.** A skill is a file, and `read` loads it; there is no `skill` tool.

- **One way to read a file.** The `skill` tool existed for one reason: the config-directory tier was
  reachable by no root, so a host call was the only way to serve a body. Two ways to read a file is
  a second authority with its own gate, its own audit record and its own chance to drift from
  `read` — and a tool that takes a *name* is a lookup table the model must be taught, when the
  prompt can simply name the file. So the tool is gone, the config-directory tier went with it
  (nothing can reach it, so advertising it would point at a denial), and the prompt lists each
  skill's `SKILL.md` as an **absolute path** — `read` does not expand `~`, and a relative path is
  taken from the workspace.
- **Discovery reads through the gate `read` uses.** That is what keeps the listing honest: a skill
  the grants refuse, or one whose link target is not declared (D66), is refused during discovery and
  never advertised. The alternative — a separate reachability check — would be a second opinion
  about the same decision, which is how a prompt and a tool come to disagree. Each discovery read is
  an ordinary `decision` record; the listing itself is the `skill` record.
- **The user tier is `~/.agents/skills` only.** That is a default tool path (D66) and one of the
  Agent Skills locations, so it is readable and discoverable without configuration. A skills
  directory somewhere else — pi raises its own under `~/.pi` — is reachable by linking it into
  `~/.agents/skills` and declaring the target as a tool path; a skill that is merely *present* under
  a declared directory is readable but not advertised, because discovery walks the two spec
  locations and nothing else.
- **What survives from D64**: the format, the frontmatter subset, `allowed-tools` being ignored and
  said so, the project tier being read through the policy, more specific winning over less, and the
  audit records. What goes is the tool and the host-read tier that justified it.

**D68.** Skill discovery follows pi's rules for a skills directory, and a project's skills are in
one place at its root.

- **One directory, not a tree scan.** A project's skills are `.agents/skills` at the workspace root
  — the shape pi has with `.pi/skills`. An earlier version walked the whole workspace looking for
  `.agents/skills` anywhere: no boundary, work proportional to directories that have nothing to do
  with skills, and a scan of the tree on every session open. A skill scoped to one crate is a
  directory in this one skills directory whose text says which crate it is for.
- **A skills directory is read the way pi reads one.** A directory holding `SKILL.md` is a skill and
  is **not** descended into, so a skill's own `references/` never becomes more skills. Otherwise its
  subdirectories are searched, dot entries and `node_modules` are skipped, and at the top level a
  bare `.md` file is a skill too. `MAX_DEPTH` bounds a tree of directories with no skill in it.
- **It honours the project's ignore files.** `.gitignore`, `.ignore` and `.fdignore`, through the
  matcher in `src/ignore.rs` that `tools/search` also uses: "what is ignored" has to mean one thing,
  or a file `grep` skips and discovery walks is a disagreement nobody can see.
- **Four conventions taken from pi, because being different has no upside here.** A name the
  specification rejects is a **warning and the skill still loads**, named as written — dropping a
  skill that can be read is the worse failure, and other implementations are stricter than this one.
  A declared name that differs from its directory is noted, since portable skills match. A name
  **collision keeps the first** discovered and reports both, so which body a name resolves to does
  not depend on how deep a directory sits. `disable-model-invocation: true` keeps a skill out of the
  prompt.
- **Metadata is escaped.** A description is written by whoever wrote the skill, and a project skill
  by whoever wrote the repository. Interpolated raw into the listing, a description containing
  `</skills>` ends the block and forges a `<skill>` entry — prompt injection into the system prompt,
  where the injected text outranks what surrounds it. Every interpolated field is escaped.
- **A refusal is said out loud.** `saddle tool` prints discovery's notes to stderr. Until it did, a
  skill refused by the policy or skipped by a rule vanished with no output at all, the same silence
  as "there are none".
- **`disable-model-invocation` is honoured**: the skill is kept out of the prompt and reachable
  through `/skill:name` (D69), which is what the flag is for.

**D69.** `/skill:name` loads a skill into the conversation, in the TUI and in line mode.

- **The command is the forcing function.** A model can miss a skill whose description applies, and
  a person who knows better needs a way to say so. `/skill:<name> <request>` puts the skill's
  instructions into the conversation followed by the rest of the line as the request, which is what
  pi does. It is also the only way to reach a skill marked `disable-model-invocation` — the flag
  hides it from the prompt, and a hidden skill nothing can invoke would just be a broken skill.
- **Served from what discovery already read.** The body is in the session, read through the gate and
  hashed at open, so the command needs no new access and no new authority: it moves text the session
  holds into the conversation. That is also why it can be expanded in the interface thread without
  asking the policy a second time.
- **The conversation holds what was sent.** The expanded text is the user message, so the transcript
  shows it and a resumed session rebuilds the same thing from the store. The alternative — the typed
  `/skill:x` line in the transcript and the body in the store — makes the transcript a different
  document from the conversation, which is worse than a long entry.
- **Both halves are in the audit.** Discovery records what was offered, with a hash per skill; a load
  records that the body went in front of the model, with the same hash, so the two can be joined. A
  prompt that says it came from a skill the session does not have is recorded rather than passed
  over.
- **The names are discoverable.** Skill commands are in the `/` menu beside the builtin ones, with
  the routing description as the line's explanation, and `/help` lists them for the session. A
  mistyped name answers with the names that do exist, because a silent no-op is the failure mode
  this project keeps having to remove.

**D70.** `/reload` reads again everything the prompt and the interface were built from.

- **What it reads again**: the instruction files (D32) and the skills (D67), both of which are baked
  into the base prompt at open; and the settings that live in `config.toml` — the status line, the
  request limit, the compaction settings, and the provider when the file names a different one.
  Those are what pi's `/reload` covers that this harness has.
- **Why it needs a command at all.** Discovery runs once at open, so a skill written mid-session was
  invisible until restart — and a skill is exactly the thing a person writes and then wants to try.
  `/reload` runs the same discovery again, through the same gate, so the rules do not change: every
  candidate is read as the agent, and every read is recorded.
- **The prompt is rebuilt, not patched.** The advertised skills are part of the base prompt, so the
  agent keeps the instruction files it was built from and rebuilds base and system from the session.
  A patch would be a second way for the prompt to be assembled; rebuilding keeps one. Grants are
  rebuilt with it, which is what D47 wants anyway.
- **A reload changes what is advertised, not what was read.** A body already expanded into the
  conversation stays there: it is a user message, and rewriting history to remove it would be a lie
  about what the model was sent. The note says so rather than leaving the person to wonder why the
  old text still seems to be in play.
- **A newly added instruction file is also handed to the conversation.** The prompt silently growing
  a rule means the next reply obeys something the history never mentions. An `AGENTS.md` that is new
  since the last read goes in as a user message, wrapped in `<instructions reloaded>`, so the
  transcript shows it arriving; one that was already there is not repeated.
- **The settings half keeps the session-only choices.** `/thinking` and ctrl+t/ctrl+o change the
  agent and are deliberately not written to `config.toml`, so a reload that re-applied the file
  wholesale would drop a choice made minutes before. Only what the file states is taken again.
- **The interface gets the list back.** The agent thread owns the session, so it sends the new skills
  to the interface, which replaces what the `/` menu and `/skill:` resolve against. Only there, and
  only on request: the queue, turns and the rest of the loop are untouched.
- **Both interfaces**, line mode included, because a piped prompt should not need a different
  spelling. `/reload` waits for the turn to end, as `/tree` and `/compact` do: the agent thread is
  mid-request, and discovery takes the session mutably.

**D71.** Colours are a theme: a table of roles in `config.toml`, and the renderer names no colour.

- **Every colour the interface draws comes from a role.** The renderer used to say `Color::Cyan`
  in fifty-eight places, which meant "change the accent" was a code change. A `Theme` holds eighteen
  roles — accent, muted, code, warn, error, success, link, text, the four diff and tool-block
  colours, the selection pair — and `render.rs` reaches them through the theme it is given. A role
  nobody paints with would be a setting that does nothing, so a test asserts every role in the list
  can be set.
- **Nothing configured changes nothing.** The built-in `default` is *exactly* what this binary drew
  before, so shipping the system cannot restyle anyone's screen. `light` and `mono` ship beside it:
  the ANSI palette is chosen for a dark terminal, and a person on a light one had no way to say so.
- **A theme is a table, and it starts from a base.** `[themes.<name>]` with `extends` naming a
  built-in or another table; the roles it leaves out come from the base. That is what makes a
  partial theme worth writing: three lines to restyle an accent, without owning the other fifteen
  roles forever. A cycle is refused by name rather than recursing.
- **Errors are errors.** An unknown theme or a misspelt role names what does exist, at session
  start, and the default is drawn meanwhile. A colour nobody chose is worse than a session that
  says the line is wrong.
- **Colours are written the way a person writes them**: an ANSI name, `#rrggbb`, or a palette index
  0–255 for a terminal without true colour.
- **`/theme` switches and saves**, like `/model`: a choice made once should not have to be made
  again. It builds the theme before writing it, so a name that does not work cannot be saved and
  blamed on the next session. `/reload` re-reads the file, so a hand edit applies without a restart.

**D72.** Theme parity with pi: its role names, its colour forms, its light/dark pairs.

- **The role names are pi's**, so a palette copied from a pi theme can be pasted in and understood:
  `accent`, `borderAccent`, `toolTitle`, `mdCodeBlockBorder`, `toolDiffContext`, `bashMode` and the
  rest. D71 shipped eighteen kebab-case roles of my own invention; those are accepted as aliases so
  a config written in the ten minutes they existed still loads, but the documented names are pi's.
- **Roles pi has for things this interface does not draw are refused by name.** Syntax highlighting,
  scrollbars, search matches, per-level thinking borders, hook-injected messages, HTML export — this
  harness has no element for any of them, and a role nothing paints with is a setting that silently
  does nothing. Setting one says exactly that, rather than "not a colour role", so a pasted pi theme
  explains itself instead of looking like a typo.
- **The colour forms are pi's**: an ANSI name, `#rgb` or `#rrggbb`, `oklch(62% 0.1 200)`,
  `okhsl(250 60% 55%)`, a palette index 0–255, a `vars` entry, or `""` for the terminal default.
  OKLCH and OKHSL are converted to sRGB here: OKHSL's saturation is relative to the most chroma the
  gamut holds at that hue and lightness, found by bisection, which is what makes equal saturation
  look equally colourful. No dependency was added for this; it is a page of arithmetic and it is
  tested against the anchors (white, black, a blue, a red, and the gamut edge).
- **`vars` are chained and a cycle is refused.** A var may name another var; a misspelt colour that
  is neither a colour nor a var says which.
- **`theme = "light/dark"` picks by terminal appearance**, pi's spelling: light first, dark second.
  The appearance comes from `COLORFGBG`, which is what a terminal exports to answer this; dark when
  it says nothing, as before. `--use-theme <name>` chooses for one run without writing anything.
- **Roles that describe a real element are wired, not just parsed.** `toolTitle` is the tool's name
  and `toolOutput` its body; `thinkingText` is a reasoning block; `userMessageText` the prompt;
  `mdHeading`/`mdListBullet`/`mdHr`/`mdQuoteBorder` the markdown pieces; `toolDiffContext` a diff's
  gutter; `borderAccent`, `borderMuted` and `bashMode` the three states of the editor's frame.
- **Not taken here: pi's `system` theme.** It queries the terminal's own palette and derives every
  role from it; that is D73.

**D73.** (The highlighter this introduced was replaced by D80.) Two more of pi's: the `system` theme, derived from the terminal, and syntax highlighting.

`theme = "system"` is a reserved name and not a palette. It asks the terminal what its colours are
and derives every role from the answer.

- **The query is OSC 10 for the foreground, OSC 11 for the background and OSC 4 for each of the
  sixteen ANSI colours**, written in one burst, then stdin read until the replies stop or 100 ms
  pass — pi's timeout. Only for a theme that needs it, only once, and only when stdin is a terminal:
  a pipe has nobody to answer. The reply is read in raw mode and raw mode is put back immediately,
  because the full-screen interface is about to take the terminal over; the answer is kept, so a
  theme rebuilt mid-session does not ask again.
- **The parsing is separated from the asking.** `terminal::parse` takes the bytes and is tested
  against the replies a terminal sends — `rgb:rrrr/gggg/bbbb`, `#rrggbb`, one-digit fields scaled not
  truncated, ANSI colours by index — and against a key press and a partial sequence in the middle of
  them. That seam is the whole reason this is testable here: a sandbox has no terminal to answer, so
  the query itself is only ever exercised by hand.
- **The background decides the appearance, and the appearance is then a fact about the terminal, not
  a guess.** Relative luminance above 0.35 is light. `COLORFGBG` is consulted only when the terminal
  was asked nothing, since it is a variable a terminal may not set at all. A `light/dark` pair (D72)
  therefore picks its half from the same answer.
- **Each role takes its hue from a matching ANSI colour** — errors from red, success from green,
  warnings from yellow, links from blue, code from magenta, accent from cyan — and falls back to the
  built-in palette's hue for a colour the terminal did not report. Lightness is then placed, not
  taken: `place` bisects towards white on a dark background or black on a light one until the
  contrast clears the floor. **4.5:1 for anything read as text**, 3:1 for `muted`, 2:1 for `dim`,
  which are faint on purpose and would be useless at 4.5. The search asks for 1.02× the floor so
  rounding to the eight bits a terminal draws cannot land a shade under it.
- **A derived role is an explicit RGB colour wherever one can be worked out.** A named ANSI colour is
  replaced by the value it stands for, so the palette does not depend on the terminal's own palette a
  second time — which is what makes the ratio it promises a fact rather than an intention.
- **Block backgrounds are mixed from the reported background** (a little lighter on dark, a little
  darker on light), so a panel is visible on a terminal nobody has seen before.
- **A terminal that answers nothing gets pi's fallback**: ANSI indices, the terminal's default
  colours, faint secondary text, and no panel backgrounds at all, because a background nobody
  reported cannot be contrasted against.
- **The terminal is asked once per run, at startup, and a light/dark switch is noticed at the next
  start.** This bullet first claimed `/theme` and `/reload` asked again before building; that was
  wrong to do, and it is what D75 exists to undo. pi listens for xterm's mode 2031 to notice a
  switch as it happens, which needs a claim on stdin that crossterm holds, and pi asks from inside
  its own event loop, where it can pause the reader first.

**Syntax highlighting** is a lexical highlighter, not a parser (`tui/syntax.rs`), because no
highlighter can be added offline and a code block is read, not compiled.

- **The fence's tag chooses the language**, by the spellings that appear in practice (`rs`, `ts`,
  `console`, `rust,ignore`). Rust, Python, JavaScript/TypeScript, shell, JSON, TOML, YAML, C and Go
  have their own keywords and comment markers.
- **An untagged fence is not highlighted**: one run in the block's own colour, which is what pi draws.
  A fence naming a language that is not known still gets the parts that read the same in any
  language — strings, numbers, comments — rather than nothing.
- **The highlighter carries two pieces of state between lines**: a block comment and a multi-line
  string can open on one line and close on another, so the rows of a block are walked in order and a
  block's state is reset when it ends.
- **It returns runs, in order, covering the whole line**, and each run patches the block's base
  colour rather than replacing it — so a role that is the terminal default still draws, and wrapping
  splits between runs instead of at the line.
- **Every colour is a `syntax*` role** from the palette, so a pi theme, a `config.toml` palette and
  the derived `system` palette all paint code the same way.
- **The parser that is not there is a decision, not a shortcut**: `syntaxKeyword` means "the
  language's keywords", not "what a grammar says this token is". A block with an unusual construct
  is highlighted plausibly and never wrongly breaks, which is the property a transcript needs.

**D74.** A theme is chosen from a list, by looking at it.

`/theme` with no argument used to print the theme's name and the names available, which meant knowing
the name before you could choose it, and choosing blind. It now opens a picker.

- **Every theme is in the list**: the built-ins and every `[themes.<name>]` table in `config.toml`,
  each saying where it comes from, so a theme is edited by opening the file that holds it. A table
  that shadows a built-in reports `config.toml`, since that is where it now lives.
- **Moving the highlight previews the theme on the whole screen**, box and all. That is the point:
  a palette is judged by looking at it, and no name in a list conveys a colour.
- **Escape puts back the theme that was in force**, in the drawing and in the name the status line
  shows. Both come from what the app had when the list opened, not from what the list was told it
  had: a picker that opened on the wrong entry must not be able to make escape change the theme.
  Enter is what makes the choice stick, by writing it to `config.toml` as `/theme <name>` always
  has; the list is a way to reach that, not a second kind of choice.
- **A theme that does not build stays in the list and cannot be chosen.** A broken palette is a
  mistake to be shown, in the place it was made, rather than hidden until startup when the cause is
  off screen. Its row carries the reason it failed.
- **The list opens on the theme in force**, so the first thing read is where you are. A `light/dark`
  pair (D72) has no row of its own: the half that applies is the entry to stand on.
- **The list scrolls and does not wrap.** A key held down stops at the end rather than jumping back
  to the top, which in a picker that previews would mean a flash of the wrong theme.
- **`/theme <name>` still switches directly**, and `--use-theme` still chooses for one run without
  writing anything: a name is a shortcut for a person who knows it, never the only way in.
- **Line mode has no screen to list them on**, so `/theme` there reports the theme in force and the
  names, and `/theme <name>` switches and saves as the interface does. That branch also stops
  `/theme` being sent to the model as a prompt, which it was: a command meant for the interface must
  never be billed as a question.

**D75.** The terminal is asked about its colours before the interface takes it over, and never after.

D73's query reads stdin and needs raw mode for the length of the wait. Doing that from inside the
running interface is not safe, and shipping it was a mistake worth a decision.

- **What it did.** `/theme` asked the terminal again before drawing the list. `ask` turned raw mode
  on, read the reply, and turned raw mode *off* — which is the shell's state, not the interface's.
  From then on every keystroke was echoed onto the screen and the interface received none of them:
  the prompt filled with what was typed, the list ignored every key, and the terminal had to be
  closed to get out. A late reply, arriving after the 100 ms deadline, was read as typing, which
  added more of the same.
- **The rule.** Exactly one caller asks: [`tui::theme_selected`], at startup, before `Screen::enter`.
  There is no `requery`, and nothing in the interface asks on a rebuild — `/theme`, `/reload` and a
  change of appearance all build from the answer taken at startup.
- **The check, not the promise.** `may_ask` refuses when stdin or stdout is not a terminal, and
  refuses when raw mode is already on, because that means the interface owns the terminal and the
  terminal does not belong to the query. A caller that gets this wrong now gets the ANSI fallback
  instead of a broken session — which is why the check is a function with its own test rather than a
  line of care at the call site.
- **Nothing left over is read as typing.** Whatever is still queued when the query ends is discarded
  (`tcflush`), and again when the interface starts, since a slow terminal answers after the deadline
  and those bytes would otherwise land in the prompt. The cost is at most a keystroke pressed in the
  first fraction of a second of a session.
- **Raw mode is watched, not assumed.** The event loop checks twice a second that the terminal is
  still in raw mode and puts it back if it is not. That does not excuse asking from the wrong place;
  it is there because losing raw mode is unrecoverable by the person in front of it — the only way
  out is closing the terminal, which loses the session.
- **Still true, and still the reason this cannot be verified here**: a reply has to come from a real
  terminal, so the query is exercised by hand. What the tests pin down is the decision that keeps it
  out of the interface, and the parsing of the replies it would get.

**D76.** Catppuccin is built in: four flavours, the palette's own colours, and one mapping.

- **The colours are the published ones**, read from Catppuccin's own table rather than from memory:
  26 per flavour, written as `#rrggbb` and parsed at compile time so the source can be compared with
  the table line for line. A flavour is recognised by its hex values, so approximating one defeats
  the point of shipping it.
- **The names are prefixed**: `catppuccin-latte`, `catppuccin-frappe`, `catppuccin-macchiato`,
  `catppuccin-mocha`. `mocha` alone would be ambiguous with any other theme that has one, and a name
  in `config.toml` should say what it is.
- **Latte wants a light terminal and the other three a dark one**, and no theme paints the terminal's
  background — the terminal supplies it, as in pi. So Latte on a dark terminal is dark ink on a dark
  background: the `/theme` picker says `light` beside it for exactly that reason (D74).
- **A mark is the published colour; text is the published colour only where it can be read.** This is
  the whole mapping, and the second half exists because of what Latte is: its accents are chosen as
  fills for a light surface, and as glyphs on its own `base` most of them sit near 2.5:1 — green is
  2.96:1, yellow 2.31:1, pink 2.34:1. Where an accent is used as text, it is moved towards the
  flavour's own `text` by the least amount that clears D73's floor. On the three dark flavours every
  accent already clears 4.5:1 by a wide margin, so nothing there is touched: the published colour is
  what the terminal draws. Moving towards `text` rather than towards black is what keeps the result a
  Catppuccin colour — it lands on the dark, desaturated version of the same hue, which is what a
  light theme wants for that role anyway.
- **Borders, bullets, rules, scrollbars and the reasoning levels keep the pastel.** Nothing reads them
  as a glyph, so fidelity costs nothing there, and it is what makes the flavour recognisable.
- **`mauve` is the identity colour** — accent, frame, headings, bullets, a tool's name — and the
  three greys the interface uses are three steps of the palette's own ramp.
- **A panel background is a colour Catppuccin has no name for.** On the dark flavours the surface
  greys are the right answer and are used as the palette intends. On Latte they sit too close to its
  own text ramp to carry text (`subtext1` on `surface1` is 3.44:1), so a panel there is a tint of
  `base` instead, and a pass/fail panel is a tint of `base` towards `green` or `red` on every flavour
  — 15% on a dark one and 12% on a light one, because the same tint takes more contrast away from
  light-mode text. A search match is a solid accent with `base` written on it on a dark flavour, and
  a pale tint with the flavour's ink on a light one: a mid-tone background reads on neither.
- **The floors are measured, not asserted.** Each flavour is tested for 4.5:1 on the roles that are
  read, 3:1 for secondary, 2:1 for the faintest, again on every panel background for the roles drawn
  there, and for a panel being distinguishable from the base it sits on. The numbers that drove the
  mapping are in this decision so the next person changing it knows what it was up against.
- **The table is kept whole.** Six of the 26 colours are used by no role yet; they are still
  recorded, because this is the palette and not a checklist, and a test reads all 26 — distinct
  values, and a ramp that runs one way — so a hex copied twice is caught.
- **A flavour is a starting point.** `[themes.catppuccin-mocha]` overrides the roles it names and
  keeps the rest, like any other built-in (D72).

**D77.** A session left alone is compacted once it is half full.

- **The rule is a function of the conversation and the clock**, `Agent::idle_compaction_due(idle_for)`,
  so it can be argued with and tested without waiting two minutes. It fires when: `auto` and `idle`
  are on, a provider is there, nothing else is compacting, `idle_for >= idle-seconds` (120s), the
  estimate is at least `idle-fraction` (0.5) of the window, and `cut_point` finds something to
  summarize. That last condition is why an idle session does not ask the model to summarize nothing.
- **Idle is measured from when the work finished, not when it was asked for.** A turn that took ten
  minutes has been busy for ten minutes; stamping the clock at the command's arrival would make the
  session "left alone" the instant it stopped. The worker loop is the only clock, so this is one
  line, and it is the line a reader should check.
- **One attempt per idle stretch.** The countdown restarts after an attempt, so a session that stays
  half full does not ask the model the same question every tick — and one that was compacted is
  below the threshold anyway.
- **The agent thread wakes to notice.** `work` used to block in `for command in commands`; it now
  waits with a 500 ms timeout and looks at the clock when nothing arrives. All agent state stays on
  one thread, which is the property the whole design rests on — nothing here introduces a second
  writer.
- **It costs a request**, which is the point: paying for it while nobody is waiting is better than
  paying for a full window when somebody comes back. It is a config switch (`idle = false`) and a
  `/settings` row, because spending money while a person is away should be something they chose.
- **A session that is not open cannot be idle.** This is the TUI's, and `saddle run` exits after its
  turn: line mode reads a line, answers it, and stops. A daemon that compacts saved sessions between
  runs is a different feature.
- **Neither pi nor opencode has this**, so there is no reference behaviour to match. Both features
  here are saddle's own, which is why each is specified rather than compared (D2 defines the feature
  inventory; it does not forbid an addition).

**D78.** The summary is written in the background while the turn carries on.

Above `background-fraction` (0.75) of the window the turn no longer stops to compact. It starts the
summary on another thread and keeps generating.

- **Silent until it lands.** The background request's reasoning and text are *not* streamed into the
  transcript: two voices writing at once is worse than a note. The user sees "compacting, the context
  passed three quarters" when it starts and the finished summary when it lands.
- **It is given a snapshot and nothing else** — a cloned provider, the serialized prefix, the
  workspace path. No `&Agent`, no session, no UI can be reached from that thread, so it cannot
  disturb the turn by construction rather than by discipline. Only text, a model and a usage come
  back, over a channel.
- **The identity rule, not a length rule.** The summary replaces the prefix only if the items being
  replaced are still exactly the items that were summarized (`with_summary`). A turn that appends
  leaves the prefix alone, so the summary still fits and whatever the turn added is kept. A
  conversation that was cleared, rewound, resumed or already compacted does not, and the summary is
  discarded. Length alone would have been a bug: a rewind can leave a conversation the same length
  with different text in it.
- **It lands only at a safe point** — the top of the turn loop between requests, the idle tick, and
  once when a turn ends. Nothing is spliced into the conversation while a request is being built
  from it.
- **One at a time, and the hard threshold waits for it.** A second background compaction is refused
  rather than queued. If the conversation reaches `window - reserve` while one is in flight, the turn
  waits for that summary instead of asking for the same work twice — bounded by the request's own
  timeouts (30s connect, 600s response), which is the same wait a synchronous compaction would have
  had.
- **It does not claim the activity line.** `Reason::blocks()` is false for a background compaction,
  so the interface never says "compacting" while the model is writing. That is what keeps it
  asynchronous from the user's point of view, not just from the code's.
- **A discarded summary is still paid for.** `record_spend` runs whether the summary is used or not:
  a cost display that hides a request it made is a cost display that lies.
- **The thread is not joined on quit.** Its result is dropped when the agent is, and a compaction
  writes nothing but the summary it hands back.
- **What is not verified here**: the request itself. It needs sockets, and the sandbox this is
  developed in has none, so the tests cover the decisions, the identity rule, the start-and-land
  bookkeeping against an injected job, and the type-level proof that a provider can cross threads.
  The end-to-end path is exercised by hand.

**D79.** A command gets its own network, with one door back to this process.

Before this, `exec` had no network at all (D19), which is why a build could not fetch a dependency
and why a test that starts a server and connects to it could not run. This supersedes the last
sentence of D28 and D19's no-network rule; both remain as the fallback described at the end.

- **Its own namespace, and no route out.** The helper enters a user namespace (unprivileged, so no
  privilege is needed) and then a network namespace, and raises `lo`. A socket in that namespace can
  reach `127.0.0.1` and nothing else, because there is nothing else to route to. That is what makes
  it safe to stop refusing `socket(2)` for `AF_INET` and `AF_INET6`.
- **One door.** A relay on the namespace's loopback carries each accepted connection to this process
  over a unix socket, saying one line first — `tls`, `http` or `proxy`, the dialect its port implies
  — and then copies bytes. It decides nothing and parses nothing, so a new protocol needs no new code
  there. The relay is forked before the seccomp filter applies, because `AF_UNIX` stays refused for
  the command itself: a network namespace does not confine `docker.sock`, `gpg-agent` or any other
  local daemon.
- **The decision stays here.** The proxy runs in threads but the engine and the audit log do not
  travel with it: a thread asks over a channel and the session thread answers while it waits for the
  command. One engine, one audit log, one policy — the property the third lowering exists to protect.
- **The name comes from the bytes, not from an address.** A TLS connection is decided on its server
  name extension, a plaintext one on its `Host` header or absolute URL, and a `CONNECT` on its own
  line. A connection that names no host is refused: a grant names a host, and an address is not a
  name (D28, D40). Encrypted Client Hello is refused too, because the outer name is a cover and
  matching it would prove nothing. A `CONNECT` tunnel is decided once and carried unchanged, since
  the handshake inside it can only reach the host that was allowed.
- **DNS is a redirect, not an authority.** Every name is answered with `127.0.0.1` and a zero TTL, so
  a client that ignores the proxy variables still lands on the relay and is decided there. The
  resolver addresses `resolv.conf` names are assigned to the namespace's own loopback, so the query
  arrives here and no file in the guest is rewritten — an unprivileged user may not bind-mount over
  `/etc/resolv.conf`, and rewriting a file the guest can read would put name resolution in a second
  place.
- **No new vocabulary.** The bridge is plumbing and carries no authority: `net` grants are the only
  authority, checked per request, exactly as `fetch` checks them (D39). There is no `exec`-level
  switch, so no policy can grant a network that bypasses the grants.
- **The command's own loopback is free.** It is its own, it cannot leave, and no grant is asked for.
  The *host's* loopback and the network behind it are unchanged: reachable only by a grant that names
  that address, so `*` still does not open them (D40).
- **The proxy variables are set where the spec is built**, so they cannot disagree with the port the
  relay listens on, and `no_proxy` names loopback so a guest reaches its own servers directly.
- **Failure is a fallback, never a widening.** If the namespace or the relay cannot start, the
  command runs exactly as it did before this decision: no network, `socket(2)` refused. `saddle`
  check` reports which of the two a command will get, and a resolver that cannot be answered is
  reported rather than silently dropped.
- **Still refused for the command:** `AF_UNIX`, `AF_PACKET`, the io_uring entry points, and `setns`,
  so a command cannot walk back into a namespace it was not given. `AF_NETLINK` is allowed only
  because glibc uses it to ask about interfaces, and inside this namespace there is nothing to learn
  but the loopback that was just raised.
- **Deferred:** a protocol whose first bytes carry no name (SSH, `git://`), which needs a pre-checked
  address or a shim; IPv6 resolver addresses; landlock's own TCP port rule (ABI 4+) as a second lock;
  and an opt-out for a workspace that wants a command with no network at all.
- **What is not verified here**: every kernel step above. The sandbox this is developed in has no
  sockets and no user namespaces of its own, so the tests cover the decisions, the parsers, the
  message a namespace is given, and the environment a command inherits; the rest is exercised by
  hand in a real session.

**D80.** The grammars are syntect's; this project decides only what a scope means.

D73's hand-written scanner was a stopgap: it was written because this build had no network and
`Cargo.lock` could not grow. It got lifetimes wrong (`'a` read as a character literal), raw strings
wrong (`r#"a "b" c"#` cut at the inner quote), nested generics wrong (`>>` as one token), and it knew
ten languages by hand. None of that is worth maintaining, so it is gone.

- **syntect parses, and only parses.** `two-face` supplies bat's syntax set — 213 grammars, where
  syntect's own defaults are 75 and have neither TypeScript nor TOML, the format this project writes
  its configuration in. The set is one embedded dump, decoded once.
- **A scope is the fact; the colour is the theme's.** The highlighter maps a scope stack to one of
  nine `Kind`s and nothing else: `Kind::style` reads `SyntaxColours` and is the only thing that
  produces a style. syntect can colour text itself, and using that would put a colour somewhere
  other than D71's theme, so its themes are never loaded.
- **The most specific scope that names a role wins**, and the roles that are properties of a
  character rather than of what encloses it — punctuation, operators — are used only when nothing
  enclosing names one. Without that order the `//` of a line comment would be painted as
  punctuation.
- **A raw string's prefix is a string.** `r#"…"#` carries a `storage.type` scope on its `r`, which
  would otherwise make it keyword-coloured inside a string.
- **Adjacent runs of one kind are joined.** A grammar pushes and pops scopes inside a string for its
  delimiters and escapes; one run per scope would draw one string as five and hand every caller a
  string that is not one string.
- **`storage.type` is a keyword.** The grammars scope `let`, `fn`, `struct`, `int`, `u32` and `str`
  the same way, and a Sublime theme paints them the same; a type this project defined is an
  `entity.name`, which is what gets the type role.
- **A language is its tag, not an enumeration here.** `Language` holds the tag, so a grammar this
  file has never heard of still works. A tag with no grammar is drawn in the block's own colour
  rather than guessed at, and a fence with no tag is `Plain`, which is what pi does.
- **A block's parse state is kept, not rebuilt.** A block comment or a triple-quoted string that
  opens on one line closes on a later one because the grammar's own state carries between lines, and
  a block that names a different language starts again.
- **Deferred:** loading a user's own `.sublime-syntax` files, and honouring a theme's own syntax
  colours. Neither is needed while every colour is a role.

**D81.** A prompt is read as markdown, in the user's own colour.

`*this*` in a message the user types is italic and `**this**` is bold, and — because it goes through
the same renderer a reply does — so is everything else markdown does: a fence in a prompt is a
highlighted code block, a `-` list is a list, a heading is a heading.

- **It is what pi does**, which is the first reason for it. pi's user-message component is its
  markdown renderer with `color: userMessageText` and `bgColor: userMessageBg`, and its parser is
  `marked` with a tokenizer override. The override was checked against the cases that matter and
  changes nothing here: with pi's own modules, `2*3*4` lexes as `em("3")` and
  `src/*.rs with tests/*.rs` as `em(".rs with tests/")`. `pulldown-cmark` answers identically, so a
  second, stricter rule written here would be a deviation from pi and not a match for it. An earlier
  version of this decision did exactly that — 85 lines of asterisk scanning — and it was removed.
- **The cost, stated rather than hidden.** Markdown eats an asterisk inside a word. A prompt of
  `2*3*4` is drawn `234` with the `3` italic, and `src/*.rs` italicises everything between it and
  the next asterisk. That is CommonMark, it is what pi shows, and it is the one way a prompt is not
  drawn exactly as it was typed. An unmatched `*` is a literal, so `src/*.rs` alone survives.
- **The colour is the base, not a wrapper.** The renderer carries a `base` style that every other
  style patches onto, so the prompt's text keeps `userMessageText`, inline code keeps `mdCode`, and a
  modifier is the only difference emphasis makes. The text is no longer bold, because bold everywhere
  hides `**bold**`.
- **One renderer, not two.** The marker is applied to the rows afterwards rather than by a second
  code path, so a prompt and a reply cannot drift apart.

**D82.** A web search is asked of the provider, and Exa is the fallback.

The `websearch` tool is the tenth built-in tool. It exists because knowing an address is not the same
as finding one: `fetch` gets a page whose URL is already known, and nothing in the tool set could
answer "what is the current version of X".

- **The provider searches, and finding that out took two wrong turns worth recording.** Synthetic's
  search is `POST https://api.synthetic.new/v2/search`, beside the `/v2/quotas` endpoint the status
  line already uses. It is *not* at `/v1/search` or `/v1/web_search`, which answer `404`, and the
  neighbouring `/v1/responses` and `/v1/tools` are absent too — so probing those routes is
  consistent with "there is no search" while being wrong. The tell is the status code on a route
  that does exist: `/v2/search` answers `401` without a key, and a path that does not exist answers
  `404`. A route that wants a credential is not a route that is missing. The second wrong turn was
  generalization from a 404: the right conclusion from `/v1/search → 404` is about `/v1/search`.
- **The provider's own key pays for its own search**, which is the point of doing it that way. A
  session that can talk to its model can search without a second credential, and `saddle login` fixes
  both at once. The URL is a provider setting — `search-url`, in `[providers.<name>]` or the preset —
  rather than a constant, so a compatible server that serves one is configured and not patched.
- **The request is the query and nothing else.** `POST {"query": "..."}` answers
  `{"results": [{url, title, text, published}]}`. Extra request fields are ignored rather than
  refused (`numResults` changes nothing: five results come back whatever is asked), so the tool
  sends the one field it knows and applies `num_results` to what arrived, saying so when the count
  asked for is larger than the count served. A body missing `query` is refused as
  `400 Invalid POST body: .query is missing`, and an empty query is a `500`.
- **The documented fields are read, and the undocumented ones are not load-bearing.** `text` is the
  result's own text and can be 400 KiB; `published` is a timestamp, present on some results and
  absent on others; there is no `author`. The endpoint also returns `highlights` — Exa's field,
  undocumented here — which is a prefix of the page rather than a passage chosen from it, so a
  parser that read only that would show titles with no content the day it went away. The excerpt is
  therefore the highlights when a backend sends non-empty ones, else `text`, and either way the
  model gets at most 1000 characters per passage, three passages per result, one line each, cut with
  an ellipsis. A date is shown as its date (`2025-11-05`) rather than as a timestamp. One parser
  reads both backends — `published` or Exa's `publishedDate`, `highlights` or `text` — so neither
  loses its dates. The 64 KiB ceiling stays as a backstop.
- **A large success is still a success.** The reply for five results is about 220 KiB, so a search
  that reused `fetch`'s 100 KiB limit would report a refusal for a working request. The POST path
  takes its own ceiling for this reason.
- **The gate is the same gate, and the grant is not free.** Each request goes through the engine as
  `Request::Net` before it is made, so the search host has to be granted: `api.synthetic.new` for a
  provider search, `api.exa.ai` or `mcp.exa.ai` for the fallback. A session granted the provider's
  completions but not its search is refused, which is deliberate — the model loop reaches its
  provider as part of the conversation, and this is a *tool* reaching the network, so it is decided
  like one. The audit records the method, the URL, the header names, the query and which backend
  answered; never a key (D48).
- **Exa is the fallback, in two forms.** With `EXA_API_KEY` (or a key stored with `/login exa`) the
  tool calls `POST https://api.exa.ai/search`. Without one, Exa's REST endpoint is not an option at
  all: it answers `402 X402_PAYMENT_REQUIRED`, a payment challenge rather than a rate limit, so
  "a few free searches" is not what an unkeyed call gets. The anonymous path is Exa's hosted MCP
  server at `mcp.exa.ai`, which answers a `web_search_exa` call today: one `initialize`, one
  `tools/call`, read from a JSON body or a server-sent event stream's `data:` lines. Nothing else of
  MCP is implemented and nothing is kept between calls, because the transport here is a single POST.
- **Nothing is searched by default without a grant**, and a provider that serves no search falls
  through to Exa rather than failing: `search-url` absent means the fallback is the whole story.
- **A result is read in two steps, and the second one costs nothing.** An excerpt answers most
  questions, but a 400 KiB page cannot be shown for each of five results, so `{"result": n}` shows
  result *n* of the search just made in full. The results are kept in the session, which is why the
  number still means the page the model was shown: asking again could answer with different pages
  under the same numbers, which is a silent substitution, not a read. Reading from what is held
  makes no request, needs no grant beyond the search's own, and records nothing — no authority is
  exercised, and the search that produced the data was already decided. A query given alongside
  `result` that does not match the held search is searched, and the answer says which search it came
  from. One consequence of the state being in memory: `saddle tool websearch '{"result": 2}'` in a
  fresh process has nothing to expand and says so, so a by-hand read passes the query too.
- **What "in full" means is bounded too.** An expansion carries at most the agent's own result limit
  (100 KiB), so a page that fits is not cut twice on the way into the context, and one that does not
  says how many bytes it has and where the page itself is — which is what `fetch` is for. An excerpt
  says nothing about the rest; an expansion states when it stopped.
- **Deferred:** Exa's `deep` search or research agent; a `[search]` section for a default result
  count and a preferred backend; and keeping a search across processes, which would need an artifact
  outside every root and buys nothing a second search does not.

**D83.** A table is fitted to the width it has, because a table that is wrapped is not a table.

Markdown tables were parsed into a grid and laid out at their natural widths, and the transcript
wrapped every row afterwards like prose. A table wider than the terminal therefore came out with its
rows broken across lines and its columns disjointed — the one construct whose meaning *is* its
alignment. It was reported against the parity tables in a reply, which is the ordinary case: a table
of three prose columns is wider than 80 columns by nature.

- **The room is passed in.** `markdown` takes the columns it may use, which is the transcript width
  the caller already knew; nothing else in the renderer changes, because every other block is still
  wrapped by `wrap` exactly as before.
- **The widths are water-filled, which is what pi does.** A column that fits inside an equal share
  keeps its natural width and takes no part in the squeeze, so the one prose column beside three
  short ones absorbs the deficit. An even split cuts the short word — `saddl` and `e` — and starves
  the column that had the most to give.
- **The floor is the longest word, not a constant.** A column is never narrowed below the widest
  word in it, so `Plugin` stays whole while the paragraph next to it gives up the room. This is pi's
  rule (`minWordWidths`) and the reason its tables stay legible when squeezed. Only when even the
  words do not fit does a column fall to one cell and a word get split.
- **A cell wraps inside its column and is padded to it**, so a tall row stays aligned. A word longer
  than the column, a URL in a narrow column, is split rather than pushing the row off the screen; the
  split drops the space at a line start, so a continuation is not silently indented.
- **Nothing moves when the table fits**: natural widths are kept, so a short table is not spread
  across the screen.
- **Deferred:** borders around cells (`│` between and a rule under the head only, as before), column
  alignment from the markdown (`:---:` is still ignored), and collapsing a table that cannot be made
  readable at the width it has into a list.

**D84.** A compatible server is a table, not a code path.

Two wire formats are understood — OpenAI's `/chat/completions` and Anthropic's `/v1/messages` — and
they are the two that other services imitate. Anything else is a `[providers.<name>]` table naming
which of the two it imitates, plus the fields that make the difference: a base URL, a model, the
environment variable holding its key, and now the headers and the key's own header name.

- **`api` chooses the format and nothing else does.** `openai` and `anthropic` are the two values;
  the preset of each name is the official endpoint, so `[providers.openai]` narrows or re-points a
  preset instead of replacing the notion of one.
- **`key-header` exists because the prefix follows the header, not the wire format.** A key in
  `authorization` is a bearer token; a key in a header the server named is sent raw, which is what
  Azure's `api-key` wants. Without it, an OpenAI-compatible gateway that authenticates with
  `api-key` could not be configured at all — and the failure would look like a wrong key.
- **`headers` is for what the wire format does not cover**: OpenRouter's `HTTP-Referer` and
  `X-Title`, a gateway's routing or account header. A header set here replaces the derived one of the
  same name rather than being sent twice, so `authorization` itself can be overridden without a
  second mechanism.
- **One function builds the headers for both formats** (`request_headers`). This is the part a
  third-party server actually depends on, and two copies would drift the moment one gained a case.
- **The Anthropic preset now exists.** It did not, and `anthropic` was used in a test as the example
  of an *unknown* provider — the same absence stated twice. Its base URL carries no `/v1`, because
  its client appends `/v1/messages`, which keeps a gateway's own prefix intact; OpenAI's base does
  carry `/v1`, because its client appends only `/chat/completions`. The asymmetry is the two
  projects' own convention and is left as it is.
- **Verified against a real server, not a mock.** A provider defined only in `config.toml`, pointing
  at a compatible endpoint with an extra header, completes a turn; moving the key to a header the
  server does not check makes that server answer `401 Authorization header required`, which is the
  proof the key went where the config said.
- **Deferred:** per-provider query parameters (Azure's `api-version`), a headers table that can
  reference an environment variable for a second secret, and `api = "openai-responses"`, which
  belongs with the OAuth work because the one endpoint that requires it is the one OAuth reaches.

**D85.** The built-in OpenAI provider authenticates with a key or with a ChatGPT login, and the
login reaches a different endpoint through a different wire format.

A ChatGPT subscription is not a setting on the API-key provider. Its token is spent at
`https://chatgpt.com/backend-api/codex` on the **Responses API**, whose conversation is a list of
typed items rather than role-tagged messages and whose stream is a sequence of named events rather
than deltas on a choice. That is a second client (`provider/responses.rs`), not a flag.

- **The login is the device code, and that was decided by which one could be verified.** The
  `/oauth/authorize` browser redirect answers `invalid_authorize_request` to every request that is
  not a real browser navigation — including the exact parameter set a first-party client sends —
  while `POST /api/accounts/deviceauth/usercode` answers a real `user_code` and `device_auth_id` to
  a plain POST. `saddle login openai-chatgpt` therefore prints a code to type at
  `https://auth.openai.com/codex/device`, polls
  `/api/accounts/deviceauth/token`, and exchanges the `authorization_code` and `code_verifier` it
  gets back at `/oauth/token`. The browser redirect is implemented and unit-tested, but it is not
  what the preset uses, because shipping an unverified path as the default is how a login breaks
  for everyone at once.
- **What the device flow cannot verify from here is the approval itself.** The probe reached a real
  code; nothing but a person with a browser can approve it, so the exchange after approval is
  written to the documented shape and unproven end to end.
- **An OAuth provider has no key.** `auth = "oauth"` changes what `missing_key` means: the
  credential is the stored access token, `available_key` returns nothing for it, and the token is
  refreshed before a request — never mid-stream, where a 401 cannot be recovered from. The tokens
  are shared behind one lock rather than copied, because a refresh rotates the refresh token and two
  concurrent refreshes would retire the one the other just took (a background compaction is a second
  `Provider`).
- **`auth.json` grew an `oauth` object beside the key, and old files still load.** A credential is
  either a key or tokens; neither shape writes the other's fields, so a file from before this change
  is unchanged on disk and a key typed today does not grow an empty token object.
- **The account is a claim in the token.** `chatgpt-account-id` comes from the access token's
  `https://api.openai.com/auth` claim, not from the login, so it survives a refresh.
- **Everything the login needs is a table**, like a provider: `[providers.<name>.oauth]` names the
  URLs, client id, scopes and redirect. `openai-chatgpt` is a preset filling it in, and an empty
  table is *incomplete* rather than silently OpenAI's — a provider that says `auth = "oauth"` and
  nothing else must not log in to another service.
- **The served catalogue is asked for, not shipped.** `GET /backend-api/codex/models` returns
  `{"models": [{"slug": …}]}` — the list is the service's to change, so a copy of it in this tree
  would be a guess that goes stale and silently keeps a model the backend has retired. The request
  is the same one a turn makes, with `?client_version=<semver>` added.
- **That version is the one claim the client makes about itself, and the list is gated on it.** The
  backend does not reject an old `client_version`: it answers a *shorter* list, or `{"models": []}`
  with no error at all, and the required-field validation error it gives when the parameter is
  missing is what `/login` was showing as a fragment of JSON. So the constant is a Codex CLI
  version, checked against the published releases, and an empty answer becomes the message that says
  which version was asked with — otherwise the picker is simply blank with nothing to explain it.
  This is the distinction that matters: a version is a fact about the client and is honest to pin; a
  list of models is a fact about the service and is not this tree's to assert.
- **There are two redirect addresses, and a device flow must exchange with its own.**
  `redirect-uri` is where a browser flow's code comes back (`http://localhost:1455/auth/callback`);
  `device-redirect-uri` is the address the server issued a device code for
  (`https://auth.openai.com/deviceauth/callback`). The token endpoint checks the code against the
  address it was issued for, so sending the loopback one after a device approval answers
  `token_exchange_user_error` — on the last step, after the person has typed the code, with nothing
  left to retry. A table that says `method = "device"` and names no device redirect is therefore
  *incomplete*, which is refused before the code is requested rather than after it is approved.
- **An error body is read for what it says.** OAuth's own shape is
  `{"error": "invalid_grant", "error_description": "…"}`; OpenAI's is
  `{"error": {"message": "…", "code": "token_exchange_user_error"}}`. Reading only the flat one
  turned a refusal into a pasted JSON fragment, so all three shapes are read and a code is shown
  beside the message.
- **`/login` is the flow, in the dialog, and `saddle login <provider>` is the same flow in a
  terminal.** A provider chosen in `/login` that authenticates with OAuth has no key field: picking
  it shows the address and the code, and the poll runs on its own thread while the screen stays
  live. The dialog never touches the network, so it keeps drawing; the thread says what it found
  through the same channel a model listing answers on, and every answer carries the attempt's id, so
  an abandoned sign-in's arrival is ignored rather than taken for the current one. Esc cancels,
  which is a flag the poll reads between requests — one already in flight finishes first — and the
  list is one key back, because a retry is a new attempt with a new id. One sign-in runs at a time:
  starting another raises the first one's flag, since the dialog has one on screen.
  Opening the dialog straight at a provider has no keystroke to answer to, so the choice leaves what
  it owes in `pending` and the host takes it once: a sign-in to start, or — for a provider already
  signed in — the model list to ask for, which is the same thing `/login` does for a provider whose
  key is already stored. A failure stays on the sign-in screen with the reason where the code was,
  so what failed and where to go from there are one screen apart.
- **Approval goes straight to the model list**, and the listing is sent with the token just stored
  rather than with a key that does not exist. That is why `list_models` and `ask_endpoint` take the
  `Provider` rather than a `(config, key)` pair: a listing has to send exactly what a turn will,
  including an OAuth token and its account header, or `/login` would report a 401 for a credential
  it has just obtained. A backend that lists nothing leaves the person typing the model name, which
  the dialog already knew how to do.
- **Deferred:** an OAuth provider other than OpenAI in the presets; refresh on a 401 (the token is
  refreshed before the request, and a refused one is reported rather than retried); the browser
  redirect for OpenAI, which needs a real navigation; and a logout inside the dialog (`/logout`
  already forgets a provider).

**D86.** The sign-in link can be opened and copied from inside the dialog, because in a terminal
neither comes for free.

A dialog in this UI draws text into cells. Nothing it draws is selectable: the mouse is captured so
drags reach saddle, and saddle's selection covers the transcript, not an overlay. So the two things
a device sign-in needs — an address to visit and a code to paste there — were on the screen and
unusable, which is what "I can't click the link and I can't select it" was.

- **The code goes on the clipboard the moment it arrives.** It is short, it is meaningless outside
  this flow, and it exists to be pasted, so copying it is what the person would do next. It is
  OSC 52, the same write the transcript's own copy uses (D35), so a terminal that refuses it leaves
  the code on screen and `c` as the fallback. The confirmation names what was copied —
  `copied the sign-in code` — rather than the selection's wording.
- **The link is underlined and is a click target.** The renderer records the cells the address was
  drawn in and the click is hit-tested against them, the way the jump badge already is (D35): what
  is clickable is what looks clickable, and a click anywhere else still does nothing. Only the part
  that fits is a target, since the rest was never drawn.
- **`o` opens the link and `c` copies it**, so the same two things work without a mouse, over ssh,
  and where the terminal does not report clicks at all.
- **Opening it launches the person's own browser**: `$BROWSER` (with its flags) if set, else
  `xdg-open`, `gio open`, `sensible-browser`, `open`, or the Windows handler. The address is one
  argument and there is no shell, so nothing in a URL is read as syntax; only `http`/`https` is
  opened, because anything else is not an address a sign-in put there.
- **A click is a press and a release in the same place.** Deciding on the press meant a drag that
  *started* on the link opened the browser instead of selecting it, so the link was the one piece of
  text in a dialog that could not be selected by dragging it. The press starts a selection like any
  other; the release decides: no movement means a click on the link, and follows it, while movement
  is a selection of the link like any other text (D87).
- **The reason a click failed goes into the dialog, not the transcript.** A note is a transcript
  entry, and a dialog covers the transcript, so the one message a person needs was pushed where they
  could not see it. A report from the opener is a dialog error while a dialog is open, and an
  ordinary note otherwise.
- **A process with no display says so instead of trying.** Where `DISPLAY` and `WAYLAND_DISPLAY` are
  both unset — a terminal on another user's session, a service, an ssh login — every opener fails
  and the best of them fails silently. That is diagnosed before a program is started, because it is
  the one failure no program will explain.
- **A program that starts and then fails is reported, not swallowed.** `xdg-open` with no desktop
  handler behind it starts, exits 3 and draws nothing, so a click looked like it did nothing at all.
  The child's exit status and its first line of stderr are watched on a thread — not waited for, a
  browser is not saddle's to outlive — and a non-zero exit becomes a note that names the program and
  points at `c`. If no program can even be started, the note says that instead.
- **This is a person's action and grants the agent nothing.** No tool produces `OpenUrl`; only a
  key or a click in that dialog does, and the address is one the person's own `config.toml` named,
  which no tool can write (D42). It is the same distinction `/permissions` rests on (D45, D46): a
  person acting on their own interface is not a capability, and nothing the model says can reach
  it.
- **Deferred:** making a dialog's text selectable at all (which needs a selection model for
  overlays, and the keys above cover what it was wanted for); opening the link from `saddle login`
  in a terminal, where ordinary terminal selection already works.

**D87.** A dialog's text is selected and copied like the transcript's, because a dialog owns the
screen and everything in it was otherwise dead text.

D86 made this visible by accident: the sign-in address could not be selected, so it could not be
copied, so the fix had to be a click target and a key. That was a patch over a hole — every dialog
in the UI had the same hole. `/settings`, `/permissions`, `/theme`, `/resume` and the tree all draw
text worth taking away: a model name, a policy path, the rule that is denying something.

- **One selection, and one surface.** `App::surface` is the open dialog's text if there is one, else
  the transcript; the drag, the mapping from a cell to a character, and the copy all go through it.
  So a dialog needs no selection code of its own, and a new dialog gets this by being drawn through
  the same `modal`.
- **What was drawn is what is selected.** A modal reports the lines it put in its box, cut to the
  cells they occupy: a dialog clips at its border, so the part that never appeared is not
  selectable. A drag that starts on the frame selects the line it is beside, because the frame is
  not text.
- **A drag past the edge stops at the edge.** The transcript scrolls when a drag is pulled out of
  it, which is how a selection is made longer than the screen. A dialog does not scroll, so there is
  nothing for the same gesture to mean: the selection goes to the last line drawn and stays there,
  rather than reaching into the transcript underneath, which the mouse was not over.
- **The highlight is painted on the buffer.** The transcript's selection is shaded by splitting
  spans before drawing, which a modal cannot do: a dozen functions build their own lines, and the
  box is drawn after them. So the selected cells are patched in place after the frame is composed.
  Same appearance, one implementation, and it works for every dialog without touching any of them.
- **Closing a dialog drops the selection.** Its line numbers mean nothing in the transcript, so
  keeping one would highlight unrelated text and copy the wrong thing.
- **D86's click target stays, and the link is selectable too.** A link is the one thing in a dialog
  where the gesture is "follow this" rather than "take this", so it is hit-tested — but on the
  release, not the press, which is what lets a drag that starts on the link select it like any other
  text (D86). The two do not fight: a click follows the link, a drag selects it.
- **Deferred:** keyboard-only selection inside a dialog (no anchor is set without a mouse); the `/`
  menu, which is a popup over the editor rather than a dialog and is filtered rather than read.

**D88.** `/model` with no name is a searchable list of what the provider serves, not a prompt
asking for a name the person may not know.

`/model <name>` still works and skips the dialog, because someone who knows the name should not wait
for a listing to come back. But the name is often not knowable: a subscription backend renames its
models, a gateway decides its own, and a listing is the only authority on what a request may ask for.

- **It is the list `/login` already ends on**, opened for one provider. No second picker and no
  second filter: the dialog gained a *purpose* — sign in, or choose a model — and the purpose
  decides what the choice means (list this provider's models), what esc does (close, because a
  picker has nothing behind it to step back into), and what the box is titled.
- **A picker asks for nothing already known.** The key the provider has is used, the address is the
  provider's own, and nothing is offered to type but the filter — so `Draft.key` stays empty and the
  listing is sent with the stored credential, which for an OAuth provider means the token and its
  account header (D85).
- **The model in force is named in the title** and is where the highlight starts, so the list is read
  against something and enter without moving keeps what is there rather than picking the first
  alphabetically.
- **A name that is not served can still be typed**, exactly as before: the filter line is the way to
  a model the server lists nothing about, which is what a listing that comes back empty leaves.
- **`/settings` → Model is the same dialog.** It was opened through the login path and could land on
  a key field the provider did not need; the one picker serves both, so there is one behaviour to
  describe and to test.

**D89.** `/model` lists what *every* configured provider serves, and choosing an entry switches both
the provider and the model.

D88 listed one provider's models — the one in force. That is the wrong question for a command whose
job is "what can I switch to": on a machine with an API key and a subscription, the two serve
different models under different names, and the identifier alone is not enough to pick one.

- **An entry is a model *and* the provider that serves it.** Two of them may serve a model with the
  same id, and a choice that named only the id would be ambiguous about which account it is spent
  from. That is why the list is `Pick { provider, model }`, and why choosing one writes both
  `provider` and `model` — which is exactly what `/login` already did.
- **The requests run concurrently, one thread per provider**, and the dialog waits for the answer as
  one list. Sequentially, a provider that is down would spend its whole timeout before the next was
  asked, and the picker would open after the sum of every failure.
- **A provider with no credential is not asked.** It is not a failure; it is one that has not been
  set up, and `/login` is where that happens. Asking anyway would put a 401 in the list's note for
  something the person already knows.
- **A partial list says what is missing from it.** "Missing" and "serves nothing" look identical in a
  union, so the providers that failed are named in a note above the list. Nothing at all, with
  failures, is an error that names them; nothing at all, with no failures, is the empty list the
  dialog already words.
- **The filter matches the provider as well as the model**, so typing `openai` narrows the union to
  one provider, and `provider/model` matches the pair. A name typed that nothing lists goes to the
  provider in force — the one a person is switching when they type a name at all — and the models
  column is padded from the whole list so it does not jump about while typing.
- **Every listing is kept for the window cache**, not only the chosen one: each server's answer is
  its own word on its own limits (D34). Recorded once, after every thread has answered, because one
  file with several writers is a race.
- **`/login` still lists one provider** — the one just logged into — because that credential was
  just entered for it. The union is for switching, not for setting up.

**D90.** `/new` starts a new session in the workspace this one is open on, and `/clear` is the same
action under the name it had before.

The command was missing rather than the machinery: `/clear` already dropped the transcript, and the
next prompt already opened a new saved conversation with the last one left in the store — which is
what pi's `/new` does, and the name a person coming from pi reaches for. One action, two names, and
the menu lists both so neither is a command that has to be remembered.

- **A new session has spent nothing and carries nothing.** The estimate of what the next request
  would carry (`context_tokens`, `measured`), the dollars the status line shows for the
  conversation, and the totals `/session` reports all described the history that has just been
  dropped, so they start again with the next one. Left standing, the estimate claimed a context that
  no longer existed, and the next turn would have been compacted for text that was no longer there.
- **A compaction running in the background of the old conversation is dropped**, not awaited and not
  applied: it was written from a prefix that is gone, and the check that discards a summary whose
  conversation changed would only be relied on to catch it later (D78).
- **The session id and the audit log carry on.** They belong to the *run*, not to the conversation:
  the workspace, the policy and the roots were decided when the process opened, and a command typed
  at the prompt cannot decide them again. What `/new` resets is the conversation — the thing
  `/resume` lists and the audit log records as a turn's subject.
- **The note names the workspace**, because that is the question a person is asking when they type
  it: which directory this fresh session is in, and that the last one is still there to resume.


**D91.** The model a conversation is working in belongs to that conversation, and the default in
`config.toml` is only what a new one starts with.

`/model` — the name form and a chosen entry alike — wrote the model into `config.toml`, and `/login`
wrote both the provider and the model. That is a choice about the conversation being worked in, so it
silently moved every later session as well, and a conversation resumed after the default had been
changed came back on a model it had never used.

- **`/model` changes this conversation.** The name form and enter in the picker both switch the
  running session and write nothing to the file. The choice is recorded with the conversation, as
  the `provider`/`model` of its row in the store, and the audit log notes the switch as it always
  did. A conversation that has not been saved yet records it at its first prompt, which is the row
  there is.
- **ctrl+s in the picker is the default.** It chooses the same entry enter would — the code decides
  the entry once, so the two keys can never disagree — and writes both the provider and the model
  to `config.toml`, which is what a new session reads. This is pi's split: `enter` says
  `Model: <id>`, ctrl+s says `Default model: <provider>/<id>`.
- **`/login` does not write the default either.** It stores the credential, which is the thing it
  exists for, and switches this conversation to the provider and model just chosen. The only entry
  it writes to `config.toml` is the `[providers.<name>]` table a server added there needs in order
  to exist at all — its `api` and `base-url`, not a `model` and not `provider =`. pi persists the
  model there, and this departs from it: two routes writing a default is how it stops being
  obvious which one did.
- **That leaves ctrl+s as the only writer**, which is worth stating plainly: a machine that has never
  pressed it has no provider in force, so every session opens `/login` until someone does, or until
  `provider =` and `model` are edited into the file by hand. One writer is the point — a second route
  is how it stops being obvious which one set the default — and both `/login` and `/model` say in
  their note where it is set, rather than leaving the person to find out that the choice they just
  made was temporary.
- **A resumed conversation comes back on its own model.** `Resumed` carries the row's
  `provider`/`model`, and the interface switches to it when it differs from what is in force —
  silently, because the resume note has already said what came back. A model that is no longer
  configured is an error note that names it and keeps the current one, rather than a conversation
  quietly moving to a model nobody chose.
- **Rewinding, forking and cloning carry it too**, because they read the same row; a fork copies the
  row as it copies the branch (D53).
- **A running session is not moved by a config change at all.** `/reload` reads the settings again
  (D70) and a hand edit of `config.toml` is picked up by the dialog that asked for it, but neither
  switches provider or model: a file that now names another provider is the next session's default,
  said out loud once, and the session keeps what it is using. The provider's own table — effort,
  limits, the address — is still re-read, with the session's model handed back in over the file's.
  Two sessions in the same directory therefore keep their own models, whatever either of them saves
  as the default.
- **`/settings` opens the same picker**, so its model row is a session choice as well; the default is
  one key away inside it, and the list says so.


**D92.** `session_query` retrieves the current branch without granting access to the session store.

- It requires `transcript-read`; every call is decided and audited with its conversation id and SQL.
  No filesystem grant implies transcript authority. First-run defaults and the fallback for a config
  with no `[agent]` table include `transcript-read`, so searching one's own conversation works out
  of the box. Existing explicit `[agent]` tables are never widened or rewritten: omitting the grant
  there still denies access, and `--revoke transcript-read` disables it for one run.
- The host exports only the active conversation's branch, including original pre-compaction messages,
  into a fresh in-memory SQLite snapshot. SQL never runs on the saved database connection. Abandoned
  branches, other conversations, state paths, native provider blocks and image bytes are not
  exported; what each request on the branch used is, as a second view (D94). Tool-result diffs are
  omitted as they are elsewhere in model context.
- The sole public view is `messages(ordinal INTEGER, id INTEGER, role TEXT, text TEXT)`. Roles are
  `user`, `assistant`, `tool`, `summary`; ordinal is one-based branch order. Replies include text
  and serialized tool calls; tool results include call ids and content. A clone's copied history
  belongs to its branch too. A missing saved conversation yields an ordinary tool error.
- Only read-only queries are accepted. SQLite's authorizer denies writes, schema inspection,
  attachment, pragmas, direct backing-table access and functions outside an explicit safe list.
  `query_only` and statement read-only checks add defense in depth; neither is the sole boundary.
- SQL is capped at 16 KiB, results at 100 rows and 50 KiB, individual SQLite values at 1 MiB and
  execution at about one million VM instructions. Temporary tables stay in memory. Output uses
  column names and row arrays, with a truncation flag; callers page or narrow their query.
- No transcript mutation capability is provided. A successful search cannot edit, delete or replace
  a saved message, nor can it expose the database directory as a filesystem resource.

**D93.** OpenAI API cost estimates and ChatGPT subscription allowances are different figures.

- The official API's `/models` catalogue does not publish prices. After config and endpoint prices,
  the official `https://api.openai.com/v1` API-key endpoint may fall back to published standard token
  rates, with cached-input rates. Matching is exact or a dated snapshot, never a substring that
  would mistake a pro/mini variant for its parent. Unknown and tiered long-context models stay
  unpriced rather than displaying a wrong charge. The source is OpenAI's pricing documentation,
  recorded alongside the table. Compatible endpoints do not inherit these prices.
- A ChatGPT login is a subscription, not an API bill. No published API-rate estimate is presented
  as money charged to that subscription. Config/endpoint prices retain their existing precedence.
- `openai-chatgpt` defaults its usage URL to `https://chatgpt.com/backend-api/wham/usage`, the GET
  used by OpenAI's Codex client. Usage requests refresh OAuth and carry the same account and provider
  headers as model requests. No credentials are read by tools or passed to children.
- The usage page's `rate_limit.primary_window` and `secondary_window` carry `used_percent`,
  `limit_window_seconds` and `reset_at` (Unix seconds). Windows are independent: the status shows
  remaining percentages labelled `5h` and `weekly` when those are the actual durations, each with
  its own reset countdown. `saddle check` also prints the absolute UTC reset stamps. Missing windows
  stay absent; a zero remaining allowance is real, not a missing value. Additional unrelated pools
  are not confused with the main limits. Model response `x-codex-*` headers update the same windows.
- The existing off-thread usage lookup at startup and after turns serves both providers. API-key
  OpenAI keeps ordinary request/token rate-limit headers; they are not subscription allowances.

**D94.** Every model request is recorded with what it used, which provider and model answered, and
whether the prompt was served from that provider's cache.

The store has kept a `usage` row per reply since D35, and `session_query` (D92) could see none of it:
"why does this provider's cache miss" was answerable only from the audit log, which is outside the
agent's reach by design and is not where a conversation's own numbers live.

- **The row names what answered it.** `provider`, `model` and `kind` (`reply` or `summary`) are
  recorded with the counts, because a conversation can switch models between turns (D91) and a cache
  hit rate only means anything for the pair that answered. A compaction is a request the person paid
  for (D78), so it is recorded the same way rather than left out — the request that filled a cache is
  part of the story as much as the one that read it.
- **`requests` is a second view** over the same in-memory snapshot (D92):
  `requests(ordinal, node, kind, provider, model, input, output, cache_read, cache_write, cached,
  cost, at, after)`. `cached` is 1 when `cache_read` is above zero: the counts stay the source of
  truth and the flag answers the question a hit rate is asked. `after` is the ordinal of the last
  message before the request, so a request joins to the prompt it answered — a large prompt with no
  cached tokens is only diagnosable next to that prompt.
- **`input` is the uncached part of the prompt**, which is what the OpenAI-compatible parser already
  makes of `prompt_tokens` less `cached_tokens` so that it agrees with Anthropic's `input_tokens`.
  A hit rate is therefore `sum(cache_read) / sum(input + cache_read + cache_write)`, the same
  addition the context estimate uses (D33), and the two providers can be compared on one query.
- **The gate is unchanged.** The same `transcript-read` grant, the same per-call audit record with
  its SQL, the same authorizer: the backing tables are as private as the views, and SQL still never
  reaches the saved database.
- **A row written before this reads as `unknown`** for its kind and empty for its provider and model,
  rather than being dropped or guessed at. What it cost is still true, and inventing a provider would
  make a hit rate lie. Rows imported from the older JSON Lines files are the same case.

**D95.** A reasoning item is an opaque value to carry, not text to rebuild, and the summary it
contains has to be asked for.

A ChatGPT subscription reaches the Responses API, which keeps no conversation of its own: every
request carries the whole history and `store` is `false`. In that mode the *only* continuity between
one step of a tool loop and the next is the reasoning item the server sent back — an `id`, a
`summary` array, and an `encrypted_content` blob that nothing but the server can read.

Saddle asked for that blob on every request and then threw it away on every reply: the parser matched
`function_call` items and ignored `reasoning` ones, and rebuilt a part from the streamed summary
text, which cannot be decrypted. So the model restarted each step without its own thinking.

- **The item is kept verbatim** — `id`, `type`, `summary`, `encrypted_content` and anything else the
  server put in it — and replayed unchanged in the position it was produced. A summary-only part is
  still built when a server streams summary text without sending an item, so nothing is lost from
  the display.
- **A summary is requested**: `reasoning: {"summary": "auto"}`. Without the key the server answers
  with an empty `summary` array and the interface has nothing to show however much the model
  thought. `auto` is the most detailed summarizer a model has; the named ones differ per model and
  `concise` is refused by some of them.
- **`reasoning.context` is left out**, which means `auto` — the model's own default. Naming
  `all_turns` is refused by models that do not have it, and it buys nothing here: every item is
  replayed whatever the model does with it, so the prompt is the same either way.
- **The legacy `include: ["reasoning.encrypted_content"]` is no longer sent.** In stateless mode the
  encrypted content comes back unasked for; the key is kept only for compatibility, and a server
  that does not know it may refuse it.
- **What is not done here is `configuration_update`** for a mid-conversation effort change (D57).
  It is documented for GPT-6 and later only, and an unknown input item is a request a server may
  refuse outright. Until it can be verified against the endpoint, `/thinking` still changes
  `reasoning.effort` and still rewrites the prefix.

**D96.** A provider gets its reasoning back, and by default nothing is dropped.

Reasoning was echoed only for the replies since the last user message. Every new prompt therefore
*deleted* the previous turn's reasoning from the middle of the request, which is a change to the
prefix: prompt caching matches from the beginning, so everything after the deletion is read at full
price and the server drops the reasoning items that follow it as its own anti-distillation rule. Two
providers state the requirement outright:

- **DeepSeek**: a request that carries `tools` *must* pass every earlier turn's `reasoning_content`
  back, and answers a 400 if it does not; a request without `tools` ignores what it is given.
- **OpenAI**: with `store: false`, replay every output item, keep them in their original order and
  append new ones; and on newer models the API removes reasoning items that follow a prefix change.

Sending is therefore right for both, and no provider branch is needed: the rule is **echo back what
the provider sent**. A server that sends no reasoning, such as OpenAI's own chat completions, has
none to echo and nothing is added for it. Reasoning is consequently kept for every model by
default; a rule that says otherwise has to be current, specific to a provider, and written down
here.

A resumed session counts its prompts by what the person wrote (`Item::prompt`), not by the wider
"has text" question the renderers used to ask: a compaction summary is not a prompt, and nothing
needs to identify a turn any more.

The same append-only rule applies to the Responses items (D95) and leaves one prefix change saddle
still makes deliberately: a compaction puts its summary where the messages it replaced were (D33).
The documented alternative is to put the compacted item at the end of the context instead; that is
not done yet, and the cost is recorded rather than hidden (D97).

**D97.** The store records two numbers that say whether the cache, and the reasoning, are working.

`requests` (D94) gains `reasoning_dropped`, and `cache_write` is read for both OpenAI surfaces:

- **`cache_write` is a cost, not a count.** A write is billed at 1.25× the ordinary input rate for
  the default lifetime (2.5× for the twelve-hour one), so a cold turn priced as a plain read
  under-reports what it cost (D36). Both the Responses and chat-completions parsers now read
  `cache_write_tokens`.
- **`reasoning_dropped` is the API telling us it rewrote our prompt.** A response reports the input
  items it removed in `input_transformations`; each one is counted, and an entry that does not name
  what it touched counts too, because a drop that vanishes because the schema moved is the one thing
  this number must never do. It is the instrument for D95 and D96: with reasoning replayed and no
  prefix change, it stays zero, and a non-zero row says the API — not saddle — is rewriting the
  history.
- **A compaction request records zero**, because it has no reply to read the count from.

**D98.** What the model thought is searchable, as a view of its own.

D95 and D96 made reasoning travel: it is kept verbatim, replayed in position, and stored on the
assistant item. `session_query` (D92, D94) could not see any of it — `Store::snapshot` mapped
`Part::Text` and `Part::Call` and dropped `Part::Native` — so a conversation of nine thousand
messages had not one reasoning block to query, and "what did the model think before it chose that
tool" was an assumption about the transcript rather than a fact about it. The text of a reply is a
lossy summary of the reasoning behind it, which is exactly the part left unsearchable.

- **A third view, not a column.** `reasoning(ordinal INTEGER, node INTEGER, part INTEGER,
  text TEXT)`, one row per reasoning block. Joined to `messages` on `ordinal` it puts a reply's
  thinking beside the text it produced; `part` keeps several blocks in the order they arrived. It
  is not folded into `messages.text`, because that column is what the model was shown and what every
  count over the transcript measures: mixing a derivation into the record of what was said would
  corrupt the counts it is compared against.
- **Only the prose is extracted**, which is the one thing the surfaces disagree about (D95). The
  function sits beside `Part` rather than in the store, because it is a fact about a wire format:
  `thinking` for Anthropic, `reasoning_content` or `reasoning` for chat completions, the `summary`
  parts — or the raw `content` — for a Responses item.
- **A block with no prose produces no row.** `encrypted_content` is a ciphertext and
  `redacted_thinking` is a placeholder; either would put unreadable bytes in a text column, and a
  `LIKE` that can never match makes an absence of thinking indistinguishable from thinking nobody
  can read. The block itself is still kept and still replayed (D95); only the view skips it.
- **A row is per block, not per turn.** A reply that thought twice has two rows, so
  `SELECT count(*) FROM reasoning` counts blocks rather than replies that had any, and a reply's
  whole reasoning is `group_concat` over its ordinal.
- **The gate is unchanged.** The same `transcript-read` grant, the same branch-only in-memory
  snapshot (D92), the same authorizer, and a backing table as private as the other two: a query may
  read `reasoning` and not `reasoning_snapshot`.

**D99.** The record holds how much of the output was thinking, and says what it still does not hold.

`Usage` is four counts that add up to a bill: input, output, cache read, cache write. The APIs
report more than that, and it divides into two kinds. One is a *partition* of a count already
recorded — how much of `output` was reasoning — which answers, per request, a question the reasoning
view can only answer when the reasoning is readable. The other is a field that changes what the
price *is*. D97 started this by adding `cache_write` and `reasoning_dropped`; this finishes the
first kind and writes down the second rather than reading it blindly.

- **`requests` gains `reasoning_tokens`**, read from `completion_tokens_details.reasoning_tokens`
  on chat completions and `output_tokens_details.reasoning_tokens` on Responses. Anthropic reports
  no such split and records 0: its `output_tokens` already counts the thinking, with nothing saying
  how much of it there was.
- **It is not a field of `Usage`.** Those four add up; this one is part of `output`, so a fifth
  field there would be summed twice by exactly the queries (`sum(input + output …)`) that exist to
  total a bill. It goes on the request row beside `dropped`, which is the same kind of number.
- **A row written before it reads as 0** (D94), and 0 means unknown rather than "the model did not
  think" — the distinction the column exists to draw in the first place.
- **What it buys** is the only evidence that survives a surface which hides its reasoning: a model
  served over chat completions reports the split and no text at all, so `reasoning_tokens` against
  `output` is where "it thought, we just cannot read it" becomes a number instead of an inference.
  Where D98 does have the prose, the two are cross-checks on each other.
- **What a parser still does not read, and why.** `total_tokens` is derivable. `audio_tokens`,
  `accepted_prediction_tokens`/`rejected_prediction_tokens` and `server_tool_use.web_search_requests`
  count features saddle never asks for. Two are left out because reading them without acting on
  them would be a column that says a bill is different and nothing that says by how much:
  **`service_tier`**, which both OpenAI surfaces and Anthropic report and which prices flex at about
  half and priority above the standard rate, and **Anthropic's `cache_creation.ephemeral_5m`/
  `ephemeral_1h` split**, where the one-hour write is billed at twice the input rate against 1.25×
  for the five-minute one. Neither can differ today — saddle asks for no tier and sets no `ttl`, so
  every request it makes is the default — and the price table is per model with no tier or TTL
  dimension (D36). The first of them to be read has to come with the price dimension that reads it,
  or the number is recorded and the bill stays wrong anyway.

**D100.** A conversation's requests ask for the same cache, where a provider lets them say so.

A prompt cache holds a prefix on one machine, and a request meets it only if it is routed there.
Until now saddle sent nothing that influenced routing, so a warm prefix was luck: the same
conversation could be answered by a machine holding nothing, and the miss looked like the provider's
per-node caching rather than like a request that never asked.

- **The key is the session id.** `prompt_cache_key` on both OpenAI surfaces. It is stored with the
  conversation, so a resumed conversation keeps the id it had (D53), and it is random, so two
  sessions in one directory never share a key. A compaction carries it too: it is a request that
  conversation paid for, and two compactions of one conversation share their prefix.
- **Two headers say the same thing** to whatever sits in front of the API: `session-id` and
  `x-client-request-id`, as Codex sends them. A header the configuration sets itself wins, as it
  does for the key's own header (D84).
- **Only OpenAI's own API gets any of it by default.** A compatible server implements the wire
  format, not the routing (D84), and a strict one refuses a body field it does not know — which is
  why the field is gated on the base URL at all. A proxy in front of `api.openai.com` is, as far as
  saddle can tell, a different provider, and gets nothing.
- **A table can ask for it**: `cache-affinity = true` sends the key and the headers whatever the
  host is. The ChatGPT preset turns it on, because its host is `chatgpt.com/backend-api/codex`
  rather than `api.openai.com`, Codex sends these same fields there, and a subscription is otherwise
  the one path that would get no affinity at all. `cache-affinity = false` takes it back, and is the
  one-line answer if that endpoint ever refuses them. The lifetime stays off for an OAuth endpoint
  whatever the table says.
- **The subscription backend is told an `originator`.** It refuses one it does not recognise, and
  the ones it recognises are the official clients'. Saddle names the client it already claims to be
  when it asks that backend which models it serves (D85) — the same claim, in the one place where
  the alternative is not being served at all.
- **`cache-retention = "long"` in a provider table** asks for `prompt_cache_retention: "24h"`. Off by
  default: for the models that read this field the provider's own default is already 24 hours unless
  the organization is under zero data retention, where asking is what gets refused. On GPT-5.6 and
  later the lifetime is set by `prompt_cache_options.ttl` instead, so the field is meaningful only on
  the earlier models that document it.
- **Never on a ChatGPT sign-in.** That endpoint rejects the lifetime field, so a subscription
  conversation carries the key and the headers and no lifetime. Its configured host is not
  `api.openai.com` either, so today a subscription session sends none of the three until that host
  is added.
- **What is still not done**, deliberately: explicit cache breakpoints —
  `prompt_cache_options.mode: "explicit"` with a `prompt_cache_breakpoint` after each tool result —
  are what OpenAI recommends for a multi-turn agent, and are the one lever that helps a *branched*
  conversation (D54) reuse a prefix up to the point it forked. Neither pi nor saddle places them, and
  nothing here depends on it; it waits for a decision that can test it against a real endpoint.

**D101.** The subscription backend is spoken to over a WebSocket, and a kept connection makes a turn
a delta.

The backend a ChatGPT subscription reaches refuses `store: true`, so it keeps no conversation of its
own and every request carried the whole history again. That is also why a cache miss there costs
what it does. Over a WebSocket the backend does keep the connection's state, `previous_response_id`
refers to it, and a later request can therefore carry only the items the connection has not seen.

- **One reply path.** A frame is `{type: "response.create", ...}` with the same body the HTTP request
  carries; each frame back is wrapped as the event the SSE reader already consumes. The parser, the
  reasoning handling and the usage reading are the ones that were there, so a second API surface was
  not written for the second transport.
- **A delta is legal only on the connection that has the state.** The socket is kept per session id,
  and a new connection always starts from the whole history, because what a continuation refers to
  was left behind on the old one.
- **A delta needs an exact baseline.** The body must match apart from `input`, and the new input must
  extend what the server holds: what was sent plus what the reply produced. The produced items are
  rendered by saddle, not taken from the response's `output`, because those two shapes differ and a
  baseline that does not match what the next request writes would make every delta a rejected one.
- **Only a completed response is kept.** A cancelled, failed or closed stream leaves a connection
  nothing can be continued from, so the next request starts again in full.
- **A continuation the backend has lost is resent in full.** It says so with an error frame rather
  than a reply; one frame is cheap to spend finding out, and the alternative is a failed turn.
- **Everything falls back.** A handshake that fails, or a frame that will not send, returns to the
  HTTP request that was already being made. The fallback is silent — it is not the person's problem,
  and the worst case is then the old behaviour rather than a failed turn.
- **`check` says which of these a provider gets**, because each one is conditional and silent until
  a bill arrives: the key and headers, the lifetime, the WebSocket, and the originator.

**D102.** A cached subscription connection belongs to its destination and authentication context,
not just to a conversation id. Reuse requires the same URL and request headers (compared through a
digest so the cache retains no extra credential copy). A changed account, credential or endpoint
opens a fresh connection with the full history. The WebSocket handshake uses the Responses v2 beta
header, independently of the HTTP preset's beta header. A completed socket is handed back before
its terminal event is published: the parser can finish immediately on that event. Incomplete
responses do not seed a continuation. Handshake/send fallback to HTTP is reported to the person,
without putting transport diagnostics in the model's conversation. Neither a delta payload nor
connection reuse guarantees cached token accounting; the server's reported usage remains the
evidence of a hit.

**D103.** An MCP server reaches the model only through a grant that names the server and each of
its tools: `mcp = [{ server = "job-scout", tools = ["search"], why = "scrapes boards" }]`. There is
no wildcard and no pattern, because a server's advertised tool list is never authority: a tool it
grows overnight is not callable until a person writes it down. A grant naming no tools, an empty
server name, or `*` for a tool is a load error rather than a grant that can call nothing. This one
line is what says a server and its tools exist at all — the system prompt and `saddle check` both
print `Policy::describe`, so the model is told what it may reach through the same record a person
reviews (D41, D47).

**D104.** An MCP server is declared, not discovered: an `[mcp.<name>]` table in `config.toml` names
what to run (`command`, `args`, `env`, `cwd`) or where to reach it (`url`), and exactly one of the
two. A table naming both or neither, a plain-http URL off this machine, a `url` carrying `args`,
`env` or `cwd`, an empty `command`, a description longer than 200 characters, or a zero timeout is
a load error naming the table, reported when the session opens rather than when the model first
reaches for a server. Config is outside every root (D42), so no server arrives with a repository,
and the grant (D103) still decides whether it runs: `saddle check` prints the tables against the
grants, naming both a server that is declared and not granted and one that is granted and not
declared.

**D105.** An MCP server is a delegate of the agent, so what it reaches is the intersection of what
its grant asks for with what the agent holds: `mcp = [{ server = "x", tools = ["t"], net = [{ host
= "api.example.com" }] }]`. A grant that asks for nothing inherits the agent's own reach; one that
asks for more — a root the agent lacks, a host no agent `net` grant covers, a scheme or port the
agent does not hold — is a load error naming the rule (`agent.mcp[0].fs-read[0]`), never a silent
trim, because a server quietly weaker than written is a worse thing to debug than a policy that
will not load. A suffix covers strict subdomains only, as `matches` does, so `*.example.com` does
not cover `example.com`, and no suffix covers `*`. Exclusions from both grants apply, so the
result is no wider than either. Nothing else crosses: a server holds no plain scope, no root of its
own and no model budget, because it is a command rather than a plugin principal.

**D106.** A command the host holds a conversation with needs its own stdin and stdout, so its spec
travels on fd 3 rather than fd 0: `saddle_sandbox::spawn` mounts the spec socket there, puts the
child in its own process group, and leaves both pipes to the caller. A helper started the old way
has no fd 3 — Rust closes every descriptor it opens — so `run` is unchanged and the spec still
arrives on stdin, after which the helper reopens stdin as `/dev/null`. The caller owns the pipes and
therefore the relay: it must drain stderr, wait or kill the child, and service the proxy while it
waits, which is why `spawn` does none of that itself. A fallback to reading the spec from stdin
hangs rather than failing, so a host that speaks a protocol to a child needs its own handshake
deadline.

**D107.** A stdio server is started under the spec its grant was narrowed to (D105, D106) and
spoken to in JSON-RPC 2.0, one object per line, over its own stdin and stdout. Its stderr is
drained into a bounded tail on its own thread and quoted only when something failed, because
"exited with status 1" without the reason costs an hour. The pipes are read on a thread so the
caller can service the relay while it waits, and every reply has a deadline: the failure mode of
this plumbing is a stall rather than an error (D106). A server gets the network a command gets —
the relay in its namespace, one door out (D79) — and the `env` table of its declaration is added
after the allowlist, because it is the server's own configuration and a token for it has nowhere
else to live. `saddle mcp list` starts each declared and granted server and prints what it offers
against what the grant allows, which is the by-hand check `saddle tool` plays for the built-ins
(D17).

**D108.** A server's tools are not declared to the model until it asks for them. `tool_search`
matches a query against what the granted servers offer and declares the matches — as many as eight
— and they stay declared for the session, so a session with ten configured servers costs ten lines
in the prompt rather than ten schemas, and the tool list changes only when a declaration is made.
The declared tool is named `mcp__<server>__<tool>`, which no built-in can collide with, and its
description says which server it came from. A search can only ever declare what the grant names: a
tool the server advertises and the grant does not is counted in the result and never declared, and
calling one is a denial even if the model guesses the name. A granted tool that was never declared
is a different thing — the model naming something it cannot see — so that is a protocol error
rather than a denial. The registry lives on the session because starting a server needs the engine
and the audit log, so it is taken out for the call and put back, which is the price of a tool call
and a policy decision being the same object.

**D109.** What a search declared is state of a branch, not of the process. The declaration is
written as a node beside the tool results that arrived with it — a bookkeeping node like `usage`,
never a message, so replaying a branch costs no tokens and the conversation the provider is sent is
byte-identical to the one before it. A resume, a fork taken after the declaration, and a jump in
the tree all bring it back, and a rewind to before it forgets it along with the messages that
followed. Restoring is a filter and not an assignment: a name is put back only if the grant in
force still names that tool of that server, so a branch recorded under a wider policy confers
nothing under a narrower one, and a name that is not a tool of ours at all is dropped rather than
trusted. The stored row is a name, never a schema or a reach: what a tool does is read from the
server afresh, so a server that changed its arguments cannot have a stale definition called with
an old shape. A new conversation declares nothing, whatever the process still has running.

**D110.** A server reached over streamable HTTP has no child process, so the host makes every
request itself — and each one is decided at the moment it is made, against that server's own reach
rather than the agent's whole list. The reach is the net grants narrowed from the agent's in D105's
way, so a server that names one host cannot reach another the agent may, and one that names nothing
inherits the agent's reach as before. A private address still needs a grant that spells it out, and
the question is asked of the narrowed list, not of the agent's. The record names the server
(`via: mcp`), because there is no argv or cwd to point at, and the header names are recorded and
never their values. The transport itself is the one the spec describes: a POST carrying one
JSON-RPC object, a reply that may be JSON or an event stream and is read either way, `Mcp-Session-Id`
kept from the handshake and sent back, `notifications/initialized` before anything else, and
`nextCursor` followed so a paginated catalogue is complete. A 401 or 403 is a failure, not a retry:
without a token there is nothing to retry with, and looping would spend a quota to learn the same
thing twice. OAuth is the next decision and not a silent fallback here.

**D111.** `saddle mcp call <server> <tool> '<json>'` makes one call the way the model does — the
registry starts that one server, reads its catalogue, declares what the grant names, and decides
the call — with no conversation around it. It is deliberately not a second path: it uses the same
`connect`, the same declaration and the same decision, so a person checking a server by hand is
checking what the model will actually get, and a by-hand call cannot succeed where the model's
would be refused. It is also the only way to reach a remote server before a search has declared
anything, and the way to exercise a transport without spending a model request. A server the policy
does not grant, or one with no `[mcp.<name>]` table, is a usage error and says so.

**D112.** Every MCP tool call is audited — the allow and the refusal — as one `decision` record with
scope `mcp`, the server and the tool named, and the arguments recorded as a length and a hash rather
than as text (D24's rule, because an argument may hold a secret the way a query string may). It is
one function, `record_call`, called by the registry and by nothing else: the model's call and the
by-hand one are the same decision with the same record shape. A call that names a server the policy
does not grant is a usage error before any call exists, so it is not a record; a call the grant does
not name a tool for is a refusal and is. This closes the gap where an MCP tool call left no trace
while every other tool call wrote one, which is the whole of D10's promise.

**D113.** A server's `[mcp.<name>] timeout` is the deadline every request of that server gets, on
either transport — the same number `saddle check` prints, so what the config promises is what is
enforced. A stdio client had a constant and an HTTP one used the fetch deadline, which made the
setting a decoration; both now take it from the table, and a server that never answers is cut off at
the moment its table names rather than thirty seconds later. The default stays sixty seconds, and
the message names the deadline it hit, so a timeout is diagnosable from the transcript alone.

**D114.** A server may send requests and notifications of its own, and both are handled rather than
dropped. Every message off a server's pipe is classified — a reply to us, a request of its own, a
notification — and a request is answered by whoever holds the pipe, at the next moment it is waiting,
because that is the only place the answer can go back down. `roots/list` is answered with the roots
the session would lower into a sandbox spec: what a command may reach, not a wish list.
`sampling/createMessage` and `elicitation/*` are refused with `-32601` and a sentence saying this
client declares no such capability — the handshake offers none, so a conforming server will not ask,
and a server that asks anyway learns why instead of waiting forever. `notifications/tools/list_changed`
marks that server's catalogue stale, and the next search re-reads it before scoring, so a tool a
server has withdrawn is not offered and one it has added is not invisible.

**D115.** A remote server's OAuth is discovered, not configured: a 401 says where the authorization
server is (RFC 9728's protected-resource metadata, or the server's own origin when it publishes
none), and that server's metadata says where to authorize, to exchange and to register. Saddle
registers itself as a public client, generates a PKCE verifier, prints a URL and catches the
loopback redirect on a port it reserved, so nothing about the flow is a fixed port or a hand-written
client id. The callback's `state` must be the one this sign-in sent, because a code that arrives with
someone else's state is a code that was put in the person's browser. Tokens are stored in `auth.json`
under `mcp:<server>`, outside every root, beside the provider credentials and unreadable by any
command (D16, D42). Every request the flow makes — metadata, registration, exchange — is decided
against the *person's* own net grants and audited, because the hosts come from a document the server
supplied, which is exactly what a grant is for; the label in the record names the server it was for.
`saddle mcp auth <server>` is the only way a token is obtained: nothing here signs in on its own
initiative, so a server cannot pull a person into an authorization flow by answering 401.

**D116.** A stored token is sent as `Authorization: Bearer`, and a 401 is answered with one refresh
and one retry — never a loop, because a server that refuses twice will refuse again and the person's
account is not saddle's to spend on finding that out. A refusal with no refresh token, or a refresh
that fails, ends in an error naming `saddle mcp auth <server>`: the terminal message has to say what
fixes it. The refresh is decided against the server's *own* reach rather than the person's, because
it happens while a session runs rather than at a terminal, which means a server whose authorization
server is a different host has to name that host in its reach — narrower than the sign-in, and said
so here rather than discovered at the worst moment. A token is never written to a log: the
`authorization` header is recorded by name, and its value never (D48).

## 4. Scope enumeration

| scope | grants | declared by | enforced at |
| --- | --- | --- | --- |
| `exec` | running commands | `saddle:host/exec` import | sandbox helper (D18, D19); the network it reaches is `net` (D79) |
| `fs-read` | reading files under granted roots | `saddle:host/fs-read` import | policy per call |
| `fs-write` | writing files under granted roots | `saddle:host/fs-write` import | policy per call, reserved paths (D23) |
| `net` | HTTP requests to granted hosts | `wasi:http/outgoing-handler` import, `fetch` and `websearch` tools, a command's proxy | host HTTP client, policy per request (D28, D39, D79, D82) |
| `transcript-read` | reading session messages and their request usage, and observing them as they arrive | `saddle:host/transcript-read` import, `saddle:plugin/observer` export | policy per call |
| `context-contribute` | adding notes and diagnostics to the context | `saddle:host/context-contribute` import | policy per call, slot schemas (D26) |
| `tool` | registering tools the model can call | `saddle:plugin/tool` export | load-time check, catalogue rendering (D26) |
| `veto` | denying a pending tool call | `saddle:plugin/veto` export | policy per call (D21) |
| `model-invoke` | spending tokens on the session provider | `saddle:host/model-invoke` import | policy per call, against a budget |
| `ui` | rendering widgets in the terminal | `saddle:plugin/ui` export | widget vocabulary (D27) |
| `persist` | storing state between calls | `saddle:host/persist` import | per-plugin storage namespace |
| `mcp` | reaching a declared MCP server, and calling the tools of it that a grant names | `mcp` grant (D103) | policy per call |

## 5. Enforcement architecture

One capability representation, in `saddle-cap`, lowered three ways:

1. to the set of interfaces a component may import and export, checked before instantiation;
2. to per-call decisions in the policy engine, for every host function a plugin or built-in tool calls;
3. to a sandbox spec for child processes, applied at `exec`.

The WASI layer is closed the same way for every plugin: no preopens, empty environment, no sockets.
Authority arrives only through `saddle:host` interfaces and `wasi:http`, and both reach the policy
engine.

- A component can reach only the scopes its imports and exports name. That set is a static property
  of the component and is checked before any plugin bytecode runs.
- The policy engine decides each call with resolved arguments: which path under which root, which
  host, how many tokens, which storage namespace.
- The sandbox spec gives child processes the same roots, because the policy engine cannot see inside a
  child.

A denormalized design where the three draw from separate sources is the failure case. Deriving the
sandbox spec from the same grants as the policy decisions stops a plugin without `net` from reaching
the network through `curl` in a child process.

## 6. Extension surface, from the parity benchmarks

Roughly the union of what pi and opencode let an extension do, with the scope each maps to:

| extension point | scope |
| --- | --- |
| register a tool, whose body runs with the plugin's authority and whose output becomes agent input | `tool` |
| execute commands | `exec` |
| read and write files | `fs-read`, `fs-write` |
| reach the network | `net` |
| read the transcript | `transcript-read` |
| contribute text into the context | `context-contribute` |
| observe messages and tool results | `transcript-read` |
| block a pending tool call | `veto` |
| invoke the model | `model-invoke` |
| render UI | `ui` |
| persist state | `persist` |

## 7. Milestones

**v0.** Harness with built-in tools and no plugin loader. Every tool invocation passes the policy gate.
Deliverable is the gate, with a session that can read, edit and run commands under a policy file,
and the audit log (D24) showing what the gate allowed. The original trust gate (D23) shipped in v0
but was removed by D44 after D42 moved policy outside the agent's reach.

**v1.** One plugin, one scope, one load-time refusal. Deliverable is a hostile plugin that imports
`saddle:host/fs-read` without the grant and fails to instantiate, plus a test asserting the failure.
The test runs at two levels: against `saddle_cap::component::check_component` with a hand-written
import list, and end to end with the compiled component.

**v2.** Remaining scopes and their host interfaces, `veto`, the D25 provenance wrapper, then
presentation.

The v1 test is the whole thesis. If a plugin that imports `fs-read` without the grant still loads, the
model is decorative.

## 8. Deferred

- Long-lived plugin instances (`lifetime = "session"`, D13).
- `exec` on macOS through Seatbelt.
- A mount namespace with read-only bind mounts for reserved paths and empty mounts over excluded
  paths, replacing the D20 snapshot and making `exclude` bind `exec`.
- Tracking child processes by cgroup. A child that calls `setsid` leaves its process group and
  survives the group kill in D19.
- An external append-only sink for the audit log.
- Input widgets in `ui`.
- A skill's bundled files are not listed by anything: the skill's own text names them, and listing
  them would be a directory read the policy does not decide.

## 9. Known risks

- The policy engine and the WIT world are one design. Getting them out of step produces a system that
  looks enforced and is not.
- Building an argv matcher for `exec` first is the tempting shortcut. It works until a plugin writes a
  Makefile.
- Parity pressure will pull toward extension points that have no scope yet, and the easy answer will be
  a generic escape hatch.
- The component model toolchain moves. A pinned version is a schedule dependency, not a detail.
- The sandbox depends on a list of system paths (D19). A tool that needs a path outside the list fails
  with a permission error that looks like a bug in the tool.
