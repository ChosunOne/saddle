# WASI tools and plugins

Saddle's twelve ordinary tools run shipped WASI components (D146–D147), including `config_help`,
which returns the configuration reference compiled from `docs/settings.md` and imports only the
budgets snapshot, so its profile delegates no scope (D171). There is no native fallback or
native/plugin implementation picker, and `tool_search` is the one name that remains a host MCP
catalogue operation. Filesystem effects, path resolution, permissions, audit, sandbox execution,
HTTP/credentials and branch-only transcript SQL remain privileged core services. Guests own schemas,
workflows, parsing, matching/traversal, pagination, minimal diffs and presentation.

## Using the shipped tools

No separate plugin installation, componentizer command or files under `target/*-plugin` are needed
at runtime. Components and their digests ship inside the executable. Inspect them with:

```sh
saddle tools
saddle tools --config     # prints recommended delegation; does not edit existing config
saddle check              # configured profiles, parent reach, budgets and diagnostics
saddle plugin list        # loaded components and any refusal
```

### Existing-configuration migration

An existing config is **not silently changed** to grant components the agent's reach. Add the desired
entries from `saddle tools --config` to the protected user `config.toml`, then `/reload`. Missing
entries/scopes mean none, and an unavailable tool returns an audited denial, never native execution.
First-run defaults write this explicit table alongside the existing `[agent]` defaults:

```toml
[standard-tools]
read = ["fs-read", "tool-read"]
ls = ["fs-read", "tool-read"]
find = ["fs-read", "tool-read"]
grep = ["fs-read", "tool-read"]
write = ["fs-read", "fs-write", "tool-read", "tool-write"]
edit = ["fs-read", "fs-write", "tool-read", "tool-write"]
patch = ["fs-read", "fs-write", "tool-read", "tool-write"]
bash = ["exec", "fs-read", "fs-write", "net"]
run_binary = ["run-binary", "net"]
tasks = ["exec"]
fetch = ["net"]
websearch = ["net"]
session_query = ["transcript-read"]
config_help = []                 # the reference: nothing to delegate
```

Presence authorizes registration of that fixed shipped component only. Each listed operation scope
selects the agent's **current** resources, not new resources: filesystem roots/exclusions, exec env
and timeout, network hosts/schemes/ports and private-address exemptions, managed-path roles, or
transcript reach. The manifest ceiling and D126 delegation check independently bound the result.
For example `write = ["fs-write"]` does not inherit read permission even if the agent holds it;
ordinary write will refuse its preimage read. A missing parent scope selects no resources; a component
whose imports cannot link is unavailable and is not advertised to the model. Thus fetch/search need
actual delegated network reach, not simply a table entry saying `net`.

Managed directory roles remain those in D66's protected `[sandbox]` declarations. Readers/listing/
search receive only their matching role; mutation preimages use the `read` role, and mutation writes
use their own `write`, `edit` or `patch` role. Existing-file mutation of an external directory therefore
needs both a configured read role and the corresponding write role. This is not another directory
allowlist and never converts a directory into a policy root. Bare sandbox paths and include rules
remain child-process reach only.

`standard-*` plugin identities are reserved for the shipped components, not configurable third-party
replacements. Profiles enter the policy hash; reload rebuilds the catalogue and bounded principal
views without resetting spending or changing artifact digests. Changing a profile does not change
`[agent]`, and granting the agent more reach does not lend an omitted operation scope to a tool.

## Command filesystem defaults (D160)

Commands inherit granted filesystem roots and protected sandbox paths. The implicit read includes
are limited to `~/.cargo`, `~/.rustup` and `~/.config`—Saddle does not walk all of home by default.
Cargo credential files, known credential/browser stores under `.config`, and protected Saddle
config/state remain withheld. Other files in these directories can still contain sensitive data;
this is a directory selection, not a guarantee about their contents. Implicit symlink bases are
skipped. These command-only includes do not grant direct file-tool access or additional writes.

Use protected `[sandbox] read` entries for other command directories, and `tools` roles when direct
file-tool access is also wanted. `include-defaults = false` turns off the implicit includes.
Explicit paths/include rules remain in force, including any user-written whole-home rule. With
defaults enabled, an explicit home rule keeps the old merged credential exclusions; overlapping
implicit child rules are omitted so they cannot undo an exclusion. Existing configs with no flag
receive the narrower defaults on startup/reload; no protected file is automatically rewritten.
`saddle check` and the model prompt report the effective command read paths.

## Named launches (D184)

