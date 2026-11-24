# Configuring Saddle

This is the guided tour: what each part of the configuration is for, what it can and cannot do, and
what it costs you when you get it wrong. It is not the key-by-key reference — that is
[`docs/settings.md`](settings.md), which is also what the `config_help` tool answers with inside a
session. When the two disagree, `saddle check` is the tie-breaker: it prints what is actually in
force for one workspace.

Read this in order if you are setting Saddle up for the first time. The sections are roughly:
the authority model, the grant scopes, the layers that add to them, the two kinds of sandbox, and
then the settings that have no authority at all.

## 1. The authority model in one page

Everything Saddle decides is a lookup over **grants**, and the default answer is no. There is no
ordering, no fallthrough, and no deny rule: a scope with no grant is refused, and an exclusion lives
*inside* the grant it narrows. Six ideas carry the whole design.

**A principal.** The agent is one principal; every plugin is another; an MCP server is a third. Each
decision names the principal it was made for, and one principal cannot use another's grants. A
plugin holding `fs-read` for a root does not get the agent's `fs-read`, and an agent grant never
silently becomes a plugin grant.

**A scope.** `fs-read`, `fs-write`, `exec`, `net`, `tool-read`, `tool-write`, `transcript-read`,
`run-binary`, `mcp`, and the extension scopes (`tool`, `ui`, `veto`, `context-contribute`,
`persist`, `model-invoke`, `event-publish`, `event-subscribe`). Each grant in a scope says which
*resource* — which root, host, tool path, server, profile — it covers.

**A root.** The file tools decide over named roots: the workspace (always), `tmp`, and whatever
`[roots]` names. A path that is inside no root is refused as unaddressable, however it is spelled.
This is the reason a project cannot quietly grant itself `~/notes`: it has no name to grant.

**A grant.** Written in protected configuration, and always more specific than the scope name: a
root with glob exclusions, a host with a scheme and port, a server with exact tool names.

**A layer.** `[agent]` → `[profiles.<name>]` → this workspace's own file → `--grant`/`--revoke`.
A layer names what it changes and inherits everything else.

**An audit record.** Every allow and every deny is written to a hash-chained log with the resolved
arguments. `saddle audit verify <log>` checks the chain. There is no "quiet" mode.

What the model can therefore never do: widen its own grants, write a policy file, read the audit log,
read `auth.json`, or reach a path that no root contains. The configuration files live outside every
root precisely so that a command the agent runs cannot reach them (a workspace-writable policy file
would be no policy at all).

## 2. Where configuration lives

| file | holds | who writes it |
| --- | --- | --- |
| `$XDG_CONFIG_HOME/saddle/config.toml` | every setting, including `[agent]` base grants | you, `/settings`, `/permissions`, `ctrl+s` |
| `$XDG_CONFIG_HOME/saddle/policies/--<path>--.toml` | one workspace's grants, `[sandbox]` lists and launch-profile overrides | you, or `/permissions` |
| `$XDG_STATE_HOME/saddle/auth.json` | keys and OAuth tokens | `/login` only |
| `$XDG_STATE_HOME/saddle/sessions.db` | saved conversations | the session |
| `$XDG_STATE_HOME/saddle/audit/` | one chain per run | the session |

The workspace file's `workspace = "..."` line is the authority for which directory it governs; the
filename is only a readable encoding and is allowed to be lossy. A session refuses a file whose line
names another directory — applying one project's grants to another is exactly the accident this
prevents. The same file is used by every session in that workspace.

`/reload` re-reads configuration, themes, instruction files, skills and budgets in a running
session; grants take effect immediately and the model is told on its next request. `[sandbox]`
changes are applied to *new* commands. Background tasks are stopped before grants are replaced,
because they were started under the old ones.

### What can be written where

- `config.toml`: everything.
- Workspace file: `[agent]` grants, `[plugin.<id>]` grants, `[sandbox]` lists, and
  `[launch-profiles.<name>]` field overrides. Top-level keys such as `provider` or `profile` are not
  workspace settings.
- The checkout: nothing. A `saddle.toml` in the repository is not read, and a session says so once.

## 3. The scopes, one at a time

### `fs-read` and `fs-write`

```toml
[agent]
fs-read  = [{ root = "workspace", exclude = [".env*", "secrets/"], why = "reads the project" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the project" }]
```

