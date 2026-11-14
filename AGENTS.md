# Working in this repository

Saddle is a coding-agent harness in Rust. Its planned plugin boundary is WASI components whose
authority is a *declared* scope set, checked before instantiation. The current v0 has built-in tools
and the component interface checker, but no plugin loader yet.
`docs/plan.md` is the specification; read it before changing behaviour in the policy, sandbox or tool
layer.

There is no README. `docs/plan.md` is the design document, the rationale and the milestone list.

This file is one of the instruction files saddle loads into its own prompt (the workspace root
`AGENTS.md`; `crates/saddle/src/context.rs`, D32). Keep it accurate — a stale instruction file is worse than none,
because it is followed.

## Layout

| crate | owns |
| --- | --- |
| `crates/saddle-cap` | scopes, roots, grants, policy and manifest parsing, the component import check, the exec lowering, and the narrowing of a delegated principal's reach (D105). No wasmtime dependency. |
| `crates/saddle-policy` | per-call decisions (`engine`) and the hash-chained audit log (`audit`) |
| `crates/saddle-sandbox` | applies a sandbox spec to the current process and execs the target, including the command's own network namespace and the relay in it (D79); starts a long-lived child whose stdin and stdout belong to the caller, with the spec on fd 3 (D106); also builds the `saddle-sandbox` helper binary used by its own tests |
| `crates/saddle` | the binary: built-in tools, model providers, the model loop, the TUI |

Inside `crates/saddle/src`:

