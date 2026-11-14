# WASI tool plugins

Saddle loads explicitly installed, digest-pinned WASI components as model-callable tools
(D126–D143). The runtime supports bounded filesystem and transcript services, not arbitrary extensions. A manifest
in a repository never executes automatically. Installation, enablement and grants are separate steps
performed by a person; an agent cannot write its own protected configuration.

## Build and install the example

From this repository, with `wasm32-unknown-unknown` installed:

```sh
cargo build -p saddle-sdk --example reader --target wasm32-unknown-unknown
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/debug/examples/reader.wasm target/reader.component.wasm
```

Create `target/reader.plugin.toml`:

```toml
id = "reader"
version = "0.1.0"
component = "reader.component.wasm"
scopes = ["tool", "fs-read"]
```

Then, from your own terminal:

```sh
cargo run --bin saddle -- plugin install target/reader.plugin.toml
```

Installation copies the manifest and component into
`$XDG_CONFIG_HOME/saddle/plugins/reader/` (normally `~/.config/saddle/plugins/reader/`). It refuses
replacement of an existing installation, does not compile or execute the component, and does not
enable it or change grants. It prints a configuration block containing the component's SHA-256.
Add that block to your protected `config.toml`, using the actual printed digest:

```toml
[plugins.reader]
enabled = true
manifest = "reader/plugin.toml"
sha256 = "<64 hexadecimal digits printed by install>"
```

Grant the plugin only what it needs, separately:

```toml
[plugin.reader]
tool = {}
fs-read = [{ root = "workspace", exclude = [".env*", "private/**"] }]
```

These grants can live in the base `config.toml`, or only in a workspace's protected policy file
under `policies/` beside it. Base grants apply in every workspace; use the workspace policy for a
workspace-specific delegation. Its existing `workspace = "<absolute path>"` header must remain.
The `[plugins.reader]` installation/enablement table belongs in the base configuration, not in a
repository manifest or a workspace policy. The agent's effective grants must also permit the
requested reads. `tool` permits registration, not additional operational authority.

## Use and inspect

```sh
saddle plugin list
saddle check
saddle plugin call reader '{"root":"workspace","relative":"Cargo.toml"}'
saddle tool plugin__reader__read-file '{"root":"workspace","relative":"Cargo.toml"}'
```

The SDK example requires `root` and `relative` string arguments. These
CLI calls and model calls use the same dispatch, policy engine and audit log. An enabled, granted,
valid plugin contributes one namespaced tool, `plugin__<id>__<tool>`, to the model's catalogue.
`plugin list` reports disabled, ungranted, failed and ready plugins. Bad digests, unsupported imports,
traps and bad descriptions leave native tools available; host or audit failures stop the operation.

After changing configuration, grants or artifacts, use `/reload` in an active session, or open a
new one. Reload discards old prepared components and catalogues, rechecks digests and both authority
ceilings, and refreshes the next model request. A stale tool name is denied. To disable a plugin,
set its entry's `enabled = false` and reload. Installation never replaces an existing directory;
updating requires explicitly removing the old installation, reinstalling, and reviewing the new pin.
There is no downloader, signing service, registry or automatic update mechanism.

## Standard text-read plugin

`saddle-tools` is the home for tool implementations rather than host services. Its first component
is `files`, exporting `plugin__files__read`. Unlike the minimal SDK reader above, it handles source
files larger than 64 KiB by requesting bounded byte ranges, selects line pages in guest code, and
keeps the host independent of line formatting:

```sh
cargo build -p saddle-tools --example read --target wasm32-unknown-unknown --release
mkdir -p target/read-plugin
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/release/examples/read.wasm target/read-plugin/read.component.wasm
cp crates/saddle-tools/plugins/read/plugin.toml target/read-plugin/plugin.toml
saddle plugin install target/read-plugin/plugin.toml
```

The opt-in production-host test uses that built artifact:

```sh
cargo nextest run --test plugins --run-ignored only \
  -E 'test(standard_read_component_paginates_large_files_through_the_production_host)'
```

