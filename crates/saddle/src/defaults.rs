//! Files saddle writes on first run so it works without setup (D30): a user config with the
//! toolchains it finds (D30). The grants live in the same file, under `[agent]` (D63).

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;

/// The grants applied when a config has no `[agent]` table at all. A present table, even an empty
/// one, is taken as written: that is how a person says "no grants" rather than "not yet" (D63).
pub const DEFAULT_AGENT: &str = r#"
fs-read  = [{ root = "workspace", exclude = [".env*"], why = "reads the project" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the project" }]
exec     = { why = "runs builds, tests and git" }
"#;

/// A directory under `$HOME` that confined commands need: its root name, and whether they may
/// write it.
/// Cache directories a command may write, as paths. Not roots: a root is nameable by every policy
/// in the tree, so `fs-write` on a `cargo` root would let a workspace policy plant a program in
/// `~/.cargo/bin` — which runs outside the sandbox the next time it is called. These are caches
/// only, and `~/.cargo/bin` is deliberately absent (D62).
const WRITABLE_CACHES: &[&str] = &[".cargo/registry", ".cargo/git"];

/// The default config.toml for a home directory.
pub fn config_text(home: Option<&Path>) -> String {
    // Only the caches that exist, and only those, are writable. Nothing is a root: what a command
    // may read is the include rule below, and what it may write is these two caches (D62).
    let write = WRITABLE_CACHES
        .iter()
        .copied()
        .filter(|p| home.is_some_and(|h| h.join(p).is_dir()))
        .map(|p| format!("\"~/{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r##"# saddle's user configuration. saddle created it on first run; edit it freely. /login and
# /model in the TUI update the provider settings and keep these comments.

# The provider used when -p is not given. saddle asks on first run and /login changes it.
# Built in: synthetic, openai.
# provider = "synthetic"

# A workspace that is, or contains, your home or config directory puts every credential on this
# machine in reach, so saddle asks you to allow that session id before it opens (D60). Set this on a
# machine where that is not true -- a disposable container, say -- to skip the question.
# allow-sensitive = false

# Model requests per prompt before the loop stops and waits for you. /settings changes it.
# max-steps = 50          # model requests one prompt may take; unset means no limit

# Grants: what the agent may do in every workspace. This is the base. A workspace's own grants
# live in their own file, policies/<key>.toml next to this one, and what that file lists replaces
# the value here for that key, narrower or wider. `saddle check` prints both.
#
# `saddle --grant fs-read:scratch` and `saddle --revoke net` change the grants for one run and
# write nothing.
[agent]
fs-read  = [{{ root = "workspace", exclude = [".env*"], why = "reads the project" }}]
fs-write = [{{ root = "workspace", exclude = [".env*"], why = "edits the project" }}]
exec     = {{ why = "runs builds, tests and git" }}

# Directories outside the workspace, by name, so a policy can grant them to the tools. A root is
# what read, ls, find, grep, write and edit decide over: a tool path has to fall inside one, and the
# grant naming that root decides. Nothing is here by default. Never make a toolchain directory or a
# shared directory (~/.cargo, ~/.agents) a root: a root is nameable by any policy in a workspace, so
# a repository's grants file could then grant write to it and plant a program that runs later,
# outside the sandbox. A path in [sandbox] is not a substitute for a root -- it reaches commands,
# not tools (D62, D65).
# [roots]
# notes = "~/notes"

# What a sandboxed command may reach beyond the built-in list. This is not a grant and does not
# widen read, ls or grep: it is one allow-list per child process, with no per-call decision. Make
# only caches writable: a command that can write ~/.cargo/bin could plant a program you later run
# outside the sandbox.
#
# An entry may also name the tools that may reach that directory directly, which is how a directory
# outside the workspace becomes readable by read, ls and grep without becoming a root and without
# any grant covering it:
#
#   read = ["~/notes", {{ path = "~/elsewhere", tools = ["read", "ls", "find", "grep"] }}]
#
# A bare path is indirect reach only, so "~/notes" above is still unreachable by the tools. The
# tools are named individually: listing `write` or `edit` allows writing there, and that is enforced
# by this list alone, because the tools run in saddle rather than in the sandbox. A typo in a name is
# an error, not a silent no-op.
#
# ~/.agents is built in, read-only (read, ls, find, grep), because that is where skills are
# installed and a skill's own references have to be readable. An entry here for the same directory
# adds to it rather than replacing it. `tools-defaults = false` turns the built-in path off.
[sandbox]
write = [{write}]

# Sandboxed commands get what they need without saddle knowing about each tool: saddle walks your
# home directory at startup, withholds the usual credential stores below, and hands the kernel the
# list of what is left. So a toolchain works without being named, and a directory added later stays
# out of reach until the next start (D62).
#
# This rule is built in and is not written here, so there is one place to read it: the list below is
# what it withholds. Add your own [[sandbox.include]] rule to exclude more, or set
# `include-defaults = false` to use only the rules you write.
#
#   [[sandbox.include]]
#   base = "~"
#   depth = 3
#   except = ["^\\.ssh$", "^\\.aws$", ...]
#
# Withheld by default: .ssh .gnupg .pki .password-store .local/share/keyrings
# .local/share/kwalletd .config/op .vault-token .aws .azure .config/gcloud .kube .docker
# .config/containers .terraform.d .oci .s3cfg .boto .config/rclone .config/doctl .config/hcloud
# .config/gh .config/glab-cli .config/hub .config/tea .netrc .git-credentials
# .config/git/credentials .npmrc .yarnrc .yarnrc.yml .pypirc .config/pip .pip .gem/credentials
# .nuget .m2/settings.xml .gradle/gradle.properties .composer .config/composer .hex
# .cargo/credentials .cargo/credentials.toml .pgpass .my.cnf .mylogin.cnf .msmtprc .offlineimaprc
# .fetchmailrc .authinfo .bash_history .zsh_history .python_history .node_repl_history
# .psql_history .mysql_history .mozilla .thunderbird .config/google-chrome .config/chromium

# Colours. `theme` names one: built in are default, light and mono. Define your own under
# [themes.<name>], and it starts from the roles it does not list, so three lines restyle one thing:
#
#   [ui]
#   theme = "mine"              # or "light/dark": light theme first, dark theme second
#
#   [themes.mine]
#   extends = "default"         # or "light", "mono", or another theme here
#   accent = "#ff00ff"
#   muted = "240"
#
#   [themes.mine.vars]          # values several roles share; a role may name one
#   warn = "oklch(62% 0.1 200)"
#
# A colour is an ANSI name (cyan, dark-gray, …), #rgb or #rrggbb, oklch(62% 0.1 200),
# okhsl(250 60% 55%), a palette index 0-255, a `vars` name, or "" for the terminal's default.
#
# The roles are pi's names, so a palette from a pi theme can be copied across:
#   accent border borderAccent borderMuted success error warning muted dim text thinkingText
#   selectedBg selectionFg userMessageText toolPendingBg toolSuccessBg toolErrorBg toolTitle
#   toolOutput toolErrorText compactDoneBg mdHeading mdLink mdLinkUrl mdCode mdCodeBlock
#   mdCodeBlockBorder mdQuote mdQuoteBorder mdHr mdListBullet toolDiffAdded toolDiffRemoved
#   toolDiffContext bashMode
# A role pi has for something this interface does not draw (syntax highlighting, scrollbars, search
# matches, per-level thinking borders, custom messages, HTML export) is refused with that reason
# rather than silently ignored.
#
# /theme switches and saves; --use-theme changes it for one run; /reload re-reads this file.
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
"##,
        write = write,
    )
}

/// Creates whichever default files are missing and returns the ones it wrote. Existing files are
/// never touched.
/// The policy text a workspace's file is seeded with when it has none: the global policy, or this
/// built-in one when the global file is missing or unreadable (D42).
pub fn ensure(paths: &Paths) -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let mut created = Vec::new();
    let path = paths.user_config();
    if create_new(&path, &config_text(home.as_deref()))? {
        created.push(path);
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
    use crate::config::SandboxEntry;
    use crate::config::UserConfig;
    use saddle_cap::policy::Policy;
    use saddle_cap::{RootName, Roots};

    #[test]
    fn config_lists_only_toolchains_that_exist() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".cargo/registry")).unwrap();
        fs::create_dir_all(home.path().join(".cargo/git")).unwrap();
        fs::create_dir_all(home.path().join(".rustup")).unwrap();
        let text = config_text(Some(home.path()));
        let cfg: UserConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.provider, None, "first run asks instead of assuming");
        // D62: no toolchain directory is a root. A root is nameable by every policy, so `cargo`
        // as a root let a workspace policy grant write to it and plant a program in ~/.cargo/bin.
        assert!(cfg.roots.is_empty(), "{:?}", cfg.roots);
        assert!(cfg.sandbox.read.is_empty());
        assert_eq!(
            cfg.sandbox.write,
            [
                SandboxEntry::Path("~/.cargo/registry".into()),
                SandboxEntry::Path("~/.cargo/git".into())
            ],
            "caches by path, not by name"
        );
        // The built-in rule rather than one written into the file (D62).
        assert!(cfg.sandbox.include_defaults);
        assert!(
            cfg.sandbox.include.is_empty(),
            "nothing is written into the file"
        );
        assert!(
            !text.contains("[[sandbox.include]]\nbase"),
            "not an active rule: {text}"
        );

        let empty: UserConfig = toml::from_str(&config_text(None)).unwrap();
        assert!(empty.roots.is_empty() && empty.sandbox.write.is_empty());
    }

    #[test]
    fn the_default_rule_withholds_every_path_it_names_in_a_real_tree() {
        // The regex test below only proves the patterns match a string. This one builds the tree
        // and runs the walk, which is what actually decides: with `depth = 2`, `.local/share/
        // keyrings` is three levels down, was never enumerated, and its pattern matched nothing —
        // so the keyring was readable (D62).
        let home = tempfile::tempdir().unwrap();
        let rule = crate::config::default_include();
        let include = saddle_cap::sandbox::Include {
            base: home.path().to_path_buf(),
            depth: rule.depth,
            except: rule
                .except
                .iter()
                .map(|p| regex::Regex::new(p).unwrap())
                .collect(),
        };
        // Everything the default list names, created so the walk has to find it.
        let withheld = [
            ".ssh/id_rsa",
            ".gnupg/private-keys-v1.d",
            ".password-store/x.gpg",
            ".local/share/keyrings/login.keyring",
            ".local/share/kwalletd/k.kwl",
            ".config/op/1password.json",
            ".vault-token",
            ".aws/credentials",
            ".azure/accessTokens.json",
            ".config/gcloud/credentials.db",
            ".kube/config",
            ".docker/config.json",
            ".config/containers/auth.json",
            ".terraform.d/x",
            ".oci/config",
            ".s3cfg",
            ".boto",
            ".config/rclone/rclone.conf",
            ".config/doctl/config.yaml",
            ".config/hcloud/config.yaml",
            ".config/gh/hosts.yml",
            ".config/glab-cli/config.yml",
            ".config/hub",
            ".config/tea/config.yml",
            ".netrc",
            ".git-credentials",
            ".config/git/credentials",
            ".npmrc",
            ".yarnrc",
            ".yarnrc.yml",
            ".pypirc",
            ".config/pip/pip.conf",
            ".pip/pip.conf",
            ".gem/credentials",
            ".nuget/NuGet/NuGet.Config",
            ".m2/settings.xml",
            ".gradle/gradle.properties",
            ".composer/auth.json",
            ".config/composer/auth.json",
            ".hex/hex.config",
            ".cargo/credentials",
            ".cargo/credentials.toml",
            ".pgpass",
            ".my.cnf",
            ".mylogin.cnf",
            ".msmtprc",
            ".offlineimaprc",
            ".fetchmailrc",
            ".authinfo",
            ".bash_history",
            ".zsh_history",
            ".python_history",
            ".node_repl_history",
            ".psql_history",
            ".mysql_history",
            ".mozilla/profiles.ini",
            ".thunderbird/x",
            ".config/google-chrome/Default/Cookies",
            ".config/chromium/Default/Cookies",
        ];
        for rel in withheld {
            let path = home.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "secret\n").unwrap();
        }
        let granted = [
            ".cargo/registry/index",
            ".cargo/bin/cargo",
            ".cargo/config.toml",
            ".rustup/toolchains/stable",
            ".config/git/config",
            ".config/jj/config.toml",
            ".gitconfig",
            "src/main.rs",
            "notes.txt",
        ];
        for rel in granted {
            let path = home.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "fine\n").unwrap();
        }
        let out = saddle_cap::sandbox::enumerate(&include, &[]);
        let got: Vec<&std::path::Path> = out.iter().map(|p| p.as_path()).collect();
        for rel in withheld {
            let path = home.path().join(rel);
            // A rule on the file, or on a directory above it, would both reach it.
            let reachable = got.iter().any(|g| path == *g || path.starts_with(g));
            assert!(!reachable, "{rel} is reachable through {:?}", got);
        }
        for rel in granted {
            let path = home.path().join(rel);
            assert!(
                got.iter().any(|g| path == *g || path.starts_with(g)),
                "{rel} should stay readable; rules were {got:?}"
            );
        }
    }

    #[test]
    fn every_default_except_pattern_matches_the_store_it_names() {
        // A pattern that never matches is silent: the directory is simply granted. So each one is
        // pinned to the path it is meant to withhold (D62).
        let rule = crate::config::default_include();
        let patterns: Vec<regex::Regex> = rule
            .except
            .iter()
            .map(|p| regex::Regex::new(p).expect("a default pattern compiles"))
            .collect();
        let withheld = |rel: &str| patterns.iter().any(|re| re.is_match(rel));
        for rel in [
            ".ssh",
            ".gnupg",
            ".password-store",
            ".local/share/keyrings",
            ".config/op",
            ".vault-token",
            ".aws",
            ".azure",
            ".config/gcloud",
            ".kube",
            ".docker",
            ".config/containers",
            ".terraform.d",
            ".config/gh",
            ".config/glab-cli",
            ".config/hub",
            ".netrc",
            ".git-credentials",
            ".config/git/credentials",
            ".npmrc",
            ".pypirc",
            ".gem/credentials",
            ".m2/settings.xml",
            ".gradle/gradle.properties",
            // The one this list nearly missed: a crates.io token, in a directory granted for the
            // toolchain itself.
            ".cargo/credentials.toml",
            ".cargo/credentials",
        ] {
            assert!(
                withheld(rel),
                "{rel} is not withheld by any default pattern"
            );
        }
        // And the things that have to keep working are not withheld.
        for rel in [
            ".cargo",
            ".cargo/registry",
            ".cargo/bin",
            ".cargo/config.toml",
            ".rustup",
            ".config/git/config",
            ".config/jj",
            ".gitconfig",
            "src",
            "proj",
        ] {
            assert!(!withheld(rel), "{rel} must stay readable");
        }
    }

    #[test]
    fn the_generated_config_is_valid_and_never_overwritten() {
        // One file now (D63), so the check is that its [agent] table parses as a policy.
        let ws = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        let cfg: UserConfig = toml::from_str(&config_text(None)).unwrap();
        let text = toml::to_string(&cfg.global_policy()).unwrap();
        Policy::parse(&text, &roots).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
        };
        assert_eq!(ensure(&paths).unwrap().len(), 1, "one file is written");
        fs::write(paths.user_config(), "# mine\n").unwrap();
        assert!(ensure(&paths).unwrap().is_empty());
        assert_eq!(fs::read_to_string(paths.user_config()).unwrap(), "# mine\n");
    }
}