| module | owns |
| --- | --- |
| `agent` | the model loop (no request limit unless one is set, D37; a turn stops on a repeated call and answer, D38), the system prompt (base plus grants, so a policy reload replaces only the grants, D47), tool dispatch, and compaction — synchronous at the hard threshold, started in the background at three quarters (D78), and run by the worker thread when the session is idle (D77) |
| `session` | opening the workspace, resolving roots, the policy engine and audit log for a session |
| `tools/fs`, `tools/search`, `tools/bash`, `tools/fetch`, `tools/patch`, `tools/websearch`, `tools/session_query` | the eleven built-in tools (`fetch` is host-side network, D39; `patch` is D56; `websearch` asks the provider, or Exa, D82; `session_query` is branch-only read-only SQL over messages, request usage and reasoning, D92, D94, D98) |
| `net` | the command network (D79): `hello` reads a hostname out of a TLS handshake, `head` out of a `CONNECT` line or a `Host` header, `proxy` decides and carries a connection, `bridge` joins the proxy's threads to the one engine and the one audit log |
| `tools/diff` | line diffs of edited files, for the transcript only — the model never sees them |
| `context` | instruction files (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md` fallback), read through the policy |
| `skills` | `SKILL.md` skills: discovery through the `read` gate, the prompt listing, `/skill:name` (D64, D67, D68, D69) |
| `ignore` | ignore-file matching (`.gitignore`, `.ignore`, `.fdignore`), shared by `tools/search` and `skills` (D68) |
| `mcp` | the MCP servers a person declared in `config.toml` (`[mcp.<name>]`): the transport fields and their validation (D104), the `check` lines that compare them with the grants, `saddle mcp call` as the model's own path with no conversation around it (D111), a `decision` record for every call allowed or refused with the arguments hashed (D112), the answers to what a server asks — `roots/list`, `-32601` for sampling and elicitation, a re-read when a catalogue changes (D114) — the discovered OAuth sign-in with its tokens in `auth.json` (D115), and the bearer a call carries with one refresh and one retry when a token expires (D116), the stdio client — JSON-RPC over a sandboxed child's pipes, with a reply deadline and a bounded stderr tail (D107) — the streamable-HTTP transport, whose every request is decided against that server's own narrowed net reach and recorded with the server named (D110), and the per-server deadline both transports enforce from the table's own `timeout` (D113), the prompt line per granted server and the label on whose words a result is (D120), `saddle mcp logout` (D121), the `/mcp` report, its restart, and its session-scoped disable and enable (D118, D123), the `exposure` word each table chooses — `deferred`, `direct` or `hidden`, with `codemode` refused by name (D117) — and its exact-name `tool-exposure` overrides (D122), and the registry that `tool_search` declares tools from, so nothing a server offers is in the prompt until the model asks (D108), with a declaration saved as a branch node so a resume, a fork or a `/tree` jump brings it back and a rewind forgets it — and restoring one re-checks the grant (D109) |
| `compact` | replacing the older part of a long conversation with a model-written summary (D33), of a session left alone (D77) or written while the turn carries on (D78) |
| `store` | the saved conversations: a SQLite tree of messages per workspace (D53), imported from the older JSON Lines files once, with one row per model request recording what it used and which model answered (D94), and a `declared` node for what an MCP search declared on a branch (D109) |
| `output` | line-mode output: a terminal, or one JSON object per line for a program (D52) |
| `cache` | the cache-miss notice: one line to the user when a prompt is re-read at full price (D50) |
| `reserved` | the D20 snapshot and restore of reserved paths around `exec` |
| `provider/` | the neutral conversation form, the Anthropic, OpenAI-compatible and Responses renderers (every other server is a `[providers.<name>]` table naming which one it imitates, D84), the cache-affinity key, headers, lifetime and originator, sent to OpenAI's own API and to the subscription backend (D100), and the WebSocket that backend is spoken to over, with the delta a kept connection allows (D101), OAuth login and token refresh (D85), the model list, the cached context windows and prices (D34, D36); reasoning is kept for every model and replayed in its original position (D95, D96) |
| `tui/` | the full-screen UI: app state, rendering (a prompt is markdown in the user's colour, D81; tables are fitted to the width, D83), editor, login (a key or an OAuth sign-in, D85; the searchable model picker `/model` opens, D88), selection (a drag copies the transcript's text or the open dialog's, D87), settings, session list, `permissions` (the grants editor, D45), `tree` (the message tree, D54), `image` (drawing a picture: kitty and iTerm2, D59), `status` (the `[ui] status` template, D36), `theme` (the colour roles, D71), `palette` (pi's role vocabulary, colour forms and the palette derived from the terminal, D72, D73), `terminal` (asking the terminal for its colours, D73), `syntax` (code blocks: syntect's grammars, nine roles, the theme's colours, D80) and `themes` (the `/theme` picker, D74), and `browser` (opening the link a sign-in shows, D86) |

`crates/saddle/src/main.rs` dispatches subcommands (`run`, `check`, `tool`, `audit verify`)
and intercepts `__sandbox` before clap parses anything — that is the helper process path, and it must
stay first in `main`.

Built-in tools are `read`, `write`, `edit`, `patch`, `ls`, `find`, `grep`, `bash`, `fetch` and
`websearch` and `session_query` (`tools::NAMES`). `patch` applies one apply-patch document across several files, deciding
every path before writing any of them, and refuses the whole patch if one is denied (D56).
`fetch` gets a URL that is already known and `websearch` finds one that is not: both are made by the
**host** after the engine allows them (D39, D82), and both name their host in a `net` grant — `fetch`
the URL's, `websearch` the search host's, which is the provider's own (`search-url`, `api.synthetic.new`
for the built-in Synthetic preset) and Exa's (`api.exa.ai`, or `mcp.exa.ai` without a key) when the
provider serves no search (D82).
`fetch` may send headers — a secret belongs there, never in a query string — and the audit log records
the header names but never a value (D48). A command's own network is separate and reaches the proxy in
this process (D79), so `curl` in `bash` works where the grants allow it. Adding network access means
adding a `net` grant to the policy, never widening the sandbox allowlist. A grant is a hostname, a
`*.suffix`, or `*` for any public host (D40); a wildcard still cannot reach loopback or private space,
because that exemption needs a grant that names the address. `saddle tool <name> '<json>'` runs any one
of them as the agent through the gate, which is the most direct way to check a policy change by hand.

Planned but not yet present: `saddle-wit`, `saddle-loader`, `saddle-sdk` (v1, D15). MCP
prompts, MCP resources, `codemode` and images in a tool result are refused rather than missing
(D119) — a boundary that is written down, not an omission.

OpenAI API-key sessions use published standard token prices for known non-tiered models when the
config and catalogue state none (D93); compatible servers do not inherit them. ChatGPT login uses
its subscription quota instead, not a fabricated API bill: `{quota}` shows each reported window's
remaining percentage and reset countdown (`5h`, `weekly`). `saddle check` includes absolute reset
timestamps. Usage GETs use refreshed OAuth and the same account headers as model requests.

`session_query` requires `transcript-read = {}` in the agent grants. First-run defaults include it;
existing explicit `[agent]` tables remain as written, and `--revoke transcript-read` disables it for
one run. It queries only the active conversation branch, including pre-compaction messages, through
three in-memory views:
`messages(ordinal, id, role, text)`, `requests(ordinal, node, kind, provider, model, input,
output, cache_read, cache_write, cached, cost, reasoning_dropped, reasoning_tokens, at, after)` —
one row per model request, `kind` being `reply` or `summary`, `cached` 1 when the prompt came from
the provider's cache, `reasoning_dropped` the reasoning items the provider said it removed from the
prompt (D97), `reasoning_tokens` how much of `output` was thinking (D99), and `after` the ordinal
of the last message before it (D92, D94) — and `reasoning(ordinal, node,
part, text)`, one row per reasoning block, joined to `messages` on `ordinal` and in arrival order by
`part` (D98). Only the prose is a row: an encrypted or redacted block is kept and replayed (D95) but
has nothing a query could read. That is what a poor cache hit rate is diagnosed
with: `SELECT provider, model, sum(cache_read), sum(input + cache_read + cache_write) FROM requests
GROUP BY provider, model` (D94). It never runs SQL on the saved database, grants filesystem
access to state, or permits transcript mutation (D92). Results are capped at 100 rows and 50 KiB;
SQL and execution are bounded too. A standalone `saddle tool session_query` has no active
conversation and reports that rather than picking another session.

## Commands

Use **`cargo nextest`** to run tests, not `cargo test`: one process per test, run in parallel, so a
test cannot see another one's state and a leak is caught here rather than in CI. On a suite this
size the wall clock comes out about the same as `cargo test` — the binary's 470 tests take ~2.1 s
either way, because a process per test costs what a thread saved — and the reason to use it is
everything else: `-E` filters by name, `--no-fail-fast` summaries, per-test timeouts and retries,
and isolation that makes a flaky test obvious.

```sh
cargo build
cargo nextest run                            # every suite, one process per test
cargo nextest run --bin saddle               # just the unit tests in the binary
cargo nextest run -p saddle-cap              # one crate
cargo nextest run -E 'test(/^tui::/)'        # by name, with nextest's filter expressions
cargo fmt --all --check                      # must stay clean; no rustfmt.toml, so defaults apply
cargo clippy --all-targets                   # must stay clean
```

`nextest` is installed at `~/.cargo/bin/cargo-nextest` (0.9.146). Three differences from
`cargo test` are worth knowing:

- **Doctests are not run** — nextest runs test binaries, not rustdoc. Nothing here documents a
  runnable example (the fenced block in `config.rs` is a sample file, in a binary crate, which has
  no doctests at all), so nothing is missed today; `cargo test --doc` is what would run one if it
  were written.
- **One test is one process**, so a test that relies on state another test set up will not see it,
  and `-j`/`--test-threads` is how many run at once rather than how many share a process. It is
  also why a test that leaks a global (an `std::env` variable, a signal handler) passes under
  `cargo test` and fails here.
- **A failure stops the run** unless `--no-fail-fast` is given, so use
  `cargo nextest run --no-fail-fast` to see every failure in one pass.

`crates/saddle/tests/agent.rs` needs loopback `socket()`/`bind()`. D79 lets it run when the command's
network namespace is available; the no-network fallback cannot run those tests. Diagnose the actual
failure rather than assuming every failure in `saddle::agent` is environmental: mock HTTP parsing or
fixture failures are code defects. The mock serves startup catalogue GETs separately from canned
model POSTs (D34), and every fixture uses throwaway home, config and state directories.
One binary at a time is `--lib` or `--test <name>`, as in
`cargo nextest run --test cli`; the binary's own unit tests are `--bin saddle`.

There is no CI config in the tree, no `rust-toolchain` pin and no `rustfmt.toml`. Edition 2024,
`rust-version = 1.88`. All dependencies are declared in the root `Cargo.toml` under
`[workspace.dependencies]` and referenced per crate as `dep.workspace = true`; add new dependencies
in the root, not in a crate manifest.

Do not pass `--offline`. It is no longer needed: a command has a network of its own (D79) and reaches
the crates.io registry through the proxy when the session's `net` grants allow it — which is how
`syntect` arrived (D80). Checked, not assumed: a crate that was not in the cache was added, fetched,
compiled and run without it, and `cargo update --dry-run` read the index.

Dropping it does not make a build depend on the network. Cargo touches the index only when something
is missing, so a build whose dependencies are all present needs no network at all — checked by
building and running the suite with the proxy pointed at a dead port, which succeeded. `--offline`
therefore buys nothing on the ordinary path, and it turns a genuine dependency problem into a
confusing one: a `--offline` build fails with "no matching package" when the real answer was one
download away.

Keep it in your pocket for the case it is for: a session whose grants do not include the registry, or
a machine with no network at all. There an ordinary build still works — nothing is fetched — but a
dependency that is missing fails after about ten seconds of cargo retrying with a connection error,
where `--offline` says `no matching package` at once. That is the only thing it improves. The registry
is still the only place a crate comes from — nothing here is fetched by hand.

An online build leaves `Cargo.lock` alone when the requirements are already satisfied; `cargo add` and
`cargo update` write it, so check `jj diff --stat` after those, as for any scripted edit.

### Running the binary by hand

`saddle` writes first-run defaults into `$XDG_CONFIG_HOME/saddle` and its state into
`$XDG_STATE_HOME/saddle`, where `sessions.db` (D53) holds every saved conversation. Under a sandboxed agent both default to `~/.config` and `~/.local/state`,
which are outside the granted roots, so the process dies with `Permission denied`. Point them at a
writable location:

```sh
XDG_CONFIG_HOME=/tmp/saddle-try/config XDG_STATE_HOME=/tmp/saddle-try/state \
  cargo run -q --bin saddle -- check
```

`--bin saddle` is required: the workspace has two binaries and no `default-run`. `saddle check` is the
cheapest way to see the resolved roots, the grants in force for the working directory, the
effective grants, whether that policy has been reviewed, whether `exec` is available on this kernel,
what network a command will get (its own namespace, or none), the context window and price in force
for the configured model, and what its prompt cache will be told — the key and session headers, the
lifetime, the WebSocket and the originator, each of them conditional (D100, D101).

To change what every workspace allows, edit `[agent]` in `config.toml`; to change one workspace, edit
its file under `policies/` — what it sets replaces the base grant for that scope, narrower or wider,
and takes effect in the next session. `--grant`/`--revoke` do it for one run. `/permissions` in the TUI edits the same file, and `a`
there adds a grant by typing what it is (`net <host>`, `root <name>`, `exec`, …); it is a person's
tool and no tool exposes it (D45, D46), and a change there applies at once, with the model told on
its next request (D47). Editing `saddle.toml` in a repository has no effect (D42, D43).
`/login` picks a provider and stores its credential: a key typed in, or — for a provider that
authenticates with OAuth — the device flow, with the code on screen while a background thread polls;
the code is put on the clipboard as it arrives, the link is underlined and opens the person's
browser on a click or `o`, and `c` copies it (D85, D86). It chooses for the session it is run in and
stores the credential, and writes nothing else to `config.toml` except the `[providers.<name>]`
entry a server added there needs to exist at all — the default is `ctrl+s` in the model list (or
editing the file by hand), and nothing else the interface does (D91). `saddle login <provider>` and `saddle logout <provider>` are the same thing
without the interface, and `check` says whether the provider in force is signed in.
`/model` with no name is a searchable list of what **every** configured provider serves — each entry
naming its provider, filterable by provider as well as by name — with the provider and model in force
named above it and highlighted in the list; `/model <name>` switches straight to one (D88, D89).
Either way the choice is this conversation's, recorded with it and **not** written to `config.toml`,
so `ctrl+s` in the list is the only thing that changes what a new session starts with: it saves both
the provider and the model (D91).
A resumed conversation therefore comes back on the model it was using, whatever the default has
become since, and two sessions in one directory keep their own models — `/reload` and a hand edit of
the file re-read the settings but never switch provider or model (D91). `/login` ends on the same
list, so a model is chosen the same way whether or not the credential was just entered.
`/skill:<name> <request>` loads a skill's instructions into the conversation followed by the
request, in the TUI and in line mode, and is the only way to reach a skill marked
`disable-model-invocation` (D69). `/reload` reads the instruction files and skills again and rebuilds the
prompt, and re-reads the settings and the theme, so a change made mid-session is picked up without
restarting (D70). `/theme` lists every theme with the highlight previewed on the whole screen — escape puts back the
one that was in force — and `/theme <name>` switches straight to one and saves it (D74); colours are
`[themes.<name>]` tables of roles in `config.toml`, named as pi names them, and the renderer names no
colour directly (D71, D72). A colour is an ANSI name, `#rgb`, `#rrggbb`, `oklch()`, `okhsl()`, a
palette index, a `vars` entry, or `""` for the terminal default; `theme = "light/dark"` picks by
the terminal's answer (or `COLORFGBG`), and `--use-theme` chooses for one run. Every role pi defines
is understood; one for something this interface has no element for is carried and paints nothing. The
four Catppuccin flavours are built in (D76): `catppuccin-latte` is for a light terminal and the other
three for a dark one, and the picker says which. `theme = "system"` asks the
terminal for its own colours at startup (OSC 10/11/4, 100 ms) and derives every role from them with a
WCAG contrast floor, falling back to ANSI colours if it answers nothing (D73). The terminal is asked
**once, before the screen is taken over** — never from inside the interface, where reading a reply
means owning stdin and raw mode, and turning raw mode off leaves every keystroke echoed and the
interface deaf to all of them (D75). Code blocks are
highlighted by `syntect` — with `two-face`'s grammar set, which is bat's — and the scope it reports is
mapped to nine roles whose colours are the theme's `syntax*` ones, so a theme paints code too and no
colour reaches the renderer from anywhere else (D80, D71).
The context is compacted three ways (D33): synchronously when it would overflow (`[compaction]
auto`, `reserve-tokens`, `keep-recent-tokens`), **in the background** at `background-fraction`
(three quarters by default) while the turn keeps generating (D78), and **when the session has been
left alone** at `idle-fraction` (half) for `idle-seconds` (D77). Both of the last two are `/settings`
rows and a `[compaction]` switch, because each spends a request. A background summary is silent until
it lands, and is discarded if the conversation changed while it was written. Neither feature exists in
pi or opencode, so there is no reference behaviour — `docs/plan.md` is the specification.
A compaction summary is collapsed in the transcript like a tool's output — five lines and a note,
its first when done and its last while it streams, with ctrl+o showing all (D124).
`/thinking` (or shift+tab) changes the reasoning effort for the session (D57), `/name` and
`/session` name and describe it (D55), and escape twice on an empty prompt opens the message tree (D54), where a prompt can be sent again to
make another branch; `/clone` copies the branch into a separate session instead. `/new` (or `/clear`,
the same action under its old name) starts a fresh conversation in this workspace: the transcript, the
spend the status line shows and the context estimate all begin again, the last conversation stays in
`/resume`, and the session id and audit log carry on because they belong to the run (D90). There is no
trust step (D44): nothing the agent runs can write a policy file, so `saddle check` is the whole
review.

## The invariant that must not be broken

One capability representation, in `saddle-cap`, lowered three ways (plan §5):

1. to the interface set a component may import and export, checked before instantiation;
2. to per-call decisions in `saddle-policy`, for every host call and every built-in tool call;
3. to a sandbox spec for child processes at `exec`.

Anything that gives one of the three its authority from a second source is the failure this project
exists to prevent. Two concrete consequences worth restating, because they are the easy mistakes:

- The exec sandbox spec is *derived* from the same grants as the policy decisions. A principal without
  `net` must not reach the network through `curl` in a child process: the child's namespace has no route
  out, and the one door it has leads to the host proxy, which decides every connection against the same
  grants (D79). The ring is load-bearing — a child that could open a socket outside that namespace would
  reach the internet with no decision at all.
- **A tool's path is a location under a root; a sandbox list is not a grant** (D65). The file tools
  decide over `roots` (the workspace, `tmp`, `[roots]`): a path is mapped to the root containing it
  and the grant on that root decides, so a path outside every root is refused as unaddressable.
  `[sandbox] read`/`write` and the include rules are what a **child process** reaches under
  landlock — one list per process, no per-call decision and no record per access — so they must
  never widen `read`, `ls` or `grep`: that would be a second source of authority for a decision.
  `bash` therefore reaches more than the tools (`~` minus the withheld stores), which the prompt
  states in those words. Shared read-only content (`~/.agents`, a toolchain) must not become a root,
  because a root is nameable by any workspace policy. Content the harness chooses — the global
  `AGENTS.md`, an attachment — is host-read with no root at all; a skill is not, since `read` loads
  it (D67).
- Built-in tools are not privileged. None of the eleven is a bypass: each passes through the same
  engine, the same audit log and the same sandbox as a plugin call (D17). `ls`, `find` and `grep`
  decide and log one read of the directory they start from, then filter everything beneath it through
  the same policy without a record per file, so an excluded file is skipped rather than shown
  (`tools/search.rs`).

A **skill** (D64) is instructions and never authority: its `allowed-tools` is ignored, because a
pre-approval written by whoever wrote the skill would be a second source of authority. A skill is
loaded with `read` on its `SKILL.md`, and there is no tool of its own (D67): discovery reads through
the same gate, so a refused skill is never advertised. The audit's `skill` records are what a
session offered and what was loaded, each with a hash.

Deny is the default everywhere: a decision is a lookup over grants, with no ordering, no fallthrough
and no deny rules. A grant may carry a `why`, which `saddle check` shows and the audit log records; it
is optional (D41). Exclusions live inside the grant they narrow (D22).

## Decision log

Decisions are cited by number as **D1** onward (D124 at the time of writing) in `docs/plan.md` §3, and
nearly every module header names the ones it implements, for example:

```rust
//! D20: landlock cannot exclude paths beneath a writable root, so a sandboxed command can write
//! the paths D23 reserves. The host snapshots them before each exec and puts them back after.
```

- Read the cited decisions before editing a module; they usually explain why the obvious
  implementation is wrong.
- `docs/plan.md` §3 is the source of truth for a number. If code cites a decision the plan does not
  define, that is a gap to close by writing the decision, not by dropping the citation.
- New behaviour cites a decision. If none covers it, add a decision to `docs/plan.md` (or a bullet to
  §8 Deferred) in the same change.
- Do not renumber or reuse decision numbers — they are cited across the codebase. Superseding a
  decision means adding a new one that says so.
- §9 Known risks lists the shortcuts that look tempting here. An argv matcher for `exec` is the main
  one: it works until a plugin writes a Makefile.

## Conventions the code follows

- Imports go at the top of the file, and nothing is written inline. Writing `use std::fs;`
  inside a function, or `crate::tui::login::Purpose::Model` in the middle of an expression, is what
  this line forbids: the module gets one import block, and a name used below it is spelled plainly.
  Where two modules bring in the same name, both are imported with an alias that says which is
  which (`Row as PermissionRow`), rather than one of them being qualified where it is used. A test
  module's imports belong at the top of that module. `#[cfg(test)]` on an import is how a
  test-only name stays out of the binary. All crates inherit workspace Clippy denies for
  `absolute_paths` and `items_after_statements`. These catch long qualified paths and imports
  after statements; Clippy does not catch an import before a function's first statement, so the
  module-level rule still applies there too. New crates must set `[lints] workspace = true`.
- Doc comments are lowercase sentences and explain intent, not mechanics. `//!` opens a module with
  its purpose and its decision numbers; `///` covers public items.
- `anyhow` with `bail!`/`Context` in the `saddle` binary crate; typed `thiserror` errors in the
  library crates.
- Messages are lowercase sentences. Host errors are prefixed `saddle:` by `main`; policy denials are
  `ToolOutput::denied`, which renders as `denied: <one sentence explanation>` and is returned to the
  model as an ordinary failed result, never as a host error. Keep both shapes stable — tests and the
  system prompt depend on them.
- No `unwrap()` or `expect()` in non-test code. Both are fine inside `#[cfg(test)] mod tests`.
- Tests: unit tests go in a `#[cfg(test)] mod tests` block at the bottom of the file they test.
  End-to-end tests go in `crates/saddle/tests/`, build against `env!("CARGO_BIN_EXE_saddle")`, and
  run the real binary in a `tempfile::TempDir` workspace with throwaway `XDG_CONFIG_HOME` and
  `XDG_STATE_HOME` — never against the developer's real config or state.
- `crates/saddle/tests/cli.rs` covers the gate through the binary (policy layering, denials,
  reserved paths, the exec sandbox, network grants, the audit chain). `crates/saddle/tests/agent.rs` drives the model loop with
  a mock provider that answers catalogue GETs and model POSTs over a loopback `TcpListener`; those
  tests need `socket()`/`bind()` to be permitted. A bind permission error is environmental; other
  failures still need investigation. They use a throwaway home as well as config and state.
- `crates/saddle-sandbox/tests/confinement.rs` re-executes its own test binary as the probe
  (`SADDLE_SANDBOX_PROBE`). It needs real landlock, and skips or fails on kernels below ABI 3.

## Rules with security consequences

Treat these as load-bearing, not stylistic. If a task appears to require changing one, stop and ask
rather than loosening it.

- **The policy is not in the tree.** The base grants are `[agent]` in
  `$XDG_CONFIG_HOME/saddle/config.toml`, and a workspace's own are `policies/--<path>--.toml` beside it
  (D63), layered on the base (D42, D43); each grants file carries a `workspace = "…"` line and a
  session refuses one that names a different directory, since the filename encoding is lossy. A `saddle.toml` in the repository is not read and a session
  says so once. A workspace file may narrow or widen a base grant: it is the authority for that
  directory, and only a person can write it. `--grant` and `--revoke` change the grants for one run
  without writing anything. Never move authority back into the workspace: landlock cannot carve
  a path out of a granted root, so a policy file there is writable by any command, and the
  snapshot/restore that guarded it (D20) is a race a `setsid` child wins.
- **Reserved paths.** Writes to `.git/config` and anything under `.git/hooks` (including
  `.git/modules/**/hooks`) are refused under every grant (D23), because hooks and settings like
  `core.fsmonitor` run later as the user, outside any sandbox. The policy file was on that list until
  D42 moved it out of the tree; it is now protected by the kernel instead.
- **Symlinks.** A path is decided on both the requested path and the resolved one, including dangling
  links; either denial refuses the call. A link to an absolute path is refused. Resolution goes
  through cap-std (`openat2` + `RESOLVE_BENEATH`), so `..` and symlink escapes are closed.
- **`exec` writes.** Landlock cannot exclude a path beneath a granted root, so `exec` can write
  reserved paths. `reserved::take`/`restore` snapshots them around every command and logs each
  restore as a denial (D20). All access there goes through a cap-std `Dir` so a command that swaps a
  reserved directory for a symlink cannot steer the restore outside the workspace.
- **Exclusions do not bind `exec`.** An `exclude` pattern narrows built-in tools and plugin host
  calls only. A secret that must stay unreadable belongs outside every root.
- **Provider keys never reach a child.** Every variable that may hold a key is stripped from the
  environment a sandboxed command inherits, even if the `exec` grant names it.
- **State and config directories are outside every root**, so neither the agent nor a sandboxed
  command can read the audit log, `auth.json`, earlier transcripts, or the policy
  files that govern them. A root that contains the state or config directory is refused.
- **No workspace is invalid, but some need an allowance** (D60). A workspace that is, or contains,
  the home or config directory puts every credential in reach, so a session there is refused until
  `saddle session allow <id> --workspace <path>` has been run for that session id by a person. The
  allowance outlives the session so it can be picked again: `saddle session list [filter]` shows
  them and `--session` takes an id, a unique prefix or a list number, so an id is never memorised.
  An allowance is a bearer permission, so session ids are random rather than a clock and a pid.
  `saddle session pending` lists the ids waiting, `saddle session revoke <number|prefix>` withdraws
  one, `check` reports the same thing without refusing, and `allow-sensitive = true` in the user's
  config skips the check for a machine with no secrets. A project inside home is fine.
- **A command's file access is an enumerated list, not a fixed set of roots** (D62). The default
  `[[sandbox.include]]` rule walks `~` at startup, withholds what matches `except` (`.ssh`, `.aws`,
  `.local/share/keyrings`, the package-registry and history files, and the rest of the usual
  credential stores), and hands landlock the **list**, which `check` prints under `exec reads`. So a toolchain saddle has never heard of works, and adding one pattern
  takes a directory out of reach for `exec` as well — which a grant-level `exclude` cannot do
  (D22). Two rules make it honest: a directory that **contains** a withheld one is expanded rather
  than granted, because a rule covers everything beneath it; and the state and config directories
  are withheld whatever the pattern says (D16). Rules over the same base are **merged**, so adding
  an `except` narrows; two separate rules would union and defeat each other. A toolchain directory
  is never a `[roots]` entry: a root is nameable by any policy, so `fs-write = [{ root = "cargo" }]`
  in a workspace file could plant a program in `~/.cargo/bin`. `[sandbox] read`/`write` take paths
  for exactly that reason.
- **A `[sandbox]` entry is a claim, not a pattern** (D66). It may be a bare path — a sandboxed
  command's indirect reach, which is all any entry meant before — or a table that adds `tools`, the
  file tools that may name that directory directly. Tool-visible directories are not roots and no
  policy can name one; `check` lists them separately. An entry that names, contains or sits inside
  the state or config directory is refused, and a misspelt tool name is an error because the file
  tools run in saddle, with no kernel behind them. A named write still cannot reach a reserved path.
  `~/.agents` is built in (`read`, `ls`, `find`, `grep`) unless `tools-defaults = false`, and an
  entry for the same directory adds to it instead of replacing it. A symlink inside one of these
  directories is followed, which no root allows, but only to another declared entry: otherwise a
  link would reach the policy (D16).
  `saddle check` shows `tool paths`; `saddle tool read '{"path":"~/.agents/…"}'` is the by-hand test.
- **`exec` is Linux-only** and needs landlock ABI 3 as a hard requirement. A command now has a network
  of its own (D79): a user and network namespace with no route out, a relay on its loopback, and a proxy
  in this process that decides every connection with the same engine, audit log and `net` grants as
  `fetch`. The name is read from the bytes a client sends (server name extension, `Host` header,
  `CONNECT` line) and a connection that names no host is refused, because a grant names a host and an
  address is not a name. `AF_UNIX`, `AF_PACKET`, io_uring and `setns` stay refused for the command —
  a network namespace does not confine `docker.sock`. If the namespace cannot be made the command gets
  no network at all, which is the pre-D79 behaviour and is strictly narrower. Do not add hosts or paths
  to the sandbox allowlist to make a test pass, and do not let the relay learn a protocol: it copies
  bytes so that a new protocol needs no new code there.
- **Every allow and every deny is audited** (D10) with the resolved arguments: a path as
  `root:relpath`, a command as `argv` plus `cwd`, a request as method and a URL whose query values
  are replaced with `key=…`. D24 also lists byte count and content hash for a write, so check both
  the spec and `Request::args` when adding a host call. `saddle audit verify` must keep passing over
  the record shape it produces.

## When you cannot do something

If a task needs authority the policy does not grant, say which grant it needs and stop. Do not look
for a way around the gate, and do not try to write the policy that governs you — it is outside every
root, so the kernel refuses it, and it is a human's file to change (D42). The harness cannot
renegotiate its own policy.