Enable the printed `[plugins.files]` block and grant `[plugin.files]` the same `tool`/`fs-read`
permissions shown for the reader example. Then:

```sh
saddle plugin call files '{"path":"src/lib.rs","offset":100,"limit":40}'
saddle tool plugin__files__read '{"root":"workspace","path":"src/lib.rs"}'
```

Without `root`, `path` may be workspace-relative, absolute or home-relative, but must map to an
explicitly granted true root. With `root`, `path` is root-relative. `offset` is a positive 1-based line number.
Pages have at most 2,000 lines and 50 KiB of text plus a bounded continuation/truncation note. Plain
text, CRLF and trailing newlines are preserved, without line-number prefixes that could interfere
with exact edits. Oversized lines may be truncated; continuation advances past the truncated line.
UTF-8 validation covers the entire scan, including characters split across range boundaries.
Binary/NUL-containing files are refused.

Each invocation scans at most 64 MiB from the beginning, using ranges up to 64 KiB and bounded guest memory.
The normal fuel/deadline still applies and may stop it earlier. Release builds are recommended;
debug guest code can consume substantially more fuel. Total line counts and native-style continuation
notes are reported, including for offsets beyond 1 MiB. A scan exceeding the bound is refused rather
than reading indefinitely. Ranges are individually authorized, reopened and audited with offset,
limit and call identity; they are not a snapshot if the file changes concurrently.

The original `read-file` interface and its 64 KiB limit remain unchanged. The new additive
`saddle:host/fs-read-range@0.1.0` uses the same `fs-read` grant and returns bytes plus an observed EOF
flag. Each range is limited to 1–65,536 bytes; the host may read one extra byte to establish EOF.
SDK hosts other than Saddle must implement `HostServices::read_range` to support it; its default
returns an unsupported-operation error. Existing compiled reader components remain compatible.

The reader's `format` argument explicitly selects the operation (D136). It defaults to `"text"`:
`{"path":"notes.png"}` reads UTF-8 text regardless of extension or signature. Use
`{"path":"screenshot","format":"image"}` for a PNG, JPEG, GIF or WebP attachment. There is no
automatic mode detection or fallback: invalid UTF-8 fails in text mode, and invalid image metadata
fails in image mode. `offset` and `limit` select text lines only. The native reader is unchanged.

In image mode, bytes are assembled from bounded ranges (at most 5 MiB) and attached as real image
results through the existing provider/UI/store pipeline. The additive `tool-result.emit-image`
service determines media type and dimensions from image headers; this does not select the read mode,
open the supplied name or perform image decoding/recompression. Only one image per invocation is
accepted. Failed/trapped calls publish no images, and saved image data/provenance survives replay.
The 64 KiB text result limit is not enlarged to accommodate image bytes.

This is an incremental migration toward replacing built-in `read`, not a permanent implementation
picker. Shared tool-visible directories use the same SDK with runtime permissions as described below;
standard-component delivery still needs work. Standard
plugins are not silently enabled or given the agent's authority merely because their source ships
in the repository.

## Standard directory listing plugin

`listing` exports `plugin__listing__ls`, implemented in `saddle-tools`:

```sh
cargo build -p saddle-tools --example ls --target wasm32-unknown-unknown --release
mkdir -p target/ls-plugin
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/release/examples/ls.wasm target/ls-plugin/ls.component.wasm
cp crates/saddle-tools/plugins/ls/plugin.toml target/ls-plugin/plugin.toml
saddle plugin install target/ls-plugin/plugin.toml
```

Enable the printed `[plugins.listing]` entry and separately grant `[plugin.listing]` `tool` and the
needed `fs-read` roots. `saddle plugin call listing '{"path":"src"}'` lists a directory; omitting
`path` lists the workspace. Path resolution follows the read plugin's rules. Results are sorted,
with `/` for directories and `@` for symlinks, at most 500 entries/50 KiB plus summary notes.