- **Boundary.** A path is mapped to the root containing it and the grant on that root decides. No
  root contains it means no answer but "unaddressable". Both the requested path and the resolved one
  are decided, including dangling links; a link to an absolute path is refused.
- **Exclusions** are glob patterns inside the grant. They narrow the file tools and plugin host
  calls. They do **not** narrow `exec` or a launch profile's workspace claim — landlock cannot carve
  a path out of a granted directory.
- **Nothing inside a root is reserved.** `.git/config` and `.git/hooks` are ordinary files; a root
  grant reaches them, from the file tools and from commands, because version control writes them as
  ordinary work. If a path must stay unwritable it has to be outside every root.
- `write` needs `read` for existing-file edits, because the host reads a preimage to guard the
  change. Granting write without read is legal and produces refusals that say so.

### `exec`

```toml
[agent]
exec = { why = "runs builds and tests", env = ["EDITOR"], timeout-secs = 300 }
```

- **Boundary.** `exec` is permission to run a command, not to reach files: what the command may read
  and write comes from the granted roots plus `[sandbox]`. A principal without `net` gets a namespace
  with no route out, even from `curl`.
- **Environment.** A child inherits a minimal environment. Any variable that may hold a provider key
  is stripped even if `env` names it. `env` only ever adds names back.
- **Deadline.** `timeout-secs` caps one command; every tool also accepts a per-call `timeout_secs`,
  which can only shorten the protected limit.
- **Background.** `bash` with `background: true` starts a session-owned task; `tasks` reports, stops
  or waits on it. A task belongs to the principal that started it — one plugin cannot read or cancel
  another's.
- **Overlap.** Independent commands may run at the same time. Saddle no longer serializes the
  workspace; sequencing commands that share files or build outputs is the caller's job.
- **Boundary that surprises people.** `stat`, `chmod`, `utime` and `setxattr` are outside what
  landlock controls, so a command can notice that an ungranted file exists. A secret has to be
  somewhere the user cannot traverse at all, not merely outside every root.

### `net`

```toml
[agent]
net = [
  { host = "*.crates.io", why = "fetches dependencies" },
  { host = "api.example.com", scheme = "https", port = 443 },
  { host = "github.com", scheme = "ssh" },        # only for named launches with ssh-hosts
]
```

- **Boundary.** A grant is a host name (or `*.suffix`, or `*`), a scheme and a port. `*` covers
  public addresses only: loopback and private space need a grant that names the address itself.
- A command's network is its own namespace with one door: a proxy in this process, which reads the
  name out of the bytes the client sends and decides it against these same grants. A connection that
  names no host is refused.
- `fetch` may send headers — a secret belongs there, never in a query string — and the audit log
  records header names but never values.
- `ssh` is not HTTP. It is only usable from a named launch whose profile lists `ssh-hosts`, and only
  when the caller also holds a `scheme = "ssh"` grant for that host.

### `tool-read` and `tool-write`

Directories outside every root that the file tools may nonetheless reach are declared under
`[sandbox]` with a `tools` list. `tool-read`/`tool-write` grants are *derived* from those claims; you
do not write them by hand, and no policy can name such a directory. They exist so that a shared
read-only tree (`~/.agents`, a toolchain) can serve `read`/`grep` without becoming a root any policy
could widen.

### `transcript-read`

`session_query` runs read-only SQL over the conversation's messages and per-request usage. A bare
grant covers every conversation of *this workspace*; `sessions = "self"` covers only the conversation
in hand. Results are capped (100 rows, 50 KiB), execution is bounded, and there is no path to the
saved database itself and no mutation.

### `run-binary`

One grant per exact launch-profile name:

```toml
[agent]
run-binary = [{ profile = "gh", why = "GitHub CLI" }]
```

- **Boundary.** This is authority to invoke *that definition*, nothing else. It does not grant
  `exec`, does not give `bash` anything, and does not make the profile's files readable to any other
  tool. Two profiles with the same executable are two different permissions on purpose.
- `[]` clears the inherited list; `["-gh"]` subtracts one entry. The effective grant must name a
  defined profile, and `saddle check` reports the pair.

### `mcp`

```toml
[agent]
mcp = [{ server = "exa", tools = ["web_search_exa"], why = "search", net = [...] }]
```

