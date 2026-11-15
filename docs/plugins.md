# WASI tools and plugins

Saddle's eleven ordinary tools run shipped WASI components (D146–D147). There is no native fallback
or native/plugin implementation picker. `tool_search` remains a host MCP catalogue operation.
Filesystem effects, path resolution, permissions, audit, sandbox execution, HTTP/credentials and
branch-only transcript SQL remain privileged core services. Guests own schemas, workflows, parsing,
matching/traversal, pagination, minimal diffs and presentation.

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
fetch = ["net"]
websearch = ["net"]
session_query = ["transcript-read"]
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
to perform an operation. Each SDK effect is decided and audited by the session's one engine under
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
- **session_query:** read-only branch snapshots, including pre-compaction messages, request usage and
  prose reasoning. No saved-database handle, session selector, writes or arbitrary SQL functions.
  Parent and delegate both need transcript-read. Standalone calls report no active conversation.

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
It does not mutate stored/provider content. Thus normal CLI tool text and resumed tool cards do not
show framing noise, while `run --json` result events retain framed `content`, `provenance` and separate
`diff` for programs. Explicit third-party `plugin call` output retains its existing framing.

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


`image-bytes` bounds each image and aggregate emitted image bytes. SQL instruction work is checked
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
The optional library SDK-reader tutorial test is separate and still documents its manual build step.

Third-party compilation remains an explicit local operation: input bytes are bounded but execution
fuel/epochs are not independent compiler CPU/memory limits. Compilation isolation, persistent instances,
plugin storage/observers/vetoes, richer host adapters and MCP resources/prompts/codemode remain deferred.
This migration does not introduce automatic plugin updates, an authority picker, ambient native access
or a second policy engine.