The guest requests lexical pages of at most 128 entries via `fs-list`. The core checks and audits
the start directory and filters both agent/plugin exclusions on every child before returning names.
Symlinks are identified but not followed; a subsequent call targeting an escaping link is refused.
Each page scans at most 100,000 entries; larger directories fail rather than silently missing names.
Non-UTF-8 names are skipped, not converted to potentially ambiguous paths. Names in concurrently
modified directories are not a snapshot. The guest owns suffixes, sorting and output truncation.

The additive `fs-paths` service maps paths to real roots under the same `fs-read` permission. It
never lets a guest select a native tool identity or access D66 tool-visible directories merely
because native `read` can. The core also resolves configured external directories through this same
SDK function, but checks their distinct runtime permissions as described below; they are not new
grantable roots or automatically accessible to every filesystem plugin.

```sh
cargo nextest run --test plugins --run-ignored only \
  -E 'test(standard_ls_component_lists_filtered_pages_and_never_follows_entry_links)'
```

## Standard find and grep plugins

`finder` exports `plugin__finder__find`; `search` exports `plugin__search__grep`. Both implement
traversal and matching in the guest. The core supplies only policy-filtered directory pages and
bounded reads; it does not run native `find` or `grep` on their behalf. Ignore parsing is shared
with native tools and skill discovery through the authority-free `saddle-ignore` crate.

Build each component, then install, enable and grant it independently:

```sh
cargo build -p saddle-tools --examples --target wasm32-unknown-unknown --release
mkdir -p target/find-plugin target/grep-plugin
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/release/examples/find.wasm target/find-plugin/find.component.wasm
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/release/examples/grep.wasm target/grep-plugin/grep.component.wasm
cp crates/saddle-tools/plugins/find/plugin.toml target/find-plugin/plugin.toml
cp crates/saddle-tools/plugins/grep/plugin.toml target/grep-plugin/plugin.toml
saddle plugin install target/find-plugin/plugin.toml
saddle plugin install target/grep-plugin/plugin.toml
```

Use the printed `[plugins.finder]`/`[plugins.search]` entries, with corresponding explicit
`[plugin.finder]`/`[plugin.search]` `tool` and `fs-read` grants. Example calls:

```sh
saddle plugin call finder '{"path":"src","pattern":"*.rs"}'
saddle plugin call search '{"path":"src","pattern":"TODO|FIXME","glob":"*.rs","ignore_case":true}'
```

Globs without `/` match filenames at any depth; those with `/` match paths from the search
directory. Both honor `.gitignore`, `.ignore` and `.fdignore`, skip `.git`, and do not follow
symlinks or read special files. Parent ignore files apply when starting in a subdirectory. Regex
search returns `path:line: text`, supports explicit readable file targets and case-insensitive
matching, and skips binary, invalid UTF-8, oversized and unreadable content. All permission checks
remain independent of ignore rules. Files larger than 64 KiB are searched through bounded ranges.

Limits: 1,000 find results, 200 grep matches, 50 KiB result text, 300-byte UTF-8-safe line previews,
4 MiB per searched file, 100,000 visited entries, 2,048 pending directories/1 MiB of queued paths,
128 KiB aggregate ignore text (64 KiB per file), plus normal fuel/memory/deadline bounds. Large
ignore data fails explicitly. Other limits produce narrowing notes; the runtime can interrupt
earlier. These are incremental replacements: other roots display `root:relative` rather than
absolute host paths, and an explicitly requested unreadable file may report a directory error.
Cold compilation of regex-bearing components is substantial; it happens once per loaded component
in a running session, but standalone CLI calls each start a new process. Compilation caching and
independent compilation resource limits remain work, not execution-fuel guarantees.

```sh
cargo nextest run --test plugins --run-ignored only \
  -E 'test(standard_search_components_walk_and_match_without_bypassing_exclusions)'
```

## Runtime access to configured external directories