Every tool is named explicitly — never a pattern — and the server's reach is narrowed from the agent's
own, so a grant cannot lend reach the agent lacks. `exposure` decides whether a tool is declared up
front (`direct`), only after `tool_search` finds it (`deferred`, the default), or never (`hidden`).

### Extension scopes

`tool`, `ui`, `veto`, `context-contribute`, `persist`, `model-invoke`, `event-publish` and
`event-subscribe` are the interfaces a plugin can hold (see §7). Two of them spend real resources:
`model-invoke` may carry `tokens-per-session` and `cost-per-session` budgets that are charged like the
agent's own spending; both are optional, and a grant without them caps nothing, so a worker is not
given a default ceiling. `persist` gives a plugin private key-value storage isolated by workspace
and plugin identity.

## 4. Permission profiles

```toml
profile = "review"        # what a new session starts on

[agent]
fs-read = [{ root = "workspace" }]

[profiles.review]
net = []                  # no network at all

[profiles.build]
exec = { timeout-secs = 300 }
net  = ["-api.example.com"]   # the base list, minus one host
```

- **Model.** A profile is a *layer of agent grants*. It cannot change `[plugin.<id>]` grants or
  `[sandbox]` lists, which stay global; it cannot introduce a root, a launch definition or a
  provider. It is a way to keep two ways of working without editing `[agent]` back and forth.
- **Inheritance.** A scope a layer names replaces that scope (lists replace, tables merge key by
  key). Everything it does not name comes from the layer below.
- **Subtraction.** A list written wholly as `-name` entries removes those entries from the inherited
  list; the name is the entry's identity (the host of a `net` grant, the root of a filesystem
  grant, the server of an `mcp` grant, the profile of a `run-binary` grant). A name nothing inherits
  is an error, so a typo is never a silent no-op.
- **Tables cannot be dropped**, only replaced: to have a profile that runs nothing, leave `exec` out
  of `[agent]` and grant it in the profiles that need it.
- `none` names `[agent]` as written. A misspelt name is refused with the list of defined ones.
- `--profile <name>` applies for one run and writes nothing; `/profile <name>` switches the session,
  telling the model on its next request and stopping background tasks first.

## 5. Launch profiles (`run_binary`)

A launch profile is a protected definition of one executable and its bounds. The definition and the
permission to use it are separate, and both are needed.

```toml
[launch-profiles.gh]
executable = "/usr/bin/gh"
read = ["~/.config/gh/hosts.yml", "~/.config/gh/config.yml"]
helpers = []
verify-sha256 = true
net = ["api.github.com"]
ssh-hosts = []
read-workspace = true
write-workspace = true
timeout-secs = 30
```

| field | boundary |
| --- | --- |
| `executable` | opened without following symlinks, must be a regular native ELF, and must be outside anything the agent can write. It is the only entry point; `argv` never names a program. |
| `read` | extra read-only paths inside the launch. Every path must exist. Not a grant to any other tool, and listing a binary here does not make it executable. |
| `helpers` | exact executables the program may launch. Empty means the program may not exec anything further. A helper runs in the same sandbox and cannot add reach. `run_binary` allows helpers by default; `"allow_children": false` denies them for one call. |
| `verify-sha256` | records a session baseline for the entry point and helpers and refuses a launch whose bytes changed. Off by default; `/reload` does not refresh an existing baseline — restart after a verified update. |
| `net` | exact HTTPS hosts on port 443. Effective only when the caller also holds a matching `net` grant. |
| `ssh-hosts` | exact hosts for port-22 tunnels. Effective only with a matching `scheme = "ssh"` grant, and only through an HTTP CONNECT proxy the program's SSH client is told about. |
| `read-workspace` / `write-workspace` | on by default. The claims cover the whole workspace, including `.jj`/`.git` metadata, without file-tool exclusions. A write claim never grants execution, so files written there cannot become helpers. |
| `timeout-secs` | the profile's own ceiling; the request's deadline can only shorten it. |

**What a launch cannot do.** It cannot run a shell, take the caller's environment, choose a working
directory in the checkout, write outside the workspace, reach a host the profile does not list, or
use a program the profile does not list as a helper. It cannot hide a credential from a program it
can run: treat the grant as trusting the program's whole command surface.

