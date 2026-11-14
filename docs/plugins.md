# WASI read-tool plugins

Saddle loads explicitly installed, digest-pinned WASI components as model-callable tools
(D126–D129). The initial runtime supports **read-only tools**, not arbitrary extensions. A manifest
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
picker. Shared tool-visible directories and standard-component delivery still need work. Standard
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
because native `read` can. Those directories need a separate bounded delegation design, not an
automatic exception or a new grantable root.

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

Services are independently linked by each plugin's bounded grants. Enabling transcript access does
not enable filesystem access or vice versa; an omitted service is unavailable even when the agent
or a sibling plugin holds it. Compiling against SDK bindings does not grant their authority.

## Built-in migration tracking

No standard component is silently aliased over a built-in or given implicit grants.

| Tool | Guest implementation | Remaining parity/adapters |
| --- | --- | --- |
| `read` | native-style text pagination/totals, deep offsets, bounded range reads and explicit image mode | managed tool paths, scans beyond resource bounds, standard-component delivery |
| `ls` | filtered directory pages, suffixes, sorted output | managed tool paths; bounded/non-UTF-8 listing differences |
| `find`, `grep` | guest traversal, shared ignore syntax, glob/regex matching | large-tree/performance edges, host-path display and unreadable-file metadata |
| `write`, `edit`, `patch` | not migrated | scoped mutation/atomic transaction services, reserved paths, preflight all patch paths |
| `bash` | not migrated | delegated exec sandbox, bounded streaming, cancellation, environment filtering and reservation restore |
| `fetch`, `websearch` | not migrated | bounded attributed network/search adapters, redirects, protected credentials and grant narrowing |
| `session_query` | guest schema/presentation over branch-only, bounded transcript SQL under delegated `transcript-read` | standard-component delivery and native-entry-point removal |

The target is replacement, not parallel selectable implementations: native entry points go away as
parity and standard-component delivery land. Remaining adapters must act as the plugin principal,
not reuse a built-in's hard-coded agent identity. D65/D66 tool-visible paths remain distinct from
roots; their SDK adapter must reuse the existing configuration rather than require another directory
declaration. That adapter is not implemented yet.

## Authority and provenance

Operational authority is bounded by the agent, then the manifest's scope ceiling, then the plugin's
explicit grants. An omitted plugin grant means none, not inheritance. An above-ceiling request is a
load error. Both principals' filesystem exclusions apply; sibling plugins cannot supply missing reach.
Native tools do not require an explicit `agent.tool` grant to use their host-managed dispatch surface.

The session assigns the plugin principal and a unique call id; guest JSON cannot select them.
Filesystem imports decide and audit both requested and resolved names with that principal. Directory
descriptors are pinned with no-follow opens; final reads require a regular file, use nonblocking and
no-follow flags, and cap bytes during I/O. Absolute symlinks, root escapes and excluded targets are
refused. A FIFO, socket, device or oversized file is not a way around the boundary.

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
- `saddle-sdk` provides guest bindings, exports and the read-only Rust example. It calls the typed
  host `read-file` function, never `std::fs`.
- `saddle::plugins` owns protected artifact loading, registration, provenance and the production
  filesystem adapter. Its worker asks the session thread over a bounded channel; the loader does
  not own a second policy engine or depend on `Session`.

Only `fs-read`, `transcript-read` and `tool` grants are supported by this runtime. Other scopes fail closed. The
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
