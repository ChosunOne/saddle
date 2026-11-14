//! Instruction files (D32). The global AGENTS.md comes first, then each directory from the
//! workspace root down to the start directory: its AGENTS.md (or CLAUDE.md where there is no
//! AGENTS.md), then its AGENTS.local.md. More specific files come later, so they have the last
//! word. Nothing above the workspace is read: it is outside every root the policy grants.
//!
//! Project files are read through the policy as the agent, like the read tool, so a file the
//! agent may not read (or a symlink leading out of the workspace) never reaches the prompt.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use saddle_cap::Principal;
use saddle_policy::Kind;
use saddle_policy::audit::sha256_hex;
use serde_json::json;

use crate::config::ContextConfig;
use crate::paths::Paths;
use crate::session::Session;
use crate::tools::fs::read_text;

/// Bytes of one file that go into the prompt; the rest is cut with a note.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    /// Shown to the user and the model: workspace-relative, or `~/`-relative for the global file.
    pub label: String,
    pub text: String,
}

pub(crate) fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

pub(crate) fn label_of(path: &Path) -> String {
    match home().and_then(|h| path.strip_prefix(h).ok().map(Path::to_owned)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

pub(crate) fn cap(mut text: String) -> String {
    if text.len() > MAX_FILE_BYTES {
        let mut cut = MAX_FILE_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let total = text.len();
        text.truncate(cut);
        text.push_str(&format!("\n[cut at {cut} of {total} bytes]"));
    }
    text
}

/// The global file to use: the configured or default AGENTS.md, or `~/.claude/CLAUDE.md` when that
/// does not exist and the fallback is on.
pub fn global_file(cfg: &ContextConfig, paths: &Paths) -> Option<PathBuf> {
    let agents = match cfg.global.as_deref() {
        Some("") => return None,
        Some(p) => expand(p),
        None => paths.config.join("AGENTS.md"),
    };
    if agents.is_file() {
        return Some(agents);
    }
    let claude = home()?.join(".claude/CLAUDE.md");
    (cfg.claude_fallback.unwrap_or(true) && claude.is_file()).then_some(claude)
}

/// The workspace-relative directories from the root down to `start`.
pub(crate) fn directories(workspace: &Path, start: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::new()];
    if let Ok(rel) = start.strip_prefix(workspace) {
        let mut at = PathBuf::new();
        for part in rel.components() {
            at.push(part);
            dirs.push(at.clone());
        }
    }
    dirs
}

/// Loads the files, records them in the audit log, and returns them with notes for the user
/// about any that were refused.
pub fn load(
    session: &mut Session,
    cfg: &ContextConfig,
    paths: &Paths,
) -> Result<(Vec<ContextFile>, Vec<String>)> {
    let mut files = Vec::new();
    let mut notes = Vec::new();
    if let Some(path) = global_file(cfg, paths) {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        files.push(ContextFile {
            label: label_of(&path),
            text: cap(text),
        });
    }
    let workspace = session.setup.workspace.clone();
    let fallback = cfg.claude_fallback.unwrap_or(true);
    for dir in directories(&workspace, &session.setup.start) {
        let exists = |name: &str| workspace.join(&dir).join(name).symlink_metadata().is_ok();
        let main = if exists("AGENTS.md") {
            Some("AGENTS.md")
        } else if fallback && exists("CLAUDE.md") {
            Some("CLAUDE.md")
        } else {
            None
        };
        let local = exists("AGENTS.local.md").then_some("AGENTS.local.md");
        for name in main.into_iter().chain(local) {
            let rel = dir.join(name);
            let raw = workspace.join(&rel).display().to_string();
            let label = rel.display().to_string();
            match read_text(session, &raw)? {
                Ok(text) => files.push(ContextFile {
                    label,
                    text: cap(text),
                }),
                Err(out) => notes.push(format!("{label} was not loaded: {}", out.content)),
            }
        }
    }
    let record: Vec<_> = files
        .iter()
        .map(|f| json!({ "file": f.label, "bytes": f.text.len(), "sha256": sha256_hex(f.text.as_bytes()) }))
        .collect();
    session
        .audit
        .event(
            Kind::Context,
            Some(&Principal::Agent),
            None,
            None,
            json!({ "files": record }),
            format!(
                "{} instruction files added to the system prompt",
                files.len()
            ),
        )
        .context("writing the audit log")?;
    Ok((files, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn directories_run_from_the_root_down_to_the_start() {
        let dirs = directories(Path::new("/ws"), Path::new("/ws/crates/saddle"));
        assert_eq!(
            dirs,
            [
                PathBuf::new(),
                PathBuf::from("crates"),
                PathBuf::from("crates/saddle")
            ]
        );
        assert_eq!(
            directories(Path::new("/ws"), Path::new("/ws")),
            [PathBuf::new()]
        );
    }

    #[test]
    fn long_files_are_cut_with_a_note() {
        let text = cap("é".repeat(MAX_FILE_BYTES));
        assert!(text.ends_with(&format!("of {} bytes]", 2 * MAX_FILE_BYTES)));
    }

    #[test]
    fn the_global_file_and_its_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
        };
        fs::create_dir_all(&paths.config).unwrap();
        let off = ContextConfig {
            global: Some(String::new()),
            claude_fallback: None,
        };
        assert_eq!(global_file(&off, &paths), None);
        fs::write(paths.config.join("AGENTS.md"), "be terse").unwrap();
        let default = ContextConfig::default();
        assert_eq!(
            global_file(&default, &paths),
            Some(paths.config.join("AGENTS.md"))
        );
        let elsewhere = dir.path().join("mine.md");
        fs::write(&elsewhere, "x").unwrap();
        let custom = ContextConfig {
            global: Some(elsewhere.display().to_string()),
            claude_fallback: Some(false),
        };
        assert_eq!(global_file(&custom, &paths), Some(elsewhere));
    }
}