**Trust model.** The entry point and helpers are trusted by exact path in protected configuration,
not by Unix ownership, so a user-installed binary is eligible (D188). `verify-sha256` closes the gap
between "protected path" and "the bytes I approved" for the duration of one session; it is not a
publisher signature and does not check libraries. Because the workspace is writable by default, a
profile that can also read a credential could copy it into the workspace where the agent can read
it — grant a writable profile only the read reach its programs need.

## 6. The command sandbox (`[sandbox]`)

`[sandbox]` is what a **child process** reaches under landlock. It is not a grant and it widens no
tool.

| key | what it is |
| --- | --- |
| `read` / `write` | extra paths a command may read or write, beyond its roots and the built-in list |
| `include` | rules that walk a base directory, keeping what does not match `except` |
| `include-defaults` | the built-in narrow includes: `~/.cargo`, `~/.rustup`, `~/.config` only, with known credential and browser stores withheld |
| `tools-defaults` | whether `~/.agents` is readable by `read`, `ls`, `find`, `grep` |

- An entry may be a bare path (command only) or a table adding `tools` — a *claim*, not a root, that
  no policy can name and that `check` lists separately.
- A workspace file may append (`+path`), remove (`-path`) or replace (unprefixed) the inherited
  lists. Subtraction cannot carve a hole in a parent claim.
- An entry naming or containing the config or state directory is refused: that would put the policy
  in reach of the thing it governs.
- **Boundary.** A sandbox list is one allow-list per process, with no per-call decision and no record
  per access. `bash` therefore reaches more than the file tools do: `~` minus the withheld stores.
  Exclusions in a grant do not narrow it.

## 7. Plugins

Three separate decisions, and all three are needed:

1. **Installed** — `saddle plugin install <manifest>` copies the pinned artifact into
   `$XDG_CONFIG_HOME/saddle/plugins` and prints its SHA-256. Nothing is enabled.
2. **Enabled** — `[plugins.<id>] enabled = true`, `manifest = "…"`, `sha256 = "…"`.
3. **Granted** — `[plugin.<id>]` with scopes of its own, in the same vocabulary as `[agent]`.

On top of that, the plugin's manifest declares a `scopes` ceiling. Effective authority is the
narrowest of the three: the agent's current reach, the manifest ceiling, and the explicit plugin
grant. A sibling plugin's grant never supplies reach, and a post-load permission change is enforced
per call without recompiling or repinning the artifact.

- A plugin's tool is called `plugin__<id>__<tool>`; `saddle plugin list` says which of installed,
  enabled, granted and loadable is missing.
- Hook-only plugins need no tool export. `veto` may only deny or abstain and fails closed;
  `context-contribute` appends attributed items; `persist` is private transactional storage;
  `ui` panels are read-only closed widgets.
- `model-invoke` spends host credentials under shared token and cost budgets; a failed or unmetered
  request keeps its reservation.
- Custom events carry host-assigned publisher identity and the handler's own grants, never the
  publisher's.

The full contract — manifests, budgets, mutation rules, the SDK and the componentizer — is
[`docs/plugins.md`](plugins.md).

## 8. MCP servers

```toml
[mcp.exa]
description = "web search"
url = "https://mcp.exa.ai/mcp"
timeout = 60
exposure = "deferred"

[agent]
mcp = [{ server = "exa", tools = ["web_search_exa"] }]
```

- Exactly one of `command` (stdio; `args`, `env`, `cwd`) and `url` (streamable HTTP) describes a
  server. A table naming both or neither is refused when the session opens.
- Every request of an HTTP server is decided against that server's own narrowed `net` reach and
  recorded with the server named. A stdio server runs as a sandboxed child.
- `exposure` decides what reaches the prompt: `deferred` tools arrive only when `tool_search` declares
  them (a declaration is saved on the branch, so resume and forks keep it, and restoring one
  re-checks the grant), `direct` declares up front, `hidden` is never reachable.
- `codemode` is refused by name: Saddle calls tools directly rather than generating code that calls
  them. MCP prompts, resources and images in results are refused rather than silently empty.
- `saddle mcp auth <server>` performs a discovered OAuth sign-in, with tokens in `auth.json`;
  `saddle mcp logout` removes them; `/mcp` reports and restarts for one session.

## 9. Providers and credentials

`[providers.<name>]` declares an endpoint or overrides a built-in one, and `api` says which wire
format it imitates (`openai`, `anthropic`, `openai-responses`). A compatible server is that table
plus a `base-url`; it does not inherit a built-in provider's prices.

