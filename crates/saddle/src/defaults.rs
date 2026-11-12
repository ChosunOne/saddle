//! Files saddle writes on first run so it works without setup (D30): a user config with the
//! toolchains it finds, and a default policy for workspaces that have no `saddle.toml`.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;

pub const DEFAULT_POLICY: &str = r#"# The global policy. Every workspace gets its own copy of this file the first time a session
# opens there, under policies/ in this directory, and that copy is what applies (D42).
#
# Both live in your config directory, which no root contains, so neither the agent nor a
# sandboxed command can write them, so a change to either takes effect in the next session.
version = 1

[agent]
fs-read  = [{ root = "workspace", exclude = [".env*"], why = "reads the project" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the project" }]
exec     = { why = "runs builds, tests and git" }
"#;

/// A directory under `$HOME` that confined commands need: its root name, and whether they may
/// write it.
struct Toolchain {
    name: &'static str,
    path: &'static str,
    write: bool,
}

/// Writable entries are caches only. A command that could write `~/.cargo/bin` or
/// `~/.cargo/config.toml` could plant something that later runs outside the sandbox.
///
/// Read-only entries are configuration that is not a toolchain but is needed to work at all, and
/// they are read **and execute** for the child, with no exclusions applied to it (D19, D22): every
/// file beneath one must be safe to read. Never add a directory that holds a token (D60). Git's
/// XDG config is here because a confined `git commit` cannot run without it; the classic
/// `~/.gitconfig` cannot be a root, since a root is a directory (D60).
const TOOLCHAINS: &[Toolchain] = &[
    Toolchain {
        name: "cargo",
        path: ".cargo",
        write: false,
    },
    Toolchain {
        name: "cargo-registry",
        path: ".cargo/registry",
        write: true,
    },
    Toolchain {
        name: "cargo-git",
        path: ".cargo/git",
        write: true,
    },
    Toolchain {
        name: "rustup",
        path: ".rustup",
        write: false,
    },
    Toolchain {
        name: "git",
        path: ".config/git",
        write: false,
    },
];

/// The default config.toml for a home directory: roots and sandbox grants for the toolchains
/// present there.
pub fn config_text(home: Option<&Path>) -> String {
    let found: Vec<&Toolchain> = TOOLCHAINS
        .iter()
        .filter(|t| home.is_some_and(|h| h.join(t.path).is_dir()))
        .collect();
    let list = |write: bool| {
        found
            .iter()
            .filter(|t| t.write == write)
            .map(|t| format!("\"{}\"", t.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut roots = String::new();
    for t in &found {
        roots.push_str(&format!("{} = \"~/{}\"\n", t.name, t.path));
    }
    format!(
        r#"# saddle's user configuration. saddle created it on first run; edit it freely. /login and
# /model in the TUI update the provider settings and keep these comments.

# The provider used when -p is not given. saddle asks on first run and /login changes it.
# Built in: synthetic, openai.
# provider = "synthetic"

# Model requests per prompt before the loop stops and waits for you. /settings changes it.
# max-steps = 50          # model requests one prompt may take; unset means no limit

# A session may not be opened in your home directory, or above it, because .ssh, .aws and every
# other credential live beneath it and a root cannot be narrowed from the inside (D60). Set this
# only if you mean to hand this machine's home directory to the agent.
# allow-home = false

# Directories outside the workspace, by name. Policies and [sandbox] refer to them by name.
[roots]
{roots}
# Roots every sandboxed command gets on top of its policy's grants. Make only caches writable:
# a command that can write ~/.cargo/bin could plant a program you later run outside the sandbox.
[sandbox]
read = [{read}]
write = [{write}]

# How the TUI starts. /settings changes these; ctrl+t and ctrl+o change them for one session.
# [ui]
# show-reasoning = true
# expand-tools = false
# mouse = true                          # false hands the mouse back to the terminal, which
#                                       # selects natively but loses the wheel and drag-to-copy
# status = "↑{{input}} ↓{{output}} {{cache}} · ctx {{ctx}}/{{window}} · {{cost}} · {{quota}} · {{provider}} {{model}}"
#                                       # fields: provider, model, input, output, cache, ctx,
#                                       # window, percent, cost, quota, workspace

# Other providers, or overrides of a built-in one (a table named after it changes its fields).
# /login writes these tables too. Keys it stores go to ~/.local/state/saddle/auth.json, not here.
#
# [providers.local]
# api = "openai"                        # or "anthropic" for a Messages-API endpoint
# base-url = "http://127.0.0.1:8080/v1"
# model = "qwen3-coder"
# api-key-env = "LOCAL_KEY"             # optional
# effort = "high"                       # reasoning effort; /settings changes it
# max-tokens = 8192                     # output limit; /settings changes it
# context-window = 128000               # D34: beats what the endpoint states
# usage-url = "https://api.example.com/usage"  # D36: where the provider states what is left
# [providers.local.pricing]             # dollars per million tokens, when the server states none;
# input = 3.00                          # add `unit = "token"` if the table is priced per token
# output = 15.00
# cache-read = 0.30
"#,
        read = list(false),
        write = list(true),
    )
}

/// Creates whichever default files are missing and returns the ones it wrote. Existing files are
/// never touched.
/// The policy text a workspace's file is seeded with when it has none: the global policy, or this
/// built-in one when the global file is missing or unreadable (D42).
pub fn default_policy_text() -> String {
    DEFAULT_POLICY.to_owned()
}

pub fn ensure(paths: &Paths) -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let mut created = Vec::new();
    for (path, text) in [
        (paths.user_config(), config_text(home.as_deref())),
        (paths.default_policy(), DEFAULT_POLICY.to_owned()),
    ] {
        if create_new(&path, &text)? {
            created.push(path);
        }
    }
    Ok(created)
}

fn create_new(path: &Path, text: &str) -> Result<bool> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(text.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use saddle_cap::policy::Policy;
    use saddle_cap::{RootName, Roots};

    #[test]
    fn config_lists_only_toolchains_that_exist() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".cargo/registry")).unwrap();
        fs::create_dir_all(home.path().join(".rustup")).unwrap();
        let text = config_text(Some(home.path()));
        let cfg: UserConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.provider, None, "first run asks instead of assuming");
        assert_eq!(
            cfg.roots.keys().collect::<Vec<_>>(),
            ["cargo", "cargo-registry", "rustup"]
        );
        assert_eq!(cfg.sandbox.read, ["cargo", "rustup"]);
        assert_eq!(cfg.sandbox.write, ["cargo-registry"]);

        let empty: UserConfig = toml::from_str(&config_text(None)).unwrap();
        assert!(empty.roots.is_empty() && empty.sandbox.write.is_empty());
    }

    #[test]
    fn default_policy_parses_and_files_are_never_overwritten() {
        let ws = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        Policy::parse(DEFAULT_POLICY, &roots).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
        };
        assert_eq!(ensure(&paths).unwrap().len(), 2);
        fs::write(paths.default_policy(), "# mine\n").unwrap();
        assert!(ensure(&paths).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(paths.default_policy()).unwrap(),
            "# mine\n"
        );
    }
}