`run_binary` invokes a protected profile with literal `argv`; it neither imports `exec` nor
inherits its sandbox, environment, or task controls. The exact profile name must be granted to the
agent and delegated to the component. A third-party component uses the `saddle:host/run-binary`
interface with that scope in its manifest and exact named grants in `[plugin.<id>]`. Network, if
used, is independently delegated and narrowed by the profile. See [configuration](settings.md#launch-profiles).

## Background shell work

`bash` runs the command; the separate `tasks` tool reads, stops and waits on it (D153, D182). A
launch is `bash` with `background: true`:

```json
{"command":"cargo nextest run", "background":true, "timeout_secs":300}
```

It returns a task id promptly. Continue working with file tools, then inspect or cancel the task with
`tasks` — every call in the next block is a `tasks` call:

```json
{"action":"status", "task_id":"task-<id>"}
{"action":"list"}
{"action":"cancel", "task_id":"task-<id>"}
```

The completion of a task started here **resumes the agent**, so the work it began does not have to be
polled: `background: true, "wake": false` declines that, and `tasks`' `action: "wait"` arms a task
later (D174):

```json
{"action":"wait", "task_id":"task-<id>"}
```

`wait` answers at once with the task as it stands — it never blocks the call, because the resumption
belongs to the session. A one-shot `saddle run` waits for a task it will be woken by instead of
exiting and killing it; in an interactive session the wake is the follow-up turn D159 describes.
`/tasks` marks a task that will resume the agent with `wake`.

Status returns a bounded snapshot of available stdout/stderr, exit status and timeout/cancellation
state. Repeated polls return the captured prefix again, not an ever-growing log, and a finished task
keeps answering with its exit status until the `exec-tasks` retention budget evicts it; `list` shows
each record as running, finished (with its exit code), cancelled or timed out (D179). Cancellation is
requested promptly, and the cancel call waits — up to a second — for the process group to be killed
and reaped, so its own reply says how the task ended; another call's status can still see it running
until then (D180). A task belongs to the principal that started it: a third-party plugin cannot read
or cancel another's task, nor arm a wake on it, while the shipped `standard-*` tools act for the agent,
so `tasks` sees what `bash` started (D182). A start's timeout bounds the **task lifetime**; a timeout
on a status call only bounds that call.

**One shell command may be active per workspace.** Foreground and background commands, including
other Saddle processes in that workspace, share an exclusive reservation lock. A second start
returns a busy error; it is not queued. This restriction prevents competing Git config/hook
snapshots. File tools remain available. Build/test tasks work well; a dev server would occupy the
shell slot until stopped. Completed filesystem effects are not rolled back.

`/tasks` lists tasks for the person; `/tasks cancel <id>` stops one. These commands also work in
line mode and are handled between turns in the TUI. Running jobs keep their originating tool block on the theme's pending background with a spinner,
even after the launch call returns. On completion the block shows the host's terminal state and
success/error background (D159).

Completion notices enter an active turn at its next safe request point. If the agent's turn has
already ended, an interactive TUI/line-mode session automatically starts one follow-up turn for
unconsumed completions from model tool calls. This spends a normal model request; running jobs and
repeated idle ticks do not. Hook-only jobs without an originating model tool invocation do not
trigger automatic spending. A queued human prompt takes priority. Notices contain only host-owned
identity/state, not untrusted command output. The agent must use the owning tool to inspect output.
A task whose launch declined with `wake: false` wakes nothing until it is armed with
`action: "wait"` (D174); the person still gets the notice either way.

Tasks stop on session exit or any successful permission/settings reload. Reload drains cleanup and
audit under the old grants before replacing them. Task ids/outputs are not persisted across restarts;
resuming a transcript never restarts its old commands. Standalone `saddle tool bash` calls close
their session immediately, so they cannot leave a task running. Ctrl+C cancelling a model request
alone is not a request to cancel every task; use `/tasks cancel` or exit the session.

No extra delegation scope is needed: existing explicit bash `exec` delegation applies. SDK users
can import `saddle:host/exec-tasks` under their existing `exec` ceiling/reach; the interface never
lends a sibling's permissions, and its `set-wake` effect is authorized, audited and isolated exactly
as inspect and cancel are (D174). `[tool-limits] exec-tasks` defaults to 16 retained task records;
completed records are evicted oldest-first when full. Capture and presentation use the existing
execution budgets. Normal sandbox/no-network fallback and escaped-process-group limitations apply.
New proxy decisions are serviced during model I/O and idle waits, but a blocking native host
operation can delay them. There is no alternate network decision engine on the monitor thread.

## Per-call timeouts

Every tool accepts optional `timeout_secs`, a positive integer number of seconds (D152):

```json
{"path":"src/main.rs", "timeout_secs":5}
```

This is a **total wall-clock deadline**, including component initialization and host I/O waits.
Omit it to retain existing behavior. Protected CPU/fuel, host-wait, command and MCP server limits
still apply and may stop the call sooner; this option never raises them. Zero, negative, fractional,
null, string and overflowing values are rejected before effects.

The host adds the optional execution field to model-facing schemas and removes it from forwarded
arguments unless the tool's original schema declares it. Existing declared `timeout_secs` fields
(such as bash's) are preserved and also interpreted as the host deadline. New plugin/MCP schemas
should leave this execution-option name to the host. No guest rebuild or SDK change is needed.
`saddle tool`, `saddle plugin call` and `saddle mcp call` accept it in their JSON arguments too.

At expiry, guest execution stops, command process groups are killed and a failed result reports the
timeout. Cleanup and audit finish before returning; completed mutations are **not rolled back**. A local MCP server that times out is stopped; restart it with `/mcp restart`.
Remote MCP requests stop waiting, but remote work may continue: there is no guarantee of remote
cancellation. Non-preemptible kernel filesystem operations and children escaping a process group
remain the existing limitations. Timeout does not turn a fatal host/audit failure into a tool error.

## Build and delivery

Developers/package builders need Rust's `wasm32-unknown-unknown` target. Normal `cargo build` and
`cargo nextest run` build the guest sources automatically using the workspace lockfile in a separate
Cargo target directory, componentize them and precompile target-specific Wasmtime artifacts. Cargo
tracks SDK, WIT, tools, limits and ignore sources. End users need neither the Rust guest toolchain
nor a repository checkout. Standard-component tests consume the packaged bytes, not manually built
files under `target`. The SDK reader compatibility fixture is built and componentized alongside
them too, so the full test suite needs no manual fixture preparation or ignored-test opt-in.

The bundle contains raw WASM, component digests, manifests and matching precompiled bytes generated
together by the trusted package compiler. Package/runtime engines use the same explicit baseline
ISA and pinned Wasmtime line. Deserialization of arbitrary native code is unsafe; the embedded
loader path is used only for immutable build-produced bytes inside the executable, followed by
actual type/scope/signature checks and grant-shaped pre-linking before any initialization. There is
no runtime-writable serialized-code cache or native-library plugin path. An installed third-party
raw component may reuse the immutable packaged compilation only when both its digest and exact raw
bytes match; its own identity, explicit grants, ceiling checks and namespace remain unchanged.
Other third-party components compile from checked raw WASM as before.

Guest execution uses fresh bounded stores/instances with fuel, epochs and cancellation, empty args/
environment/streams, no preopens and no ambient sockets. SDK availability never implies permission
to perform an operation. The typed D150 interface catalogue supplies load-time requirements and
implementation availability; the loader binds checked identities without repeating permission tables.
Canonical WIT and actual adapter coverage are tested against that catalogue. Deferred hooks and
unknown imports still fail before initialization, even when unused. Each SDK effect is decided and audited by the session's one engine under
the host-assigned principal and call id. Fatal host/audit failures stop the session, not merely a tool.

## Mutation behavior and diffs

Ordinary `write`, `edit` and patch updates/deletions read and guard preimages. The additive
`fs-preimage` SDK service returns a bounded optional byte snapshot after ordinary requested/resolved
read checks. **Only actual not-found means absence**; a refusal, unreadable file or other I/O error
is never interpreted as a new file. Write requires a readable UTF-8 preimage or positively observed
absence and submits a hash or missing precondition. Updates/deletions require an existing UTF-8
preimage. A new patch addition or move destination must be absent and cannot overwrite an existing
file. Patch additions can therefore safely create files with write-only authority; this is not a
blind existing-file overwrite. Standard schemas offer no blind overwrite/delete mode.

The low-level SDK `any` expectation remains for binary compatibility with explicitly configured
third-party plugins. It is not used by ordinary standard mutations. Hash expectations independently
require read permission in the host, so they cannot become an oracle for write-only principals.

Diffs use the native-style Myers line algorithm: minimal hunks, three context lines, old line numbers
for removals, new numbers otherwise, and gap markers between distant changes. Multi-file output labels
the paths. Presentation is accepted and staged **before effects**; a diff refusal leaves files untouched.
It is published only on successful completion and retained through saving/replay. Guest fuel, memory
and configurable presentation bytes bound diff computation/output; no unsupported native clock is
needed. Very large diffs can still be cut at the configured presentation budget.

Mutation batches preflight all permissions, reservations, target conflicts and preconditions before
staging/publishing. Replacement is atomic per entry, not across the whole batch. I/O publication
failures can partially apply and say so. Empty created parents may remain. Final precondition checks
are not a race-free filesystem compare-and-swap against independent writers. Cancellation/traps do
not undo committed effects and are reported accordingly. No command- or plugin-supplied path can
steer publication through a swapped symlink or multiply-linked inode outside its granted reach.

## Other tool contracts

- **read:** default UTF-8 text, explicit `format: "image"` for PNG/JPEG/GIF/WebP. No automatic image
  detection or binary fallback. Text pages preserve whole-file bytes when they fit, otherwise return
  line/continuation notes and bounded long-line prefixes. Entire scans validate UTF-8, not just the
  displayed page. Deep offsets are scanned, not guessed. Images use bounded ranges and real provider/
  UI image attachments, not base64 in model text.
- **ls/find/grep:** host-filtered lexical metadata pages; guest sorting, traversal, shared ignore
  syntax and glob/regex matching. They skip `.git`, symlinks, special files and ignored paths.
  Unreadable child directories are skipped with a note; an unreadable requested start fails. Invalid
  UTF-8 names are counted as unrepresentable rather than lossy-renamed into ambiguous targets.
  Workspace-contained results are relative; other roots/external directories yield usable absolute
  paths via the permission-gated `label-root` operation, never opaque routing identifiers.
- **bash:** guest-owned `sh -c`, schema and formatting over the delegated core exec service. Sandbox,
  filtered environment, reservation restore, network proxy, capture, cancellation and process-group
  cleanup use the actual bounded caller. Its filesystem/net view can be narrower than the agent's.
  Network namespace failure gives no network, never unrestricted sockets.
- **fetch/websearch:** guest schemas/parsing/presentation over core HTTP or fixed protected search
  transports. Every redirect is independently authorized and cross-origin redirects strip supplied
  headers. Credentials stay in core and only reach the configured provider/fixed Exa endpoints.
  Search snapshots are principal-scoped, not shared with siblings; expansion rechecks current endpoint
  reach and reload evicts data above narrowed cache budgets.
- **session_query:** read-only SQL over one conversation's branch snapshot, including pre-compaction
  messages, request usage and prose reasoning, with a `sessions` view naming the conversations of this
  workspace. No saved-database handle, writes or arbitrary SQL functions. Parent and delegate both
  need transcript-read, and a delegated grant is narrowed to the parent's: a plugin reads another
  conversation only when the agent's own grant covers the workspace (D163). Standalone calls report
  no active conversation unless they name one.

  The grant names what it covers: `transcript-read = {}` is every conversation of this workspace (the
  default, and what a bare grant always means now), `transcript-read = { sessions = "self" }` is only
  the conversation in hand. The tool takes an optional `session` (an id or unique prefix from
  `sessions`); without it the query is against the conversation in hand, as before. `saddle check`
  and `/permissions` print which reach is in force, and an audited decision records the conversation
  it was for and whether that was another one.

Numeric bounds in these contracts are protected runtime settings, not component build variants.
UTF-8 WIT strings, supported image formats, partial multi-file effects, process-group limitations
(`setsid`) and inability to immediately interrupt every blocking resolver/filesystem read are separate
semantic/platform limits. Fuel cannot preempt synchronous native I/O; transport deadlines bound
network waiting. Directory pages/range reads are not snapshots against concurrent filesystem changes.

## Result provenance and presentation

Ordinary tool names run bounded standard principals; third-party names are
`plugin__<id>__<tool>`. Descriptions and model-visible outputs retain D25's random delimiter framing.
Host-owned provenance records principal, scope, call id, version and source component digest; guest
text cannot provide or forge it. Saved results keep their original metadata/framing after reload,
disable, resume or branching. Human diffs are never model text.

Human line/TUI rendering removes only a matching outer frame authenticated by host provenance.
It does not mutate stored/provider content. Thus an ordinary tool's CLI text and resumed tool cards
do not show framing noise, while `run --json` result events retain framed `content`, `provenance` and
separate `diff` for programs. A third-party name keeps the frame on purpose: `saddle plugin call` and
`saddle tool plugin__<id>__<tool>` print exactly what the model would receive, so what a person is
debugging is the same bytes the model reads.

## Explicit third-party plugins

Third-party installation remains separate from enablement and grants:

```sh
cargo build -p saddle-tools --example read --target wasm32-unknown-unknown --release
# Componentize the module using the canonical example:
cargo run -p saddle-loader --example componentize -- module.wasm reader.component.wasm
saddle plugin install /path/to/plugin.toml
```

A manifest identifies a raw component relative to itself and declares a scope ceiling:

```toml
id = "reader"
version = "0.1.0"
component = "reader.component.wasm"
scopes = ["tool", "fs-read", "tool-read"]
```

Installation copies bounded, no-follow checked artifacts to protected `$XDG_CONFIG_HOME/saddle/plugins`;
it neither enables them nor writes grants. It prints the pin for the user to configure:

```toml
[plugins.reader]
enabled = true
manifest = "reader/plugin.toml"
sha256 = "<printed component SHA-256>"

[plugin.reader]
tool = {}
fs-read = [{ root = "workspace" }]
tool-read = { tools = ["read"] } # only existing D66 directory reach, bounded by the parent
```

For a custom writer/update tool add the corresponding explicit fs-write/tool-write grants and manifest
ceilings; read access is independent. Omitted grants inherit nothing. The ceiling chain is agent
current reach → manifest scopes → explicit plugin grants. Parent/child exclusions both apply; a sibling
or registration grant cannot supply missing operational reach. No repository auto-discovery, automatic
updates or plugin-to-MCP adapter exists.

```sh
saddle plugin list
saddle plugin call reader '{"path":"src/main.rs"}'
saddle tool plugin__reader__read '{"path":"src/main.rs"}'
```

SDK worlds and generated bindings share `crates/saddle-wit/wit`. The historical world name
`read-tool` remains for compatibility. Additive filesystem, mutation, preimage, presentation, exec,
HTTP/search and transcript interfaces are linked only under eligible grants and typed before init.
Filesystem imports may link with any eligible filesystem scope; each read/write still independently
requires its own per-resource permission. The old `tool-paths` interface remains for D138 binary
compatibility; standard code uses one ordinary filesystem SDK for roots and D66 paths, with no Cargo
permission features. Raw sockets, ambient outgoing HTTP, synchronous WASI poll and arbitrary native
plugins remain unavailable even if a component imports them unused.

## Harness hooks and custom events (D155)

These are **different typed channels**, not a shared event bus distinguished by topic prefixes.
A plugin cannot emit a harness event. A custom event named `turn-start` remains a custom event.
Hook-only plugins do not need a tool export or a `tool` grant, and do not advertise an agent tool.
They use the same installation, explicit enablement, digest pin and bounded instances as tool plugins.

### Harness registration

Implement `saddle_sdk::harness::Hooks` and use its `export_harness_hooks!` macro:

```rust
use saddle_sdk::harness::{self as bindings, Event, Hooks, Kind, export_harness_hooks};
use saddle_sdk::events::emit;

struct Extension;
impl Hooks for Extension {
    fn subscriptions() -> Vec<Kind> {
        vec![Kind::TurnStart, Kind::ToolEnd]
    }
    fn observe(event: Event) -> Result<(), String> {
        // This publishes to the custom channel, never back into harness hooks.
        emit("work.observed", &event.data)
    }
}
export_harness_hooks!(Extension with_types_in bindings);
```

The closed harness enum is `TurnStart`, `TurnEnd`, `ToolStart`, `ToolEnd`. Host-produced `data` is
JSON: turn start carries `prompt`; turn end carries `end`; tool start carries `tool` and `arguments`;
tool end carries `tool`, `ok`, `content` and `provenance`. There are no provider secrets, opaque
reasoning payloads or config/state handles in these records. Host origin authenticates the lifecycle
event, not its text: user prompts, model arguments and plugin tool output remain untrusted. Oversized payloads are audited and not
delivered, not silently truncated. Tool events surround normal shipped/plugin/MCP calls; turn events
surround agent turns. Fatal host/audit failures do not run another handler.

The observer export requires explicit `transcript-read` in the manifest ceiling and plugin grants,
bounded by the agent's transcript reach (which conversations that covers, D163). This example also needs `event-publish` because it emits a
custom event. An observer is not a veto: its error is audited but cannot deny or rewrite a pending
call. It cannot inject a model message by returning text.

### Custom registration and publication

Implement `saddle_sdk::events::Handler` and use the **separate** `export_plugin_events!` macro:

```rust
use saddle_sdk::events::{self as bindings, Event, Handler, Subscription, export_plugin_events};

struct Listener;
impl Handler for Listener {
    fn subscriptions() -> Vec<Subscription> {
        vec![Subscription { publisher: "my-extension".into(), name: "work.observed".into() }]
    }
    fn receive(event: Event) -> Result<(), String> {
        // event.publisher is assigned by the host. event.data is untrusted input.
        // Any SDK operation here uses THIS handler's grants, not the publisher's.
        Ok(())
    }
}
export_plugin_events!(Listener with_types_in bindings);
```

Subscriptions match an exact `(publisher plugin id, name)` pair; no wildcards or automatic harness
subscription. `saddle_sdk::events::emit(name, data)` needs `event-publish`; receiving needs
`event-subscribe`. A custom-only listener needs neither `transcript-read` nor `event-publish`.
A plugin implementing both traits uses both export macros with their respective binding modules;
`crates/saddle-sdk/examples/hooks.rs` is a compiled example, and `event_listener.rs` is custom-only.
The ordinary tool export remains independent and can coexist with these exports.

Both event capabilities require explicit parent and delegated grants, for example:

```toml
[agent]
event-publish = {}
event-subscribe = {}

[plugin.my-extension]
event-publish = {}

[plugin.my-listener]
event-subscribe = {}
```

Include the corresponding scope names in each manifest. `event-subscribe = {}` grants access to the
custom channel; registrations select interest and do not confer additional authority. Publishing can
only identify the current plugin, never the harness or a sibling. Payload fields cannot override the
channel or publisher. Treat payloads as untrusted, and validate them before acting on your own file,
network or execution permissions. No event grants lend those permissions.

### Delivery and limits

Events are session-owned, not persistent plugin instances. Registration calls cannot emit events.
Custom events queue during an invocation and drain at tool/turn safe points after harness handlers;
dispatch is FIFO with deterministic recipient ordering, not recursive callbacks. Handler-emitted
messages share the current drain's delivery/time budget, so cycles cannot reset the ceiling. See the
`event-*` settings below. Full queues and oversized publications fail before enqueueing. Exhaustion
drops remaining deliveries with an audit record. Handler failures do not roll back committed effects.

Reload clears pending events and rebuilds registrations under the new grants without repinning or
rebuilding components. Session close discards events. Handler payloads and errors are never injected
into model context or trusted notices. Publication/delivery audits record identity, channel, event name,
content bytes/hash, recipient and call id, not payload bodies. The public budgets SDK is available to
hook-only components without an unrelated tool grant; it is metadata, not operational authority.

## Typed extension capabilities (D156)

These are separate APIs, not permissions implied by an observer or custom event registration.
`crates/saddle-sdk/examples/features.rs` is a compiled example combining all five with a tool export.
A hook-only component may export veto/UI without registering an agent tool. Keep each scope in the
manifest ceiling and explicitly delegate it from the parent. For example, these protected config
entries enable the five capabilities for an installed plugin named `features`:

```toml
[agent]
veto = {}
context-contribute = {}
persist = {}
ui = {}
model-invoke = { tokens-per-session = 200000, cost-per-session = 1.0 }

[plugin.features]
veto = {}
context-contribute = {}
persist = {}
ui = {}
model-invoke = { tokens-per-session = 100000, cost-per-session = 0.5 }
```

These snippets extend existing tables; they do not replace ordinary file/network grants. The optional
cost ceiling is in the provider's configured price units (normally USD), and unknown pricing refuses
admission when a cost ceiling exists. Subscription quota is not a known API price.

### Veto pending tools

Implement `saddle_sdk::veto::Veto::before_tool(PendingToolCall) -> Decision` and export it with
`export_veto!(Extension with_types_in veto_bindings)` using `saddle_sdk::veto as veto_bindings`.
The pending call contains its name and immutable JSON arguments. Return `Decision::Continue` to
abstain or `Decision::Deny(reason)` to refuse. Continue grants no authority: the usual tool policy
still decides every resource. Traps, timeouts and oversized replies fail closed. Reasons are untrusted,
framed plugin content, not host advice. Veto callbacks use their own delegated grants.

### Append notes, diagnostics and corrections

`saddle_sdk::context::note(text)` returns a host-assigned contribution id. `diagnostic(Diagnostic)`
accepts a known root, relative path, positive line range, severity and message. It does not read the
named file or grant filesystem access. Use `append(&Contribution { body, corrects: Some(id) })` for a
correction to your own earlier contribution on the active branch. The earlier item stays intact.

The host saves separate provenance-bearing items and sends framed text at message-safe points,
never as system instructions or between tool calls and their results. Standalone tool calls without
an active conversation refuse contributions. Resume/fork rebuilds correction ownership from that
branch, not another conversation. Size and pending-count bounds apply.

### Private persistence

`saddle_sdk::persist::{get, put, remove, compare_exchange}` operates on byte values. Keys are opaque
strings, not paths. `apply(&[Change { key, value, expected }])` checks every `Expectation::{Any,
Missing, Value(bytes)}` and commits the whole batch atomically; a failed precondition returns `false`
without writes. `value: None` deletes. Duplicate keys are refused. SQLite namespaces are keyed by
canonical workspace and plugin identity, so siblings and other workspaces cannot read them. Storage
survives restart/reload; revocation denies access without deleting data. Quotas bound keys, values,
batches and total bytes. After a quota reduction, shrinking/deleting data remains possible.

### Explicit model invocation

`saddle_sdk::model::invoke(&Request { instructions, messages, max_output_tokens })` uses the session's
selected provider and host-managed credentials. Messages have `Role::User` or `Role::Assistant`.
The independent request receives only this input, not the main conversation; it has no tools.
`Reply` contains text, prose reasoning, model name and usage (including optional cost), never keys or
opaque reasoning payloads. Returned tool calls are refused and never executed. A standalone call
without a session provider refuses instead of selecting a provider itself.

Admission reserves conservative input-byte/framing and output-token estimates against **both** the
plugin and shared parent session budgets, including optional spend ceilings. Successful metered replies
reconcile with reported usage; failures and unmetered replies retain reservations. Provider overruns
are charged and block later requests, not claimed to undo remote billing. Reload does not reset
spending. Explicit requests appear as `plugin` request usage on the saved branch. No automatic retry
can duplicate a plugin request; deadlines stop local waiting, with remote cancellation best effort.

### Labelled read-only UI panels

Implement `saddle_sdk::ui::Ui::render() -> Result<Panel, String>` and use its `export_ui!` macro with
that module's binding alias. Panels contain `Widget::{Text, Items, KeyValue, Progress}`; text spans use
closed `Style::{Text, Muted, Success, Warning, Error, Emphasis}` values. Progress requires a positive
total and completed <= total. The host strips terminal controls, bounds bytes/widgets/rows and labels
each panel with the authenticated plugin identity. There are no raw terminal handles, arbitrary
colours, input widgets or host approval actions.

Rendering runs at turn start (also initial standalone tool dispatch), not inside the TUI drawing
thread. It allows authorized reads, including private KV reads, but refuses mutations, execution,
networking, event publication, contributions and model spending. Ordinary authorized callbacks may
`ui::publish(&panel)` or `ui::clear()` their own frame. Frames update at safe points in TUI and
line/JSON events; they are not model context or saved conversation items. Reload clears panels and
rechecks current grants. Failed rendering retains the previous frame.

## Workspace overrides (D158)

There is one protected override file per workspace, shared by all its sessions:
`$XDG_CONFIG_HOME/saddle/policies/--<workspace-path>--.toml`. `saddle check` names the exact file.
It holds both `[agent]`/`[plugin]` grant overrides and `[sandbox]` path overrides on top of the
base in `config.toml`. For example, add this to the existing workspace file:

```toml
workspace = "/home/me/source/project" # the exact canonical workspace path

[agent]
exec = {}

[sandbox]
write = ["+~/.dylint_drivers"]
```

Edit it yourself and use `/reload`. It applies to every session in this workspace, not other
workspaces. The separate `session-overrides/<session-id>.toml` source introduced by D157 has been
removed and is not read. Move any desired entries into the workspace file yourself; no automatic
migration changes authority. Unknown top-level sections or sandbox fields are rejected rather than
silently ignored. `[sandbox]` permits only `read` and `write`, not include rules or other settings.

List semantics are:

| value | meaning |
| --- | --- |
| omitted | inherit the global list |
| `["+~/cache", "-~/old-cache"]` | inherit, then add/remove paths in order |
| `["~/cache"]` | replace the inherited list |
| `[]` | replace with an empty list |

Every path in a modifying list needs an operator. Mixing plain and prefixed entries is invalid.
Prefixes in **global config** are invalid too: the global file has no inherited list. Errors refuse
startup; a failed `/reload` reports the invalid configuration and keeps the entire previous setup.
A table entry can put the operator on `path`, for example
`read = [{ path = "+~/notes", tools = ["read"] }]`; ordinary tool-name checks and delegated D66 roles
still apply. Symlinked override files/directories are refused so repository content cannot become
configuration authority. State/config paths and their aliases remain forbidden.

Subtraction removes matching explicit entries in that list; it is not a deny rule. Removing a child
path cannot carve it out of another parent-directory claim, a default include rule or a policy root
grant. These overrides do not change workspace/tmp grants, roots, credentials, plugin delegation or
other global settings. A bare path remains command-only. Directories must exist before the kernel
can grant access: create a new cache directory in your own terminal first, rather than granting its
parent. Session-start/reload audits retain the source path/hash and effective explicit command lists.

## Runtime resource budgets (D145)

Set `[tool-limits]` in protected config, then `/reload`; `saddle check` prints the resolved profile.
Defaults live once in `saddle-limits`, shared by host/loader/guests. The immutable invocation profile
is public numeric metadata only; the host independently enforces it if a guest ignores it. Size/count
settings fit the 32-bit guest ABI, fuel a 64-bit counter. Positive values are required except optional
redirect/image/report/quote counts, which may be zero. Backend/platform maxima still apply.

Different stages have distinct limits. A large ordinary write needs sufficient serialized JSON input
space as well as mutation space and guest memory. JSON escaping counts. A large presentation also
needs enough plugin output space. For example:

```toml
[tool-limits]
plugin-input-bytes = 4194304
plugin-memory-bytes = 67108864
mutation-bytes = 8388608
mutation-changes = 128
exec-capture-bytes = 1048576
exec-present-bytes = 262144
plugin-output-bytes = 1048576
presentation-bytes = 786432
```

All available keys and defaults (bytes unless a key names another unit):

```toml
[tool-limits]
plugin-component-bytes = 4194304
plugin-manifest-bytes = 16384
plugin-count = 16
contribution-bytes = 16384
contribution-count = 32
persist-key-bytes = 256
persist-value-bytes = 65536
persist-keys = 1024
persist-total-bytes = 4194304
persist-changes = 32
model-input-bytes = 65536
model-output-tokens = 4096
model-reply-bytes = 65536
ui-bytes = 16384
ui-widgets = 32
ui-rows = 12
event-queue = 64
event-deliveries = 128
event-bytes = 16384
event-name-bytes = 128
event-subscriptions = 64
event-seconds = 30
plugin-description-chars = 1000
plugin-schema-bytes = 16384
plugin-version-bytes = 64
plugin-stack-bytes = 524288
plugin-table-elements = 10000
plugin-instances = 32
plugin-memories = 4
plugin-tables = 8
plugin-resources = 128
plugin-random-bytes = 65536
plugin-memory-bytes = 16777216
plugin-fuel = 100000000
plugin-timeout-ms = 2000
plugin-input-bytes = 65536
plugin-output-bytes = 65536
host-wait-seconds = 300
host-calls = 16
metadata-bytes = 65536
path-bytes = 4096
directory-name-bytes = 1024
url-bytes = 8192
header-value-bytes = 8192
exec-arguments = 128
exec-argument-bytes = 65536
range-bytes = 65536
directory-page-entries = 128
directory-scan-entries = 100000
mutation-bytes = 1048576
mutation-changes = 32
diff-bytes = 65536
diff-present-bytes = 61440
native-diff-input-bytes = 2097152
image-bytes = 5242880
image-count = 1
image-name-bytes = 256
request-image-bytes = 33554432
read-page-bytes = 51200
read-page-lines = 2000
read-scan-bytes = 67108864
list-entries = 500
search-visited = 100000
search-pending = 2048
search-path-bytes = 1048576
search-file-bytes = 4194304
search-ignore-file-bytes = 65536
search-ignore-bytes = 131072
find-results = 1000
grep-matches = 200
grep-line-bytes = 300
search-regex-bytes = 1048576
text-bytes = 51200
exec-capture-bytes = 262144
exec-present-bytes = 24576
exec-report-paths = 16
exec-report-denials = 8
exec-seconds = 300
exec-tasks = 16
presentation-bytes = 61440
http-capture-bytes = 102400
network-seconds = 30
http-redirects = 5
http-headers = 64
http-header-bytes = 65536
search-response-bytes = 1048576
web-query-bytes = 16384
web-results = 100
web-default-results = 5
web-quote-chars = 1000
web-quotes = 3
search-cache-entries = 17
query-bytes = 16384
query-result-bytes = 51200
query-rows = 100
query-value-bytes = 1048576
query-columns = 32
query-expr-depth = 100
query-program-ops = 100000
query-steps = 1000000
```


`image-bytes` bounds each image and aggregate emitted image bytes; `request-image-bytes` bounds the
pictures one model request carries, oldest replaced by a note when a long conversation would exceed
it (D162). SQL instruction work is checked
at 1000-instruction progress callbacks, not an exact single-instruction boundary. Native thread stack
includes a host allowance beyond the Wasm stack; library embedders must reserve adequate native stack.
Unknown future SDK metadata keys are ignored for forward compatibility; protected config still rejects
typos. Older third-party artifacts may retain their own internal limits. Changing resource settings
never widens manifest ceilings, delegated grants, exec env/timeouts, exclusions/reservations, credential
handling, private-network checks or SQL authorizers.

## Validation and remaining boundaries

```sh
cargo build
cargo nextest run --no-fail-fast
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
```

All standard-component tests are active and consume packaged bytes automatically. They cover ordinary
name dispatch, minimal edit/patch diffs and save/model separation, no blind preimage fallback, refusal
before effects, explicit enablement/delegation, grant/budget reload without repinning, managed paths,
symlinks/reservations, delegated execution/networking, credential isolation, sibling caches, images and
branch SQL. TUI rendering/replay retains human diffs and strips only authenticated display frames.
The SDK reader compatibility fixture is built by the same package build and its round-trip test runs
in the default suite, so nothing needs a manual component build or an ignored-test opt-in.

Third-party compilation remains an explicit local operation: input bytes are bounded but execution
fuel/epochs are not independent compiler CPU/memory limits. Compilation isolation, persistent instances
and input widgets remain deferred; plan §8 lists them. D156's typed extension tests use the packaged
SDK fixture for veto fail-closed behavior, append-only contributions, isolated KV/CAS and runtime quotas,
isolated model transport and cancellation, and labelled read-only UI panels. MCP prompts, MCP resources, `codemode` and
images in a tool result are refused by decision (D119), not deferred. This migration does not introduce
automatic plugin updates, an authority picker, ambient native access or a second policy engine.