The same compiled filesystem plugin and SDK functions handle workspace paths and D66 directories
such as `~/.agents` (D139). The core resolves the path and checks the calling plugin's runtime
permissions. There is no `tool-paths` Cargo feature, alternate guest artifact or guest-side choice
of permission mechanism. D138's feature-based routing was superseded, not retained as an option.

Directory authority remains the existing user-owned `[sandbox]` declarations and default read-only
`~/.agents` entry. No directory is declared again. Bare sandbox paths and include rules do not become
host-tool access. A plugin may hold root access, external-directory access, or both:

```toml
[plugin.files]
tool = {}
fs-read = [{ root = "workspace" }]
# Optional runtime permission; add/remove and reload without rebuilding or repinning:
tool-read = { tools = ["read"] }
```

With only `fs-read`, this reader remains loaded and reads the workspace, but is denied `~/.agents`.
Adding the `tool-read` grant and reloading enables that existing configured reach for the exact same
artifact. Removing it and reloading denies external reads again while workspace reads still work.
A custom external-only reader can omit `fs-read` entirely and use the same SDK calls.

The filesystem interfaces are linked with any filesystem capability; interface availability never
supplies permission for a path or operation (D140). With no filesystem capability, filesystem imports
are refused before initialization. Other SDK services keep their own capability requirements. Manifest ceilings and
agent reach still bound every plugin grant. Standard manifests allow the optional runtime scope but
never grant it.

Use `ls`, `find` or `grep` for the corresponding component's external-directory role. Roles select
configured directory reach, not the guest's name or implementation. The parent ceiling is derived
from current D66 declarations; no extra agent grant or new directory list is needed. An explicit
agent `tool-read` table may narrow that ceiling, never add directories. A sibling's grant, `tool`
registration and root grants cannot supply external-directory access.

The core retains true-root precedence: root exclusions cannot be bypassed by treating a root path
as an external directory. Requested paths and linked targets must both be allowed. Protected
config/state overlaps, undeclared targets and special-file reads remain refused. Metadata filtering,
bounded ranges, deadlines, pinned handles and per-plugin audit/call identity are unchanged. Reload
also rebuilds directory mappings and pins. Opaque path identifiers are handled by the core; plugins
use `resolve_path`, `read_range` and `read_directory` without routing between implementations.

Build standard components using the ordinary commands above, with no feature flags. The old
D138-specific host interface is retained for binary compatibility, not used by the standard helpers.
The acceptance test exercises one artifact and digest in one running harness across grant/revoke:

```sh
cargo nextest run --test agent --run-ignored all -E 'test(one_compiled_reader)'
cargo nextest run --test plugins --run-ignored all -E 'test(managed_)'
```

## Standard transcript-query component

The `session_query` guest owns the schema and presentation; core `transcript` services own the
active-branch snapshot, SQL authorizer, execution and bounds (D137). The plugin has no database or
filesystem access. Build it with:

```sh
cargo build -p saddle-tools --example session_query --target wasm32-unknown-unknown --release
mkdir -p target/session-query-plugin
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/release/examples/session_query.wasm \
  target/session-query-plugin/session_query.component.wasm
cp crates/saddle-tools/plugins/session_query/plugin.toml target/session-query-plugin/plugin.toml
cargo run --bin saddle -- plugin install target/session-query-plugin/plugin.toml
```

Enable and pin it as above under `[plugins.transcript]`, and explicitly grant its services:

```toml
[plugin.transcript]
tool = {}
transcript-read = {}
```

The agent must also hold `transcript-read = {}`. No `fs-read` grant is needed. Its namespaced tool
is `plugin__transcript__session_query`, taking `{"sql":"SELECT ordinal,role,text FROM messages LIMIT 10"}`.
The SDK entry point is `query_transcript(sql)`: SQL is capped at 16 KiB, results at 100 rows/50 KiB,
with `columns`, `rows` and `truncated` fields. Only `messages`, `requests` and `reasoning` views of
the active branch are queryable. Saved-database tables, other sessions, writes and file functions
remain inaccessible. With no active conversation, it returns an error instead of choosing one.

