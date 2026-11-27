# Saddle

A coding-agent harness in Rust. It runs a model loop against Anthropic, OpenAI-compatible or
Responses providers, and gives the model a small, fixed set of tools whose authority is an explicit
set of grants: every file read, write, command, network request and plugin call is decided before it
happens and recorded in a hash-chained audit log. Deny is the default.

`CONFIG.md` explains what every configuration feature is for, what it can and cannot do, and where
its boundaries are. `docs/settings.md` is the key-by-key reference (also returned by the
`config_help` tool inside a session), `docs/plan.md` is the design document and
`docs/plugins.md` is the plugin and tool contract.

## Install

Requirements:

- Linux for the command sandbox (landlock ABI 3+; named launch profiles need ABI 6).
- Rust 1.100 or newer with the `wasm32-unknown-unknown` target, which builds the shipped WASI tools.

```sh
rustup target add wasm32-unknown-unknown
cargo build --release
install -m 0755 target/release/saddle ~/.cargo/bin/saddle
```

`cargo build` builds, componentizes and precompiles the standard tools automatically; nothing else
needs preparing. `cargo nextest run` runs the test suite.

## First run

Saddle writes first-run defaults and keeps its state outside the workspace:

- configuration: `$XDG_CONFIG_HOME/saddle/` (usually `~/.config/saddle/`)
- state, saved conversations and the audit log: `$XDG_STATE_HOME/saddle/` (usually `~/.local/state/saddle/`)

```sh
saddle login <provider>        # or /login in a session
saddle check                   # resolved roots, grants, sandbox, network, model
saddle run "explain this repo" # one turn, in the current directory
saddle                         # the full-screen interface
```

`/permissions` edits this workspace's grants from inside the interface, and `a` there adds one by
typing it (`net api.example.com`, `root scratch`, `exec`, …). `--grant`/`--revoke` do it for one run.

## Configure

Grants live in protected configuration, **never in the checkout** — a sandboxed command can write
everything its roots allow, so a policy file in the repository would be no protection at all.

`$XDG_CONFIG_HOME/saddle/config.toml` holds what every workspace shares:

```toml
[agent]
fs-read  = [{ root = "workspace" }]
fs-write = [{ root = "workspace", exclude = [".env*"] }]
exec     = { timeout-secs = 300 }
net      = [{ host = "*.crates.io" }]
```

A single workspace is narrowed (or widened) in `$XDG_CONFIG_HOME/saddle/policies/--<path>--.toml`,
which must carry a matching `workspace = "…"` line. Layers inherit any scope they do not name, and a
list may subtract one entry (`net = ["-api.example.com"]`) instead of restating the rest.

Named permission profiles let one checkout keep several grant sets:

```toml
[profiles.review]
net = []

[profiles.build]
net = [{ host = "*.crates.io" }]
```

Select one with `profile = "…"` in `config.toml`, `--profile build`, or `/profile build` in a
session.

### Running a trusted binary with `run_binary`

A launch profile runs one exact executable, in a separate foreground sandbox, with literal
arguments and no shell. The definition is protected configuration; permission to invoke it is a
separate grant:

```toml
[launch-profiles.gh]
executable = "/usr/bin/gh"
read = ["~/.config/gh/hosts.yml", "~/.config/gh/config.yml"]
net = ["api.github.com"]
timeout-secs = 30

[agent]
run-binary = [{ profile = "gh" }]
```

The workspace is readable and writable by default (`read-workspace`, `write-workspace`); programs a
profile launches are listed in `helpers`. This is not a way to hide a credential from a program it
can run: grant a profile only when you trust its whole command surface. `saddle check` prints what is
in force. `docs/settings.md#launch-profiles` covers helpers, `verify-sha256`, `ssh-hosts`, and the
per-workspace overrides.

## Plugins and extensions

A plugin is a WASI component with a manifest that declares its scope ceiling. It owns its tool
schema, workflow and presentation; the core keeps path resolution, permissions, audit, sandboxing
and privileged data. Nothing a plugin offers is reachable beyond the scopes the manifest declares
*and* the grants a person writes for it.

```toml
# reader/plugin.toml
id = "reader"
version = "0.1.0"
component = "reader.component.wasm"
scopes = ["tool", "fs-read", "tool-read"]
```

Build and install (the loader ships a componentizer example):

```sh
cargo build -p saddle-tools --example read --target wasm32-unknown-unknown --release
cargo run -p saddle-loader --example componentize -- module.wasm reader/reader.component.wasm
saddle plugin install reader/plugin.toml
```

Installation copies the pinned artifact into `$XDG_CONFIG_HOME/saddle/plugins` and prints the
SHA-256. It does not enable the plugin or grant it anything; add both to `config.toml`:

```toml
[plugins.reader]
enabled = true
manifest = "reader/plugin.toml"
sha256 = "…"

[plugin.reader]
tool = {}
fs-read = [{ root = "workspace" }]
tool-read = { tools = ["read"] }
```

Then:

```sh
saddle plugin list
saddle plugin call reader '{"path":"src/main.rs"}'
saddle tool plugin__reader__read '{"path":"src/main.rs"}'
```

The SDK crate is `saddle-sdk` and the canonical WIT is in `saddle-wit`. Besides tools, plugins can
subscribe to events, veto actions, contribute context, persist private KV, ask the model and draw
read-only panels — each a separate interface with its own grant. `docs/plugins.md` documents every
one, with the budgets and the delegation rules.

## The rules that matter

- Every allow and every deny is audited; `saddle audit verify` checks the chain.
- A tool's path is a location under a root, and the grant on that root decides. A sandbox path list
  is not a grant.
- A command's network is its own namespace plus one proxy in this process, which decides every
  connection against the same grants — so `curl` works exactly where a `net` grant allows.
- Nothing the agent runs can write a policy file, so `saddle check` is the whole review.
