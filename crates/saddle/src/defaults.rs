use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;
use crate::standard::DEFAULT_PROFILES;

pub const DEFAULT_AGENT: &str = r#"
fs-read  = [{ root = "workspace", exclude = [".env*"], why = "reads the project" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the project" }]
exec     = { why = "runs builds, tests and git" }
transcript-read = { why = "reads this workspace's conversations" }
"#;

const WRITABLE_CACHES: &[&str] = &[".cargo/registry", ".cargo/git"];

pub fn config_text(home: Option<&Path>) -> String {
    let write = WRITABLE_CACHES
        .iter()
        .copied()
        .filter(|p| home.is_some_and(|h| h.join(p).is_dir()))
        .map(|p| format!("\"~/{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r##"# saddle's user configuration. saddle created it on first run; edit it freely. ctrl+s in the model
# list, and the provider settings in /settings, update it and keep these comments.

# The provider, and its model, that a new session starts with. saddle asks on first run until this
# is set. /login and /model choose for the session they are run in; ctrl+s in the model list (or
# editing this file) is what changes the default.
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
# The transcript grant covers every saved conversation of this workspace. Add
# `sessions = "self"` to keep session_query to the conversation in hand.
transcript-read = {{ why = "reads this workspace's conversations" }}
# What the model itself may spend on the agent's own behalf, including a worker it starts.
# A budget, not a capability: it never widens a filesystem or network grant. Add
# `tokens-per-session` and/or `cost-per-session` here to cap it; without them the grant is the
# capability alone and spending is uncapped, as the conversation's own turn is (D37).
model-invoke = {{ why = "lets the agent start and brief its own workers" }}

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

# Command reads include only ~/.cargo, ~/.rustup and ~/.config by default (D160), withholding
# known credential/browser stores as well as Saddle config/state. Saddle does NOT walk your whole
# home: Pictures, Documents, other projects, shell history,
# and unrelated dot-directories are not implicitly readable. `include-defaults = false` disables
# these includes. Add other command paths explicitly to [sandbox] read/write above.
# Entries can name tools to allow direct file-tool access as well; bare paths are command-only.
# Custom [[sandbox.include]] rules are explicit, opt-in enumeration, not a default home walk.
# Rules over the same base merge exclusions; protected config/state paths are always withheld.

# Colours. `theme` names one: built in are default, dark, light, mono, system and the four
# catppuccin flavours (catppuccin-latte, catppuccin-frappe, catppuccin-macchiato,
# catppuccin-mocha; latte wants a light terminal, the other three a dark one). Define your own under
# [themes.<name>], and it starts from the roles it does not list, so three lines restyle one thing:
#
#   [ui]
#   theme = "mine"              # or "light/dark": light theme first, dark theme second
#
#   [themes.mine]
#   extends = "default"         # or "light", "mono", "catppuccin-mocha", or another theme here
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
# Every role pi defines is understood, and the syntax roles paint fenced code blocks. A role for
# something this interface has no element for (HTML export, hook-injected messages) is carried and
# draws nothing.
#
# /theme lists them all and previews each one; --use-theme changes it for one run; /reload re-reads
# this file.
#
# Compaction (D33, D77, D78). When the conversation nears the window the older part is replaced by a
# summary; these decide when, and whether that happens while you wait.
#
# [compaction]
# auto = true                 # compact at all, including once after a "context too long" error
# reserve-tokens = 16384      # room kept free below the window
# keep-recent-tokens = 20000  # recent conversation kept word for word beside the summary
# idle = true                 # summarize a session you left alone, so coming back to it does not
#                             # spend the whole window on a prompt typed before a break
# idle-fraction = 0.5         # ... once it is this share of the window full
# idle-seconds = 120          # ... and you have been away this long
# background = true            # start the summary at three quarters and keep generating, rather
#                             # than stopping the turn to summarize
# background-fraction = 0.75  # ... at this share of the window
#
# A summary costs one request. Idle compaction spends it while you are away; background compaction
# spends it beside the turn. Both are visible in the transcript when they happen.
#
# How the TUI starts. /settings changes these; ctrl+t and ctrl+o change them for one session.
# [ui]
# show-reasoning = true
# expand-tools = false
# mouse = true                          # false hands the mouse back to the terminal, which
#                                       # selects natively but loses the wheel and drag-to-copy
# notify = "bell"                       # a turn that ends while this window is not focused:
#                                       # bell, osc9 or osc777 (desktop notifications), or off
# images = "kitty"                      # how a picture is drawn: kitty, iterm2, none, or unset
#                                       # to work it out from the terminal
# math-scale = 1.0                      # how large a formula is drawn: 1.0 is the size its fonts
#                                       # and the terminal's cell give it, 1.2 a fifth larger
# status ="↑{{input}} ↓{{output}} {{cache}} · ctx {{ctx}}/{{window}} · {{cost}} · {{quota}} · {{provider}} {{model}}"
#                                       # fields: provider, model, input, output, cache, ctx,
#                                       # window, percent, cost, quota, workspace

# Keys. Almost nothing here is configurable: a mode says its keys in its footer, and up/down/enter/
# esc are what a list is for. The exception is a move saddle would otherwise not offer at all, where
# there is no key to print until someone picks one — the ends of the branches in /tree (D169).
# With no [keys] table there is no such key. A modifier a key names is one it requires; write
# ctrl, alt or shift, then a key name (n, tab, space, enter, up, page-down, f5, ...).
#
# [keys]
# tree-next-end = "ctrl+n"            # move to the next end of a branch in /tree
# tree-previous-end = "ctrl+p"        # ... and the previous one

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

# Explicit delegation for the shipped WASI implementations. Removing an entry disables that tool.
# Scopes select only the agent's current reach, never new paths/hosts/permissions.
{standard}
"##,
        write = write,
        standard = DEFAULT_PROFILES,
    )
}

pub fn ensure(paths: &Paths) -> Result<Vec<PathBuf>> {
    let home = env::var_os("HOME")
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
    use crate::config;
    use crate::config::SandboxEntry;
    use crate::config::UserConfig;
    use saddle_cap::policy::Policy;
    use saddle_cap::sandbox;
    use saddle_cap::sandbox::Include;
    use saddle_cap::{RootName, Roots};
    use std::fs;
    use std::path::Path;

    #[test]
    fn config_lists_only_toolchains_that_exist() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".cargo/registry")).unwrap();
        fs::create_dir_all(home.path().join(".cargo/git")).unwrap();
        fs::create_dir_all(home.path().join(".rustup")).unwrap();
        let text = config_text(Some(home.path()));
        let cfg: UserConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.provider, None, "first run asks instead of assuming");
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
    fn default_toolchain_includes_do_not_grant_personal_files_in_a_real_tree() {
        let home = tempfile::tempdir().unwrap();
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
            ".gitconfig",
            "src/main.rs",
            "notes.txt",
            "Pictures/comfyui/result.png",
            "Documents/private.txt",
            "other-checkout/source.rs",
            ".unknown-private/token",
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
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "secret\n").unwrap();
        }
        let granted = [
            ".cargo/registry/index",
            ".cargo/bin/cargo",
            ".cargo/config.toml",
            ".rustup/toolchains/stable",
            ".config/git/config",
            ".config/jj/config.toml",
        ];
        for rel in granted {
            let path = home.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "fine\n").unwrap();
        }
        let out: Vec<_> = config::default_includes()
            .into_iter()
            .flat_map(|rule| {
                let include = Include {
                    base: home.path().join(rule.base.strip_prefix("~/").unwrap()),
                    depth: rule.depth,
                    except: rule
                        .except
                        .iter()
                        .map(|p| regex::Regex::new(p).unwrap())
                        .collect(),
                };
                sandbox::enumerate(&include, &[])
            })
            .collect();
        let got: Vec<&Path> = out.iter().map(|p| p.as_path()).collect();
        for rel in withheld {
            let path = home.path().join(rel);
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
    fn explicit_home_compatibility_exclusions_match_the_stores_they_name() {
        let rule = config::explicit_home_defaults();
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
            ".cargo/credentials.toml",
            ".cargo/credentials",
        ] {
            assert!(
                withheld(rel),
                "{rel} is not withheld by any default pattern"
            );
        }
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
    fn transcript_search_is_default_but_explicit_grants_are_not_widened() {
        for text in [String::new(), config_text(None)] {
            let cfg: UserConfig = toml::from_str(&text).unwrap();
            assert!(
                cfg.global_policy(None).unwrap()["agent"]
                    .get("transcript-read")
                    .is_some()
            );
        }
        for text in ["[agent]\n", "[agent]\nexec = {}\n"] {
            let cfg: UserConfig = toml::from_str(text).unwrap();
            assert!(
                cfg.global_policy(None).unwrap()["agent"]
                    .get("transcript-read")
                    .is_none()
            );
        }
    }

    #[test]
    fn the_generated_config_is_valid_and_never_overwritten() {
        let ws = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        let cfg: UserConfig = toml::from_str(&config_text(None)).unwrap();
        let text = toml::to_string(&cfg.global_policy(None).unwrap()).unwrap();
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