Unrelated services are independently linked by each plugin's bounded grants. Enabling transcript
access does not enable filesystem access or vice versa. The filesystem family is available under
any filesystem capability, but each operation/path still needs its own grant; one plugin's grants
never supply another's. Compiling against SDK bindings does not grant their authority.

## Standard mutable filesystem components

The `writer`, `editor` and `patcher` components implement `write`, `edit` and `patch` in guest code
(D140). Their SDK primitive is `apply_changes(&[Change])`, a bounded batch of writes/removals with
`Any`, `Missing` or SHA-256 preconditions. The core does not parse patches or choose text matches.

```sh
cargo build -p saddle-tools --examples --target wasm32-unknown-unknown --release
for name in write edit patch; do
  mkdir -p target/$name-plugin
  cargo run -p saddle-loader --example componentize -- \
    target/wasm32-unknown-unknown/release/examples/$name.wasm \
    target/$name-plugin/$name.component.wasm
  cp crates/saddle-tools/plugins/$name/plugin.toml target/$name-plugin/plugin.toml
done
saddle plugin install target/write-plugin/plugin.toml
# Likewise install edit-plugin/patch-plugin, then enable and pin each printed entry.
```

Permissions are runtime configuration of those same artifacts:

```toml
[plugin.writer]
tool = {}
fs-write = [{ root = "workspace", exclude = [".env*", "private/**"] }]

[plugin.editor]
tool = {}
fs-read = [{ root = "workspace" }]
fs-write = [{ root = "workspace" }]

[plugin.patcher]
tool = {}
fs-read = [{ root = "workspace" }]
fs-write = [{ root = "workspace" }]
```

The agent must hold the corresponding grants too; both principals' exclusions apply. The tools
are `plugin__writer__write`, `plugin__editor__edit` and `plugin__patcher__patch`. Arguments match
native `write` (`path`, `content`), `edit` (`path`, `old`, `new`) and `patch` (`patch`). Writers do
not need read authority. A write-only patcher can add/delete; updating/moving requires actual read
permission. SDK service availability is not permission to read or write a path: the filesystem
service family can link under any filesystem capability, while each operation checks its own grant.
Adding/removing write permission and reloading does not rebuild or repin the plugin.

The core preflights every requested/resolved target and precondition before publication, rejects
reserved paths and overlapping/aliased targets, stages only supplied bytes through pinned parent
handles, and replaces entries by rename rather than truncating an existing inode. File payloads
are not logged: audit records paths, operation, size, hash, principal and call id. Mutations during
`describe` are refused. A guest trap after committed effects does not roll them back and is reported
as such. Staging files use exclusive creation and are cleaned after failure; empty created parents can remain.
SHA-256 preconditions independently require read authority on both requested and resolved paths;
write-only callers cannot probe existing hashes.

Bounds are 32 paths and 1 MiB of aggregate resulting content per batch, 1 MiB per conditional input
file, plus the normal 64 KiB tool-argument and fuel/deadline limits. Add/move destinations must be
absent; deletes are idempotent. Edits/hunks must match exactly once. Source hashes detect stale
preimages before publication, but are not an OS-level atomic compare-and-swap against independent
writers. Publication is atomic per entry, **not** a multi-file transaction or crash-durability
promise: I/O errors during publication may partially apply and are explicitly reported.

The same artifacts support D66 configured external directories (D141): add
`tool-write = { tools = ["write"] }` to the writer's runtime grants (use `edit`/`patch` for those
components). The protected `[sandbox]` entry must independently name that role. Updates also need
`tool-read = { tools = ["read"] }` and a configured read role. True roots retain precedence;
external links must land in independently permitted declared directories, and protected/reserved
paths remain refused. No new allowlist, root, build variant or repinning is involved.

