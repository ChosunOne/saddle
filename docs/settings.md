# Saddle settings

Every setting a person can change, where it lives, and what changes when it does. This is the
reference `config_help` returns, so it has to answer for a session in any workspace: the files it
names are outside every root, which means an agent can read this and say what to change but can
never read or write the files themselves (D16, D42). `saddle check` prints what is in force for one
workspace, and is the first thing to run when behaviour and this document disagree.

A change to `config.toml` or to a workspace's grants file takes effect in the next session.
`/reload` re-reads the settings, the theme, the instruction files and the skills in a running
session (D70); `/permissions` and `/settings` write the file for you rather than asking a person to
edit it; `--grant` and `--revoke` change grants for one run and write nothing (D63).

## files

Saddle keeps two directories, both outside every root:

| file | what it is |
| --- | --- |
| `$XDG_CONFIG_HOME/saddle/config.toml` | every setting below, in one file; `~/.config/saddle` when the variable is unset |
| `$XDG_CONFIG_HOME/saddle/AGENTS.md` | the global instruction file, until `[context] global` names another (D32) |
| `$XDG_CONFIG_HOME/saddle/policies/--<path>--.toml` | one protected overrides file per workspace (D63, D158, D184) |
| `$XDG_STATE_HOME/saddle/sessions.db` | the saved conversations, as a tree per workspace (D53) |
| `$XDG_STATE_HOME/saddle/auth.json` | the keys and OAuth tokens `/login` stores (D85) |
| `$XDG_STATE_HOME/saddle/models.json` | each endpoint's model list, context windows and prices (D34) |
| `$XDG_STATE_HOME/saddle/audit/` | one hash-chained log per run: every allow and every deny (D10) |