- **Keys are never in `config.toml`.** `/login` stores them in `auth.json` (mode protected), or they
  come from the environment variable named by `api-key-env`. An environment variable beats a stored
  key, and no child process ever inherits one.
- `headers` is for gateway or account headers; treat anything secret there as readable by anyone who
  can read the config file.
- Per-session choices (`/model`, `/thinking`, `/settings` for `max-tokens`) never rewrite the file
  unless you save them (`ctrl+s` in the model list).
- `usage-url` and `search-url` are how the status line learns what is left and how `websearch` uses
  the provider's own search before falling back to a second key.

## 10. Settings that carry no authority

These change behaviour or presentation only. None of them can widen a grant, and a wrong value is a
display or spending problem rather than a permission one.

- `[ui]` — theme, reasoning display, expansion, mouse, notifications, image protocol, the status-line
  template (`{provider} {model} {input} {output} {cache} {cachehit} {ctx} {window} {percent} {cost}
  {quota} {tasks} {workspace}`), and math scale.
- `[keys]` — only the moves with no printable default: the ends of the branches in `/tree`.
- `[themes.<name>]` — colours by role, `extends` a built-in, `vars` for shared values.
- `[context]` — which instruction files load (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md` fallback).
- `[skills]` — discovery on/off and how many are advertised. A skill is *instructions, never
  authority*: its `allowed-tools` line is ignored, and loading one is an ordinary `read` through the
  policy.
- `[compaction]` — when the older part of a conversation is replaced by a summary. Each of the three
  ways spends a request, so these are also spending settings.
- `[tool-limits]` — resource and presentation budgets. A budget refuses work that is too large, never
  work that is not allowed; it is not a second permission system. `/reload` applies a change without
  restarting.
- `max-steps` — how many requests one prompt may take; unset means no limit.
- `allow-sensitive` — skips the approval a workspace needs when it is, or contains, the home or config
  directory. A bearer permission: leave it off unless the machine holds no secrets.

## 11. Recipes

**Read-only review.** No shell, no network, no writes. Leave `exec` and `fs-write` out of
`[agent]` altogether — a table scope such as `exec` can only be replaced, never dropped, so a profile
cannot take it away:

```toml
[agent]
fs-read = [{ root = "workspace" }]

[profiles.review]
net = []
```

**Build and test, no internet except crates.io:**

```toml
[profiles.build]
exec = { timeout-secs = 300 }
net  = [{ host = "*.crates.io" }]
```

**One trusted CLI, nothing else:** define the profile (§5), then
`run-binary = [{ profile = "gh" }]` under `[profiles.github]`. Keep the workspace claims default so
the tool can read its repository metadata; clear `read`/`write` when it needs neither.

**A third-party tool for one directory only:** install and enable the plugin, then grant
`fs-read = [{ root = "notes" }]` under `[plugin.<id>]` with `[roots] notes = "~/notes"`. The agent
does not need that root, and the plugin cannot reach the workspace unless you also grant it.

## 12. When a decision surprises you

| symptom | usually |
| --- | --- |
| `denied:` on a path | the path is in no root, or the grant on its root excludes it. `saddle check` prints roots, grants and `tool paths`. |
| `denied:` on a host | no `net` grant covers that host/scheme/port; `*` never covers loopback or private addresses. |
| a tool is missing | `[standard-tools]` has no entry for it (or its context entry is absent). `saddle tools --config` prints the table to add. |
| a plugin's tool is missing | installed, enabled and granted are three different things; `saddle plugin list` says which is missing. |
| `no profile named …` | the name is not defined; the error lists the ones that are, and `none` is `[agent]`. |
| a launch is refused before it starts | a `read` path does not exist, a helper is not listed, the workspace write/read claim overlaps protected config, or the SHA-256 baseline does not match. |
| a workspace is refused at open | it is or contains the home or config directory; `saddle session allow <id> --workspace <path>` approves one id. |
| `Permission denied` at startup | the config or state directory is not writable by this process — common inside a sandbox with borrowed `XDG_*` variables. |

Two commands answer most questions without a session: `saddle check` (roots, grants, profiles, launch
profiles, sandbox reach, network, budgets) and `saddle audit verify <log>` (that the audit chain of a
past run is intact).