Guests emit bounded human-only line diffs through `tool-result.emit-diff`. They are retained in
saved/replayed tool results, not model text. Blind writes/deletes label unavailable preimages rather
than acquiring read access implicitly. Diffs are bounded, may summarize a changed span rather than
produce minimal hunks, and are discarded on guest error/trap. Native tools have not been removed yet.

```sh
cargo nextest run --test plugins --run-ignored all -E 'test(mutation)'
cargo nextest run --test agent --run-ignored all -E 'test(one_compiled_writer)'
```

## Standard shell component

The `shell` component exports `plugin__shell__bash` (D142). Build/componentize the `bash` example
with the same commands as the filesystem examples. Its SDK accepts an argv and bounded timeout;
the guest chooses `sh -c`, owns its schema and formats captured streams/status. The core execution
service is shared with the temporary native adapter, but always uses the actual caller's grants.

```toml
[plugin.shell]
tool = {}
exec = { timeout-secs = 120 }
fs-read = [{ root = "workspace" }]
fs-write = [{ root = "workspace" }]
# Optional; omission means commands cannot borrow the agent's network grants:
net = [{ host = "crates.io" }]
```

The parent must hold the corresponding reach. Exec environment names cannot exceed its grant,
timeouts are narrowed, and filesystem exclusions still do not bind exec (the existing sandbox
contract). Protected sandbox read/write/include configuration supplies the same enumerated runtime
and toolchain paths as native execution, not additional file-tool roots. Credentials are stripped.
Network decisions and reserved-path restores retain the plugin principal and call id; audit failures
are fatal. Namespace failure falls back to no network. Cancellation/timeout kills the process group,
with the existing `setsid` escape limitation unchanged.

Commands may wait beyond the guest's two-second CPU deadline: long SDK calls suspend that wall
accounting only for actual bounded host waiting, with 300 seconds cumulative wait and 16 long calls
per invocation. Fuel and guest computation deadlines remain enforced. Commands capture at most
256 KiB per stream; the standard guest presents at most 24 KiB each and labels truncation. There is
no interactive terminal or ambient guest subprocess access. Description-time execution is refused;
a trap after execution warns that effects may already have happened.

```sh
cargo nextest run --test plugins --run-ignored all -E 'test(standard_shell)'
```

## Standard network/search components

The `http` and `web` components export `plugin__http__fetch` and `plugin__web__websearch` (D143).
Build/componentize the `fetch`/`websearch` examples using the filesystem workflow. Native entry points
now use the same guest-owned parsing/presentation library through temporary adapters; they do not
supply agent authority to plugin calls.

```toml
[plugin.http]
tool = {}
net = [{ host = "docs.rs" }, { host = "*.rust-lang.org" }]

[plugin.web]
tool = {}
# Grant the configured provider's search host, or the selected Exa fallback:
net = [{ host = "api.synthetic.new" }, { host = "api.exa.ai" }, { host = "mcp.exa.ai" }]
```

Parent grants must cover every requested scheme/host/port. Narrowing a wildcard to an exact name
cannot acquire private-network exemptions the parent lacks. SDK filesystem authority is unrelated.
The host canonicalizes HTTP(S) URLs, rejects userinfo and routing/framing override headers, checks
DNS addresses through the shared resolver, independently gates each redirect (five maximum), and
removes all caller headers on cross-origin redirects. Responses capture at most 100 KiB; the guest
validates UTF-8 and presents about 60 KiB with truncation labelled. Header names are audited, values
never are; query values are redacted. Headers must fit 64 entries/64 KiB.

The search SDK accepts only query/count, not an endpoint or protected credential selector. The core
uses the configured provider, keyed Exa REST or anonymous Exa MCP, and sends credentials only through
those fixed protocol paths. Credential-bearing requests do not follow redirects. Protected search
responses are capped at 1 MiB and cleartext key echoes are redacted. The guest parses backend results,
selects excerpts, numbers the originally returned set, formats full pages and chooses whether a new
query requires another search. Anonymous MCP text is not falsely presented as numbered full pages.