The workspace file's name is a readable encoding of the directory and is allowed to be approximate:
the `workspace = "..."` line inside it is what counts, and a session refuses a file naming a
different directory. It holds `[agent]` grants, `[plugin.<id>]` grants and `[sandbox]` lists; what it
does not mention is taken from the base, and what it does mention replaces that scope for this
directory, wider or narrower. It can also override individual [launch-profile fields](#launch-profiles).
`/permissions` creates it on first use, and `saddle check` names it.

Neither directory is a root, and a path in one cannot become one. That is deliberate: a policy file
inside the workspace would be writable by anything the agent runs (D42). `saddle check` prints the
resolved form of each of these paths for the workspace it is run in, which is the answer when the
environment variables are not what you think.

## top-level

| key | default | what it does |
| --- | --- | --- |
| `provider` | unset | the provider a new session starts with; unset asks on first run |
| `max-steps` | unset, no limit | model requests one prompt may take before the loop stops and waits (D37) |
| `allow-sensitive` | `false` | skip the allowance a workspace that holds credentials needs (D60) |
| `profile` | unset, `[agent]` as written | which `[profiles.<name>]` set of grants a session starts on; `none` names `[agent]` (D175) |

`/model` chooses for one session and never writes this file; ctrl+s in the model list saves both the
provider and the model, and is the only thing that changes what a new session starts with (D91).

## agent

`[agent]` is the base grants: what the agent may do in every workspace. A workspace's own file
replaces a scope it names. `saddle check` prints the result, and `--grant`/`--revoke` change one run.

| scope | written as | what it allows |
| --- | --- | --- |
| `fs-read` | list of `{ root, exclude, why }` | reading a root's paths with the tools, minus the glob `exclude` patterns (D22, D65) |
| `fs-write` | the same | creating, editing and removing them (D140) |
| `exec` | `{ why, env, timeout-secs }` | running commands in the sandbox; `env` names variables to pass on, `timeout-secs` caps one command (D19) |
| `net` | list of `{ host, scheme, port, why }` | HTTP(S), by host name, `*.suffix` or `*`; loopback and private addresses need a grant naming the address itself (D28, D40) |
| `tool-read` | `{ tools, why }` | `read`, `ls`, `find` and `grep` on a `[sandbox]` tool path — this is built from those declarations and is not widened by hand (D66, D141) |
| `tool-write` | the same | `write`, `edit` and `patch` on one |
| `transcript-read` | `{ why, sessions }` | `session_query` over this workspace's conversations; `sessions = "self"` narrows it to the conversation in hand (D92, D163) |
| `mcp` | list of `{ server, tools, why, fs-read, fs-write, net }` | one server's named tools; every tool is named, never a pattern, and its reach is narrowed to the agent's (D103, D105) |
| `model-invoke` | `{ tokens-per-session, cost-per-session, why }` | requests a plugin may make on its own behalf (D156). Both budgets are optional: a grant written without them is the capability alone |
| `event-publish`, `event-subscribe` | `{ why }` | publishing and subscribing to custom plugin events (D155) |
| `context-contribute`, `veto`, `tool`, `ui`, `persist` | `{ why }` | the other typed extension interfaces (D156) |

A scope with no grant is denied, and there is no deny rule: a decision is a lookup over grants, with
no ordering and no fallthrough. `why` is free text shown by `saddle check` and written to the audit
log beside the decision (D41).

```toml
[agent]
fs-read  = [{ root = "workspace", exclude = [".env*"], why = "reads the project" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the project" }]
exec     = { why = "runs builds and tests", timeout-secs = 60 }
net      = [{ host = "*.crates.io", why = "fetches dependencies" }]
transcript-read = { why = "reads this workspace's conversations", sessions = "self" }
```

A workspace file is the same table under the same heading, with `workspace = "..."` above it:

```toml
workspace = "/home/me/project"
version = 1

[agent]
net = [{ host = "api.example.com", why = "the project's API" }]
```

`version = 1` is the base document's own line and is accepted in either file.

### profiles

A profile is a named layer of grants, so switching what the agent may do does not mean editing
`[agent]` and editing it back (D175, D176). The layers apply in this order:

```
[agent]  →  [profiles.<name>]  →  this workspace's own file  →  --grant / --revoke
```

A layer names what it changes and **inherits every scope it does not name**: a scope the later layer
names replaces that scope (a list replaces the list, a table is merged key by key), and the rest come
from the layer below. So the base holds what every profile shares, and each profile says only what it
adds.

```toml
profile = "build"            # the default; `none` is [agent] as written

[agent]                      # what every profile shares
fs-read  = [{ root = "workspace" }]
fs-write = [{ root = "workspace", exclude = [".env*"] }]

[profiles.build]             # the same, plus running commands and one host
exec = { timeout-secs = 300 }
net  = [{ host = "*.crates.io" }]

[profiles.offline]           # the same, and no network even if the base had one
net = []

[profiles.review]            # the same, without one host the base grants
net = ["-api.example.com"]
```

| how | what it does |
| --- | --- |
| `profile = "name"` | what a new session starts on; unset means `[agent]` |
| `--profile <name>` | one run only, nothing written |
| `/profile` | report the set in force and the ones defined |
| `/profile <name>` | switch this session at once — the model is told on its next request, and background tasks stop first because they were started under the replaced grants (D153) |

A list scope can also **subtract** single entries, the way a `[sandbox]` list subtracts a path
(D177): written wholly as `-name` entries, it is the inherited list without those entries, so a layer
takes one grant away without restating the ones it keeps. The name is the entry's identity — the
**host** of a `net` grant, the **root** of an `fs-read`/`fs-write` grant, the **server** of an `mcp`
grant — and every layer has the form, so a project file subtracts from what a profile left.

```toml
[profiles.review]
net     = ["-api.example.com"]   # that host goes, the other hosts stay
fs-read = ["-scratch"]           # every grant for that root goes
mcp     = ["-exa"]               # that server goes
```

A list is written wholly as names or wholly as entries; `net = []` still empties a scope rather than
subtracting, and `+name` is refused (an added grant carries a scheme, a port and a `why` a bare name
cannot). A name nothing inherits is an error naming what the inherited list does hold, so a typo is
not a silent no-op. A `-` in `[agent]` itself is refused: the base grants are what everything
inherits, and there is nothing below them to subtract from.

A scope written as a table — `exec`, `transcript-read` — can only be replaced, never dropped. To have
a profile that runs nothing, leave `exec` out of `[agent]` and grant it in the profiles that need it.

Names are 1-64 letters, digits, `-` or `_`, and `none` is reserved for `[agent]` as written. A name
that is not defined is refused, at load and at a switch, with the defined ones listed: a mistyped
switch never falls back to a wider layer. `saddle check` prints the profile in force and the
alternatives. `[plugin.<id>]` grants and `[sandbox]` lists stay global — a profile is the agent's
grants, not a second copy of the command sandbox's authority.

## roots

`[roots]` names directories outside the workspace so a policy can grant them to the tools: a root is
what `read`, `ls`, `find`, `grep`, `write`, `edit` and `patch` decide over, and a path outside every
root is refused as unaddressable, however it is named (D65).

```toml
[roots]
notes = "~/notes"
```

Never make a toolchain or shared directory a root (`~/.cargo`, `~/.agents`): a root can be named by
any workspace's policy, so a repository's file could then grant write to it and plant a program that
runs later, outside the sandbox (D62).

## sandbox

`[sandbox]` is what a **command** reaches beyond the built-in list, under landlock. It is not a
grant: it widens no tool, it is one allow-list per child process, and exclusions inside a grant do
not narrow it (D20).

| key | what it is |
| --- | --- |
| `read` | paths a command may read, in addition to its granted roots and the built-in includes |
| `write` | paths a command may write, in addition to its granted roots; `~/.cargo/registry` and `~/.cargo/git` by default (D62) |
| `include` | rules that walk a base directory and exclude what matches, instead of listing each path (D62) |
| `include-defaults` | whether the built-in narrow includes apply: `~/.cargo`, `~/.rustup` and `~/.config` only, with known credential and browser stores withheld (D160) |
| `tools-defaults` | whether `~/.agents` is readable by `read`, `ls`, `find` and `grep` for skills (D66) |

An entry is a bare path, or a table that also names the file tools allowed on it:

```toml
[sandbox]
read = [
  "~/notes",                                              # a command only
  { path = "~/shared", tools = ["read", "ls", "find", "grep"] },
]
```

A directory reachable by a tool is a *claim*, not a root and not a grant: no policy can name it, and
`check` lists them as `tool paths` (D66). A typo in a tool name is an error, not a silent no-op, and
a path in `[sandbox]` that names or contains the config or state directory is refused.

A workspace file may override these lists: `+path` appends to the inherited list, `-path` removes,
an unprefixed list replaces it, and a mixed list is an error (D157, D158). List subtraction is not a
deny rule and cannot carve a hole in a parent claim.

## launch-profiles

Launch profiles (D184) run through the single `run_binary` tool. They do not give existing tools
access to the listed files. Define a default in `config.toml`:

```toml
[launch-profiles.gh]
executable = "/usr/bin/gh"
read = ["~/.config/gh/hosts.yml"]
net = ["api.github.com"]
timeout-secs = 30
```

In the protected workspace override file, keep the canonical `workspace` line and set only the
fields you want to change:

```toml
[launch-profiles.gh]
read = []
```

The omitted timeout inherits; the empty list clears the inherited extra read paths. A nonempty list
replaces the whole inherited list; `+`/`-` list operators are not supported here. Paths must be
absolute or start with `~/`, without `..`. `timeout-secs`, if set, is a positive integer. To choose
a different executable, define a new profile by name. `saddle check` prints the effective definition
and names the fields from the workspace file. No settings in the checkout define a profile.

Named launches can read and write the current workspace by default (D190, D193), including
repository metadata such as `.jj` and `.git`. These are separate from the `read` list:
`read = []` clears only extra paths. Set `read-workspace = false` or `write-workspace = false` in
the profile or its protected workspace override to opt out of either; a write claim implies
reading the workspace, so removing both needs both set to false. The claims cover the
workspace tree without applying file-tool exclusions, because landlock cannot carve a path out of
a granted directory; a `write` claim therefore reaches paths such as `.env` that the file tools
may not. A write claim never grants execution, so a file written there is still refused as an executable.
Neither claim extends outside the tree through symlinks, and neither changes permissions
for other tools or for `bash`. A program that can read a credential can copy it into the
workspace, where the agent can read it, so grant a writable profile only as much read reach as its
trusted programs need. If the workspace overlaps Saddle's protected config or state, the launch
refuses the claim.

If the program launches other programs, list the exact binaries in `helpers` (D186). For example,
Git's `git-remote-https` may be a symlink to `/usr/lib/git-core/git-remote-http`; list the real
file, not the symlink. A workspace can inherit, replace, or clear `helpers`, just like `read`.
`run_binary` allows configured helpers by default. Pass `"allow_children": false` to deny all
further executable launches for that call. An empty `helpers` list also denies further launches;
`"allow_children": true` cannot override it. This controls program execution, not fork or thread
creation. Every helper runs inside the same sandbox and cannot add access. A path in `read` does
not make it executable.

Set `verify-sha256 = true` to check for binary changes during a Saddle session (default: `false`).
Saddle records SHA-256 digests of the executable and configured helpers when the session opens.
Each launch checks the executable and enabled helpers before running; a mismatch or unavailable
baseline refuses the launch. `/reload` can enroll new paths but does not refresh an existing
baseline. After an intentional update, verify the new binary and restart Saddle. The protected
workspace override can inherit or replace this setting. This is a session baseline, not a
publisher signature or a check of libraries; keep the files protected from concurrent writers.

To grant a launch profile, add `run-binary` to the global `[agent]`, a named permission profile,
or the protected workspace `[agent]`. For example, to grant `gh` only in the `github` permission
profile (unless a workspace overrides it):

```toml
[profiles.github]
run-binary = [{ profile = "gh" }]
```

Select it with `--profile github` or switch to it in a session with `/profile github`. This selects
the grant, not a different executable or sandbox. An effective grant must name a defined launch
profile. Permission profiles and workspace grants can replace the inherited list, clear it with
`run-binary = []`, or subtract one entry with `run-binary = ["-gh"]`. Use
`--grant run-binary:gh` or `--revoke run-binary` to change the grants for one run. `saddle check`
shows the result independently of `exec`. This grant does not give `bash` any new access.

In an existing configuration, select the shipped tool with
`run_binary = ["run-binary", "net"]` in `[standard-tools]`. Network is optional: list exact
HTTPS hosts on port 443 in the profile's `net` field, and grant those hosts in the usual `[agent]`
`net` list. Both are required. If the command proxy cannot start in an isolated namespace, the
binary runs without network. Omit `net` at both levels for a purely local profile.

```sh
saddle --profile github tool run_binary '{"profile":"gh","argv":["api","user","--jq",".login"]}'
```

### SSH for named launches

`ssh-hosts = ["github.com"]` in a launch profile permits that destination on port 22 only when
its caller also has a matching `{ host = "github.com", scheme = "ssh" }` entry in `net`. The
standard tool must delegate `net` as above. This is a separate grant from HTTPS; there is no SSH
access by default. OpenSSH needs an HTTP CONNECT-capable `ProxyCommand`, for example the following
in a dedicated SSH client config on a machine with Ncat installed:

```sshconfig
Host github.com
    User git
    Port 22
    ProxyCommand /usr/bin/ncat --proxy 127.0.0.1:3128 --proxy-type http %h %p
    BatchMode yes
    IdentitiesOnly yes
    IdentityAgent none
    IdentityFile ~/.ssh/saddle-deploy-key
    UserKnownHostsFile ~/.ssh/saddle-known-hosts
    StrictHostKeyChecking yes
    UpdateHostKeys no
```

The profile must authorize the actual helper executables (for `jj`, Git, OpenSSH, the shell used
by `ProxyCommand`, and Ncat), and the client config, dedicated key, and verified known-hosts file
must be readable inside the profile. Use a repository-scoped deploy key and verify the server's
host key independently. Anyone who can invoke the profile may be able to read a key it can read.
The proxy does not expose a local SSH agent and does not force a remote to use SSH.

`argv` excludes the executable. Arguments are literal and unrestricted by Saddle; a CLI or its
helpers can return a credential they can read. Use a dedicated, appropriately scoped credential if
needed, and don't grant a profile whose full command surface you don't trust. The tool runs
foreground only. Its working directory is the current workspace when the profile claims it — the
default, by read (D190) or write (D193) — so a repository tool finds its checkout without being told
a path; a profile that sets both `read-workspace = false` and `write-workspace = false` starts in a
private launch directory instead. It has no writable files or directories beyond those claims, and
the system `/dev/null` device can be opened read-write (D191). Workflows that update local refs or files are not supported.
The entry point and helpers can be user-installed native ELF binaries (D188). Whitelist only files
and directories whose contents
and updates you trust; paths overlapping agent write access and scripts are refused. Write
real paths for all profile paths (for example `/usr/bin/cat` instead of `/bin/cat` where `/bin` is a
symlink). Named launches require Linux with Landlock ABI 6; the initial-executable-only mode
also requires seccomp user notification.

## standard-tools

Each shipped WASI component runs only if `[standard-tools]` selects it, and the scopes listed there
delegate the agent's current reach rather than granting anything of their own (D147). Removing an
entry disables that tool; there is no native fallback. `saddle tools --config` prints the default
table, `saddle tools` prints each component's digest, and `saddle check` prints the selection in
force.

```toml
[standard-tools]
read  = ["fs-read", "tool-read"]
bash  = ["exec", "fs-read", "fs-write", "net"]
tasks = ["exec"]          # a background task bash started: report, stop, wait (D182)
config_help = []          # the configuration reference: nothing to delegate
```

`config_help` holds no reach at all — it answers from the text compiled into it — so its entry lists
no scopes and it answers a session whose grants allow nothing. It answers nothing if the entry is
missing, with a denial naming the missing entry.

A config written before this table existed does not run the shipped tools until the entries are
added (`saddle tools --config` prints what to add).

## plugins

Explicit third-party plugins. `[plugins.<id>]` is installation and enablement — `enabled`, the
manifest's path and its digest — and `[plugin.<id>]` in a policy file is what that plugin may do,
with the same scopes as `[agent]` (D128). Installing writes the artifact but enables nothing and
grants nothing; those stay separate decisions. `saddle plugin list` says what is installed, enabled,
granted and loadable, and `docs/plugins.md` in the saddle source tree documents the contract.

```toml
[plugins.reader]
enabled = true
manifest = "/home/me/plugins/reader/plugin.toml"
sha256 = "…"
```

## ui

`[ui]` is how the terminal interface starts; `/settings` writes these, and ctrl+t and ctrl+o change
two of them for one session.

| key | default | what it does |
| --- | --- | --- |
| `theme` | `default` | a built-in or `[themes.<name>]` theme; `"light/dark"` picks by the terminal (D71, D74) |
| `show-reasoning` | `true` | whether reasoning is shown as it streams (D151) |
| `expand-tools` | `false` | whether tool results start expanded (D124) |
| `status` | the built-in template | the status line, from `{provider}`, `{model}`, `{input}`, `{output}`, `{cache}`, `{cachehit}`, `{ctx}`, `{window}`, `{percent}`, `{cost}`, `{quota}`, `{tasks}`, `{workers}`, `{workspace}` (D36, D187, D195) |
| `mouse` | `true` | `false` hands the mouse back to the terminal, losing the wheel and drag-to-copy (D35) |
| `notify` | `bell` | how a turn that ends while the window is not focused is announced: `bell`, `osc9`, `osc777`, `off` (D164) |
| `images` | worked out | how a picture is drawn: `kitty`, `iterm2`, `none`, or unset to detect (D59) |
| `math-scale` | `1.0` | how large a formula is drawn, held to half to four times its natural size (D165) |

`{tasks}` shows this session's running background tasks (for example, `2 tasks`). It is part of
the default template and disappears when no tasks are running. Add `{tasks}` to a custom template
to show it there too.

`{workers}` shows the live workers of this session and how many are mid-turn, for example
`2 agents: 1 active, 1 idle` or `1 agent: 1 idle`. It is part of the default template and
disappears when the session has no live team. A worker that has finished keeps its conversation but
is not counted, and neither is a stopped one.

## keys

`[keys]` holds the moves that have no default key because there is no key to print until someone
picks one: the ends of the branches in `/tree` (D169). Almost nothing else is configurable — a mode
says its keys in its footer. A key is `ctrl`, `alt` or `shift` plus a named key or one character
(`ctrl+n`, `alt+j`, `shift+tab`, `f5`, `page-up`, `enter`, `space`), and the modifiers it names are
required rather than exclusive.

```toml
[keys]
tree-next-end = "ctrl+n"
tree-previous-end = "ctrl+p"
```

## compaction

`[compaction]` decides when the older part of a conversation is replaced by a model-written summary
(D33). Each of the three ways spends a request.

| key | default | what it does |
| --- | --- | --- |
| `auto` | `true` | compact when the conversation would overflow, and once after a context-overflow error |
| `reserve-tokens` | `16384` | room kept free below the context window |
| `keep-recent-tokens` | `20000` | recent conversation kept word for word beside the summary |
| `idle` | `true` | summarize a session left alone, so coming back does not spend the window on a stale prompt (D77) |
| `idle-fraction` | `0.5` | the share of the window at which that happens |
| `idle-seconds` | `120` | how long counts as left alone |
| `background` | `true` | summarize while the turn carries on instead of stopping it (D78) |
| `background-fraction` | `0.75` | the share of the window at which that starts |

## context

`[context]` is the instruction files (D32): the global `AGENTS.md`, then from the workspace root
down to the starting directory each directory's `AGENTS.md` (or `CLAUDE.md`), then its
`AGENTS.local.md`. `global` names another file and an empty string turns the global one off;
`claude-fallback` (default `true`) reads `CLAUDE.md` where there is no `AGENTS.md`.

## skills

`[skills]` is discovery (D64): `enabled` (default `true`) and `max` (default `32`, how many are
advertised in the prompt). The locations are fixed: `~/.agents/skills` and `~/.config/saddle/skills`
for the user, `.agents/skills` at the workspace root for the project. `/skill:<name> <request>`
loads one.

## themes

`[themes.<name>]` defines a theme, and `[ui] theme` selects it (D71). `extends` starts from a
built-in (`default`, `dark`, `light`, `mono`, `catppuccin-latte`, `catppuccin-frappe`,
`catppuccin-macchiato`, `catppuccin-mocha`) or another theme here, so one change restyles one thing.
`[themes.<name>.vars]` holds values several roles share. A colour is an ANSI name, `#rgb`, `#rrggbb`,
`oklch(62% 0.1 200)`, `okhsl(250 60% 55%)`, a palette index, a `vars` name, or `""` for the
terminal's default (D72, D73). The roles are pi's: `accent`, `border`, `text`, `muted`, `dim`,
`success`, `error`, `warning`, `thinkingText`, `selectedBg`, `userMessageText`, `toolPendingBg`,
`toolSuccessBg`, `toolErrorBg`, `toolTitle`, `toolOutput`, `bashMode`, the `md*` Markdown roles, the
`toolDiff*` roles and the `syntax*` ones. `/theme` previews every theme and `--use-theme` picks one
for a run.

## providers

`[providers.<name>]` declares one endpoint, or overrides a field of a built-in one (`synthetic`,
`openai`, `openai-chatgpt`, `anthropic`). A compatible server is a table saying which wire format it
imitates (D84).

| key | what it does |
| --- | --- |
| `api` | `openai` (chat completions), `anthropic` (Messages), or `openai-responses` where a preset uses it |
| `base-url` | the endpoint, without the `/v1` suffix for Anthropic |
| `model` | the model to ask for |
| `api-key-env` | the environment variable holding the key; empty means the server takes none |
| `max-tokens` | output limit per request; `/settings` changes it for a session |
| `effort` | reasoning effort: `low`, `medium`, `high`; `/thinking` changes it for a session (D57) |
| `context-window` | the model's window for compaction, beating what the endpoint states (D34) |
| `usage-url` | where the provider states what is left, for the status line's `{quota}` (D36) |
| `search-url` | the provider's own web search, which `websearch` uses before a second key (D82) |
| `headers` | extra request headers a gateway or account needs |
| `key-header` | the header the key goes in, when it is not the one its `api` implies (Azure) |
| `cache-retention` | `short` (the default, send nothing) or `long` (ask for a day) (D100) |
| `cache-affinity` | send the cache key and session headers whatever the host (D100) |
| `auth` | `api-key` or `oauth`; an OAuth provider's token comes from `/login`, never this file (D85) |
| `oauth` | the endpoints a sign-in talks to, as a preset or `[providers.<name>.oauth]` |
| `pricing` | dollars per million tokens when the server states none: `input`, `output`, `cache-read`, `cache-write`, and `unit = "token"` if the table is priced per token (D36) |

Keys are never written to `config.toml`: `/login` stores them in `auth.json`, or they come from the
environment variable named here. A kilobyte of `[mcp]`-style secret in a header belongs in
`headers`.

## mcp

`[mcp.<name>]` declares one server, and the `mcp` grant decides whether any of its tools may be
called (D104, D103). Exactly one of `command` (stdio) and `url` (streamable HTTP) describes it; a
table naming both or neither is refused when the session opens.

| key | what it does |
| --- | --- |
| `description` | the prompt's one line about this server |
| `command`, `args`, `env`, `cwd` | the child to run, for a stdio server; `cwd` is a directory under a root |
| `url` | an `https://` endpoint, or plain `http://` to this machine only |
| `timeout` | seconds one call may take; default 60 |
| `exposure` | `deferred` (declared by `tool_search`, the default), `direct` (declared up front) or `hidden` (never reachable) (D117) |
| `tool-exposure` | per-tool overrides, by exact tool name (D122) |

`codemode` is refused by name: saddle calls tools directly rather than writing code that calls them
(D119). `saddle mcp list` shows what is declared and granted, `saddle mcp auth <server>`/`logout`
manage a discovered OAuth sign-in, and `/mcp` reports and restarts one for a session.

## tool-limits

`[tool-limits]` is resource and presentation budgets, never permission (D145). They do not widen or
narrow a grant, and a permission check stands on its own: a budget refuses work that is too large,
not work that is not allowed. `/reload` applies a change without restarting.

The budgets in force for the session are appended to this section, kebab-case as `config.toml`
spells them, and the same list is in `docs/plugins.md` with what each one bounds. The ones a person
most often wants:

| key | bounds |
| --- | --- |
| `read-page-bytes`, `read-page-lines` | one `read` page |
| `text-bytes`, `list-entries`, `grep-matches` | how much one tool result carries |
| `exec-seconds` | how long one command may run |
| `exec-capture-bytes`, `exec-present-bytes` | how much of a command's output is captured, and how much is shown |
| `mutation-bytes`, `diff-bytes` | the size of one write batch, and of the diff shown |
| `image-bytes`, `image-count` | one picture, and how many one tool call may return |
| `request-image-bytes` | the pictures one request carries; older ones become a note (D162) |
| `network-seconds`, `http-redirects` | one HTTP request |
| `query-rows`, `query-result-bytes` | one `session_query` answer |
| `team-workers`, `team-steps` | live workers per session, and, when set, the model requests one worker turn may take; `0` (the default) means no limit, as the lead's own turn has none |

## commands

The settings a session can change for itself, and where they are written:

- `/settings` — provider, model, reasoning effort, limits and display; writes `config.toml`.
- `/permissions` — the grants this workspace has; writes its file under `policies/` (D45). `a`
  adds one by typing it, and the prompt shows the entry and what it reaches while it is typed
  — `net https://api.example.com/` is a grant for that host on https, port 443 (D46, D173).
- `/model` — the model for this session only; ctrl+s in the list saves the default (D91).
- `/theme` — a theme for this session, saved when chosen by name (D74).
- `/thinking` — reasoning effort for this session (D57).
- `/login`, `/logout` — a key or an OAuth sign-in, stored in `auth.json` (D85).
- `/reload` — re-read the settings, the theme, the instruction files, the skills and the budgets
  (D70).
- `/compact` — summarize now, at the next turn, or the conversation in hand (D33).
- `/mcp` — the servers, their restart, and their state for this session (D118).
- `/tasks` — background commands, whether finishing one resumes the agent, and how to cancel one
  (D153, D174).
- `/profile` — the set of grants in force; `/profile <name>` switches this session to one of
  config.toml's `[profiles.<name>]` sets, and `none` goes back to `[agent]` (D175).
- `/help` — the keys and every command, from the interface itself.

Outside the interface, `saddle check`, `saddle tools`, `saddle tools --config`, `saddle plugin
list`, `saddle mcp list`, `saddle session list` and `saddle audit verify <log>` answer the same
questions without a session, and `--grant`, `--revoke`, `--use-theme`, `--provider`, `--model`,
`--max-steps` and `--session` change one run.

## problems

The failures that look like bugs and are settings:

- **`denied:` on a tool path** — the path is outside every root, or the grant for its root does not
  allow the operation. `saddle check` prints the roots and the grants; a tool path declared under
  `[sandbox]` shows up as one (`tool paths`), a root as a name.
- **`denied:` on a host** — `net` is a list of host names, and `*` does not cover loopback or
  private addresses; those need the address named (D40).
- **`no profile named …`** — `--profile` or `/profile` named a set config.toml does not define; the
  error lists the ones it does, and `none` goes back to `[agent]` (D175).
- **A tool is missing** — `[standard-tools]` has no entry for it, or its configuration scopes are
  absent: `saddle tools --config` prints the table to add.
- **A plugin's tool is missing** — installed is not enabled, and enabled is not granted:
  `saddle plugin list` says which of the three is missing (D128).
- **`Permission denied` at startup** — the process cannot write its config or state directory, which
  happens under `bash` when `XDG_CONFIG_HOME`/`XDG_STATE_HOME` point outside what a command may
  write.
- **A workspace is refused** — it is, or contains, the home or config directory, so it puts every
  credential in reach. `saddle session allow <id> --workspace <path>` approves that id, `saddle
  session pending` lists what is waiting, and `allow-sensitive = true` skips the question (D60).
- **A resumed conversation is on the wrong model** — a choice belongs to its conversation; `/model`
  with no arguments lists every provider's models and marks the one in force (D88, D91).
- **A key is ignored** — an environment variable named by `api-key-env` beats a stored key, and a
  child process never inherits one, even when `exec` names it.
- **The status line's `{quota}` is empty** — the provider states nothing, or `usage-url` is unset
  (D36).
