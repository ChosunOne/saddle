# Working in this repository

Saddle is a coding-agent harness in Rust. Its distinguishing property is that plugins are WASI
components whose authority is a *declared* scope set, checked before the component is instantiated.
`docs/plan.md` is the specification; read it before changing behaviour in the policy, sandbox or tool
layer.

There is no README. `docs/plan.md` is the design document, the rationale and the milestone list.

This file is one of the instruction files saddle loads into its own prompt (the workspace root
`AGENTS.md`; `src/context.rs`, D32). Keep it accurate — a stale instruction file is worse than none,
because it is followed.

## Layout

| crate | owns |
| --- | --- |
| `crates/saddle-cap` | scopes, roots, grants, policy and manifest parsing, the component import check, the exec lowering. No wasmtime dependency. |
| `crates/saddle-policy` | per-call decisions (`engine`) and the hash-chained audit log (`audit`) |
| `crates/saddle-sandbox` | applies a sandbox spec to the current process and execs the target; also builds the `saddle-sandbox` helper binary used by its own tests |
| `crates/saddle` | the binary: built-in tools, model providers, the model loop, the TUI |

Inside `crates/saddle/src`:

| module | owns |
| --- | --- |
| `agent` | the model loop (no request limit unless one is set, D37; a turn stops on a repeated call and answer, D38), the system prompt (base plus grants, so a policy reload replaces only the grants, D47), tool dispatch |
| `session` | opening the workspace, resolving roots, the policy engine and audit log for a session |
| `tools/fs`, `tools/search`, `tools/bash`, `tools/fetch`, `tools/patch` | the nine built-in tools (`fetch` is host-side network, D39; `patch` is D56) |
| `tools/diff` | line diffs of edited files, for the transcript only — the model never sees them |
| `context` | instruction files (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md` fallback), read through the policy |
| `skills` | `SKILL.md` skills: discovery through the `read` gate, the prompt listing (D64, D67) |
| `compact` | replacing the older part of a long conversation with a model-written summary (D33) |
| `store` | the saved conversations: a SQLite tree of messages per workspace (D53), imported from the older JSON Lines files once |
| `output` | line-mode output: a terminal, or one JSON object per line for a program (D52) |
| `cache` | the cache-miss notice: one line to the user when a prompt is re-read at full price (D50) |
| `reserved` | the D20 snapshot and restore of reserved paths around `exec` |
| `provider/` | the neutral conversation form, the Anthropic and OpenAI-compatible renderers, the model list, the cached context windows and prices (D34, D36) |
| `tui/` | the full-screen UI: app state, rendering, editor, login, settings, session list, `permissions` (the grants editor, D45), `tree` (the message tree, D54), `image` (drawing a picture: kitty and iTerm2, D59), and `status` (the `[ui] status` template, D36) |

`crates/saddle/src/main.rs` dispatches subcommands (`run`, `check`, `tool`, `audit verify`)
and intercepts `__sandbox` before clap parses anything — that is the helper process path, and it must
stay first in `main`.

Built-in tools are `read`, `write`, `edit`, `patch`, `ls`, `find`, `grep`, `bash`, `fetch`,
(`tools::NAMES`). `patch` applies one apply-patch document across several files, deciding every path
before writing any of them, and refuses the whole patch if one is denied (D56).
`fetch` is the only one that reaches the network, and the **host** makes the request after the engine
allows it (D39): `exec` children never have sockets, so `curl` in `bash` fails by design. It may send
headers — a secret belongs there, never in a query string — and the audit log records the header
names but never a value (D48). Adding
network access means adding a `net` grant to the policy, never widening the sandbox allowlist. A
grant is a hostname, a `*.suffix`, or `*` for any public host (D40); a wildcard still cannot reach
loopback or private space, because that exemption needs a grant that names the address. `saddle tool
<name> '<json>'` runs any one of them as the agent through the gate, which is the most direct way to
check a policy change by hand.

Planned but not yet present: `saddle-wit`, `saddle-loader`, `saddle-sdk` (v1, D15).

## Commands

```sh
cargo build --offline
cargo test --offline          # all suites
cargo fmt --all --check       # must stay clean; no rustfmt.toml, so defaults apply
cargo clippy --offline --all-targets   # must stay clean
```

There is no CI config in the tree, no `rust-toolchain` pin and no `rustfmt.toml`. Edition 2024,
`rust-version = 1.88`. All dependencies are declared in the root `Cargo.toml` under
`[workspace.dependencies]` and referenced per crate as `dep.workspace = true`; add new dependencies
in the root, not in a crate manifest.

Cargo may have no network access: pass `--offline` and do not add dependencies that are not already in
`Cargo.lock`.

### Running the binary by hand

`saddle` writes first-run defaults into `$XDG_CONFIG_HOME/saddle` and its state into
`$XDG_STATE_HOME/saddle`, where `sessions.db` (D53) holds every saved conversation. Under a sandboxed agent both default to `~/.config` and `~/.local/state`,
which are outside the granted roots, so the process dies with `Permission denied`. Point them at a
writable location:

```sh
XDG_CONFIG_HOME=/tmp/saddle-try/config XDG_STATE_HOME=/tmp/saddle-try/state \
  cargo run --offline -q --bin saddle -- check
```

`--bin saddle` is required: the workspace has two binaries and no `default-run`. `saddle check` is the
cheapest way to see the resolved roots, the grants in force for the working directory, the
effective grants, whether that policy has been reviewed, whether `exec` is available on this kernel,
and the context window and price in force for the configured model.

To change what every workspace allows, edit `[agent]` in `config.toml`; to change one workspace, edit
its file under `policies/` — what it sets replaces the base grant for that scope, narrower or wider,
and takes effect in the next session. `--grant`/`--revoke` do it for one run. `/permissions` in the TUI edits the same file, and `a`
there adds a grant by typing what it is (`net <host>`, `root <name>`, `exec`, …); it is a person's
tool and no tool exposes it (D45, D46), and a change there applies at once, with the model told on
its next request (D47). Editing `saddle.toml` in a repository has no effect (D42, D43). `/thinking` (or shift+tab) changes the reasoning effort for the session (D57), `/name` and
`/session` name and describe it (D55), and escape twice on an empty prompt opens the message tree (D54), where a prompt can be sent again to
make another branch; `/clone` copies the branch into a separate session instead. There is no
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
  `net` must not reach the network through `curl` in a child process.
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
- Built-in tools are not privileged. None of the nine is a bypass: each passes through the same
  engine, the same audit log and the same sandbox as a plugin call (D17). `ls`, `find` and `grep`
  decide and log one read of the directory they start from, then filter everything beneath it through
  the same policy without a record per file, so an excluded file is skipped rather than shown
  (`tools/search.rs`).

A **skill** (D64) is instructions and never authority: its `allowed-tools` is ignored, because a
pre-approval written by whoever wrote the skill would be a second source of authority. A skill is
loaded with `read` on its `SKILL.md`, and there is no tool of its own (D67): discovery reads through
the same gate, so a refused skill is never advertised. The audit's `skill` record is what a session
offered, with a hash per skill.

Deny is the default everywhere: a decision is a lookup over grants, with no ordering, no fallthrough
and no deny rules. A grant may carry a `why`, which `saddle check` shows and the audit log records; it
is optional (D41). Exclusions live inside the grant they narrow (D22).

## Decision log

Decisions are cited by number as **D1** onward (D67 at the time of writing) in `docs/plan.md` §3, and
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
  a mock provider that answers over a loopback `TcpListener`; those tests need `socket()`/`bind()` to
  be permitted, so they fail in a sandbox that blocks it — that failure is the environment, not the
  code.
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
- **`exec` is Linux-only**, needs landlock ABI 3 as a hard requirement, and has no network: seccomp
  blocks `socket()` for `AF_INET`/`AF_INET6`/`AF_UNIX`/`AF_NETLINK`/`AF_PACKET` and the io_uring
  entry points. Do not add hosts or paths to the sandbox allowlist to make a test pass. Network
  access for the agent is the `fetch` tool and a `net` grant (D39), never a socket in a child.
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