The session retains at most 17 bounded response snapshots, keyed by principal. A sibling cannot
expand another plugin's search. Expansions make no HTTP request, and plugin snapshot access rechecks
the original endpoint's current reach, so revocation cannot leave a cache-reading bypass. Search
queries are bounded to 16 KiB and results to 100. Raw responses remain core-managed data, not plugin
persistent storage. Reload needs no rebuild or repinning.

Network calls use the cumulative host-wait budget from D142, with 30 seconds per call. Cancellation
is checked around I/O; it cannot immediately interrupt a blocked resolver/read. Description-time
HTTP, search and cache access are refused; audit failures are fatal. Raw WASI outgoing HTTP, sockets
and poll remain unavailable. Standard delivery and native-entry-point removal are still separate.

```sh
cargo nextest run --test plugins --run-ignored all -E 'test(standard_fetch)'
cargo nextest run --test agent --run-ignored all -E 'test(standard_search)'
```

## Built-in migration tracking

No standard component is silently aliased over a built-in or given implicit grants.

| Tool | Guest implementation | Remaining parity/adapters |
| --- | --- | --- |
| `read` | native-style text pagination/totals, deep offsets, bounded range reads, explicit image mode and optional managed paths | scans beyond resource bounds, standard-component delivery |
| `ls` | filtered directory pages, suffixes, sorted output, optional managed paths | bounded/non-UTF-8 listing differences |
| `find`, `grep` | guest traversal, shared ignore syntax, glob/regex matching | large-tree/performance edges, host-path display and unreadable-file metadata |
| `write`, `edit`, `patch` | guest workflows over bounded, preflight-all mutation batches and optimistic preconditions | standard delivery; documented size/atomicity and bounded diff presentation differences |
| `bash` | guest shell workflow over delegated sandbox execution, bounded capture, timeout/cancellation and reservation restore | standard delivery; documented capture/presentation and process-group limitations |
| `fetch`, `websearch` | guest schemas/workflows/parsing/presentation over attributed HTTP and protected search services | standard delivery; bounded presentation and blocking-I/O cancellation limits |
| `session_query` | guest schema/presentation over branch-only, bounded transcript SQL under delegated `transcript-read` | standard-component delivery and native-entry-point removal |

The target is replacement, not parallel selectable implementations: native entry points go away as
parity and standard-component delivery land. Remaining adapters must act as the plugin principal,
not reuse a built-in's hard-coded agent identity. D65/D66 tool-visible paths remain distinct from
roots; their SDK adapter reuses the existing configuration rather than requiring another directory
declaration.

## Authority and provenance

Operational authority is bounded by the agent, then the manifest's scope ceiling, then the plugin's
explicit grants. An omitted plugin grant means none, not inheritance. An above-ceiling request is a
load error. Both principals' filesystem exclusions apply; sibling plugins cannot supply missing reach.
Native tools do not require an explicit `agent.tool` grant to use their host-managed dispatch surface.

The session assigns the plugin principal and a unique call id; guest JSON cannot select them.
Filesystem imports decide and audit both requested and resolved names with that principal. Directory
descriptors are pinned with no-follow opens; final reads require a regular file, use nonblocking and
no-follow flags, and cap bytes during I/O. Normal roots refuse absolute symlinks, root escapes and
excluded targets. Managed tool paths permit links only into other declared, independently allowed
tool paths. A FIFO, socket, device or oversized file is not a way around the boundary.

Plugin descriptions and results are untrusted data. Results are wrapped with a random session
delimiter, embedded delimiter prefixes are neutralized and terminal controls removed. Host metadata
records principal, scope, call id, version and component digest beside the result. Saving, resuming,
forking and replay retain the original wrapper and metadata; native results remain unwrapped.
These labels are provenance, not permission to follow instructions found in a plugin response.

## Runtime and SDK

- `saddle-wit/wit` owns the canonical `saddle:host/fs-read@0.1.0`,
  `saddle:plugin/tool@0.1.0` and `read-tool` world. Host and guest bindings use the same files.
- `saddle-loader` hashes and compiles the supplied binary bytes, checks actual imports, export
  signatures and capabilities before initialization, and creates a fresh bounded instance for each
  `describe` or `invoke`. Native serialized Wasmtime code is not accepted.
- `saddle-sdk` provides bindings/exports and typed filesystem, execution, HTTP and transcript/search
  services; guests receive no ambient `std::fs`, subprocess or socket authority.
- `saddle::plugins` owns protected artifact loading, registration, provenance and the production
  filesystem adapter. Its worker asks the session thread over a bounded channel; the loader does
  not own a second policy engine or depend on `Session`.

Only `fs-read`, `fs-write`, `tool-read`, `tool-write`, `exec`, `net`, `transcript-read` and `tool` grants are supported by this runtime. Other scopes fail closed. The
component classifier describes a wider planned ABI; that does not make its adapters available.
Descriptions are capped at 1,000 characters, schemas at 16 KiB and configured plugins at 16.
Schemas must parse as JSON objects with an object input type; the provider may enforce further
JSON Schema restrictions. Saddle does not execute schema references or guarantee every provider
accepts every schema.
Manifests are capped at 16 KiB and component artifacts at 4 MiB. Artifact paths must stay under
the protected plugin directory without symlinks; manifest identity and the configured digest must match.

Defaults per invocation are 100 million fuel units, a two-second deadline, 16 MiB per linear memory
(at most four), 10,000 elements per table (at most eight), at most 32 core instances, and 64 KiB
input/output/file-read limits. Image emission has a separate one-image/5 MiB cap. Initialization and `describe` share the execution bounds. The epoch clock
runs every 10 ms while any prepared plugin needs it.

WASI starts with empty environment/arguments, no working directory or preopened descriptors, closed
stdin, discarded stdout/stderr, a 128-entry host resource table and 64 KiB random-byte request
limits. No terminal, sockets or credentials are inherited. Raw filesystem access, sockets, outgoing
HTTP and synchronous `wasi:io/poll` are not linked. Poll could block in native code beyond epoch or
fuel interruption; general `wasm32-wasip2` Rust components importing it are therefore unsupported.
The example's `wasm32-unknown-unknown` target avoids these ambient adapters. The development-only
componentizer encodes the embedded WIT metadata; it does not install or grant anything. The SDK's
WIT path currently assumes this monorepo layout; separately published packages are not implemented.

Cancellation and deadlines are checked while serving host reads, but cannot preempt a kernel
filesystem operation already in progress. A stuck remote filesystem remains a limitation. Fuel and
epochs bound guest execution, not compilation: independent compiler time/memory isolation is still
deferred. Only explicitly installed, digest-pinned local artifacts are automatically loaded; do not
treat the compiler as a resource-isolated processor for arbitrary untrusted downloads.

Other deferred work includes remaining host interfaces, persistent instances/storage, observers,
veto/UI hooks, a bounded debug-log sink, packaged SDK distribution and artifact update tooling.

## Test the boundary

```sh
cargo nextest run -p saddle-cap -p saddle-loader --no-fail-fast
cargo nextest run --test plugins
cargo nextest run --test agent -E 'test(plugin)'
cargo nextest run -p saddle-loader --run-ignored only \
  -E 'test(sdk_reader_component_round_trips_through_the_runtime)'
cargo fmt --all --check
cargo clippy --all-targets
```

Ordinary tests compile checked-in WAT into real components; they need no guest Rust target. The
opt-in SDK test builds the example and checks allowed reads, exclusions and audit-chain integrity.
CLI/model tests use isolated home/config/state directories and cover installation, pinning, import
refusal before initialization, agent/plugin/sibling boundaries, symlinks, special/large files,
registration, reload revocation, saved provenance and replay. Loader tests additionally exercise
fresh-instance state, resource limits, deadlines/cancellation, signatures, host failures and empty
ambient authority.
