//! Skills (D64): a directory holding `SKILL.md`, whose frontmatter names it and says when it
//! applies. Discovery advertises the name, the description and the directory; the body is loaded
//! only when the model asks for it, so a directory of ten skills costs a few lines of prompt
//! rather than all of their text. The format is the Agent Skills one, which is what a skill
//! written for another harness already is.
//!
//! A skill is instructions, never authority. `allowed-tools` is read and ignored: what the agent
//! may do is what the policy allows, and that file is outside every root (D42). A project skill is
//! written by whoever wrote the repository, so it is prompt content like `AGENTS.md` (D32): it can
//! ask for what the grants already allow, and every call it causes is in the transcript and the
//! audit log.
//!
//! The user tier (`~/.agents/skills`, then `~/.config/saddle/skills`) is the user's own file, read
//! by the host like the global instruction file. The project tier (`.agents/skills` from the
//! workspace root down to the start directory) is read through the policy as the agent, so a skill
//! the agent may not read is refused and noted rather than advertised (D10). More specific wins,
//! as with instruction files: a project skill of the same name replaces a user one, and the note
//! says which it replaced.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use saddle_cap::Principal;
use saddle_policy::Kind;
use saddle_policy::audit::sha256_hex;
use serde_json::json;

use crate::config::SkillsConfig;
use crate::context::{cap, directories, home, label_of};
use crate::session::Session;
use crate::tools::fs::read_text;

/// The file that makes a directory a skill, and the only one discovery reads.
pub const FILE: &str = "SKILL.md";
/// The longest description advertised, which is the routing signal and so must stay short.
pub const MAX_DESCRIPTION: usize = 1024;
/// The most a directory tree is walked below a skills directory, so a deep tree costs a bound.
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// When to use it, written for routing. Required: without it the model cannot tell.
    pub description: String,
    pub dir: PathBuf,
    /// For the user and the model: `~/`-relative for the user tier, workspace-relative for a
    /// project skill.
    pub label: String,
    /// The whole file, frontmatter included, which is what the model reads when it loads it.
    pub text: String,
}

/// The skills found, and the notes for the person about any that were refused or shadowed.
pub fn discover(session: &mut Session, cfg: &SkillsConfig) -> Result<(Vec<Skill>, Vec<String>)> {
    let mut notes = Vec::new();
    if !cfg.enabled {
        return Ok((Vec::new(), notes));
    }
    // Every candidate goes through the same gate as the `read` tool, because `read` is how a skill
    // is loaded (D67). A skill the tools cannot open is not advertised: the prompt would otherwise
    // point at a denial. The user tier is `~/.agents/skills`, which is a default tool path (D66);
    // the project tier is read through the policy, so an excluded one is refused here.
    let mut skills: Vec<Skill> = Vec::new();
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(home) = home() {
        bases.push(home.join(".agents/skills"));
    }
    let workspace = session.setup.workspace.clone();
    for dir in directories(&workspace, &session.setup.start) {
        bases.push(workspace.join(&dir).join(".agents/skills"));
    }
    for base in bases {
        // Host stat: a missing directory is not a problem, and deciding nothing is the point.
        if base.symlink_metadata().is_err() {
            continue;
        }
        for file in walk(&base) {
            let label = label_of(&file);
            let raw = file.display().to_string();
            match read_text(session, &raw)? {
                Ok(text) => add(&mut skills, &mut notes, &file, label, text, cfg.max),
                Err(out) => notes.push(format!("{label} was not advertised: {}", out.content)),
            }
        }
    }
    // Advertised in the prompt, in the order found: general first, more specific later.
    skills.truncate(cfg.max.max(1));
    let record: Vec<_> = skills
        .iter()
        .map(|s| json!({ "name": s.name, "file": s.label, "bytes": s.text.len(), "sha256": sha256_hex(s.text.as_bytes()) }))
        .collect();
    session
        .audit
        .event(
            Kind::Skill,
            Some(&Principal::Agent),
            None,
            None,
            json!({ "discovered": record }),
            format!("{} skills available", skills.len()),
        )
        .context("writing the audit log")?;
    Ok((skills, notes))
}

/// Every `SKILL.md` below `base`, depth-capped.
///
/// Symlinks are followed, unlike the file tools' walk: a skill installed by linking it into
/// `~/.agents/skills` is the usual way to share one, and discovery is a host read of the user's own
/// directory rather than a path the model chose. The price is a cycle, so a directory already
/// visited is skipped; the depth cap bounds the rest. A project skill is still read through the
/// policy afterwards, so a link leading out of the workspace is refused there rather than here.
fn walk(base: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::from([(base.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        // The resolved directory is what a cycle repeats, so that is what is remembered.
        let real = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if !seen.insert(real) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            // `is_file` follows a link, so a linked skill is found even though the entry itself is
            // not a directory.
            if child.join(FILE).is_file() {
                found.push(child.join(FILE));
            } else if depth < MAX_DEPTH && std::fs::metadata(&child).is_ok_and(|m| m.is_dir()) {
                queue.push_back((child, depth + 1));
            }
        }
    }
    found
}

/// Parses one skill, takes it or explains why not, and replaces a weaker one of the same name.
fn add(
    skills: &mut Vec<Skill>,
    notes: &mut Vec<String>,
    file: &Path,
    label: String,
    text: String,
    max: usize,
) {
    // The resolved directory, so the model can find the bundled files the skill mentions even when
    // the skill was installed by linking it in.
    let dir = file
        .parent()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let fields = match frontmatter(&text) {
        Some(fields) => fields,
        None => {
            notes.push(format!(
                "{label} was not loaded: {FILE} must open with `---` frontmatter"
            ));
            return;
        }
    };
    // The name is the identifier the model calls, so it is held to what the specification allows
    // rather than to whatever the directory is called.
    let name = match fields.get("name").filter(|n| valid_name(n)) {
        Some(name) => name.clone(),
        None => {
            let dir_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if valid_name(&dir_name) {
                dir_name
            } else {
                notes.push(format!(
                    "{label} was not loaded: it needs a `name` of lowercase letters, digits and \
                     single hyphens, at most 64"
                ));
                return;
            }
        }
    };
    let Some(description) = fields.get("description").filter(|d| !d.trim().is_empty()) else {
        notes.push(format!(
            "{label} was not loaded: a skill needs a `description` saying when to use it"
        ));
        return;
    };
    if let Some(tools) = fields.get("allowed-tools").filter(|t| !t.trim().is_empty()) {
        notes.push(format!(
            "{name} asks for tools ({tools}) in `allowed-tools`; ignored: a skill is not authority, \
             the grants in the policy decide"
        ));
    }
    let skill = Skill {
        name: name.clone(),
        description: cap_description(description),
        dir,
        label,
        text: cap(text),
    };
    if let Some(at) = skills.iter().position(|s| s.name == name) {
        notes.push(format!(
            "skill {name} from {} replaces the one from {}",
            skill.label, skills[at].label
        ));
        skills.remove(at);
    } else if skills.len() >= max.max(1) {
        notes.push(format!(
            "skill {name} from {} is not advertised: {max} skills is the limit",
            skill.label
        ));
        return;
    }
    skills.push(skill);
}

fn cap_description(description: &str) -> String {
    let description = description.trim();
    if description.len() <= MAX_DESCRIPTION {
        return description.to_owned();
    }
    let mut cut = MAX_DESCRIPTION;
    while !description.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", description[..cut].trim_end())
}

/// A name the specification allows: lowercase letters, digits and single hyphens, at most 64.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// The frontmatter's top-level scalars. This is not a YAML parser: it is the subset a `SKILL.md`
/// uses — `key: value`, quoted or plain, and a `>` or `|` block for a description long enough to
/// need a paragraph. Anything else is left to the body, where it does no harm.
fn frontmatter(text: &str) -> Option<BTreeMap<String, String>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut fields = BTreeMap::new();
    let mut block: Option<(String, bool, Vec<String>)> = None;
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if let Some((key, folded, parts)) = &mut block {
            if indented || line.trim().is_empty() {
                parts.push(line.trim().to_owned());
                continue;
            }
            let joined = if *folded {
                parts.join(" ")
            } else {
                parts.join("\n")
            };
            fields.insert(key.clone(), joined.trim().to_owned());
            block = None;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key.is_empty() {
            continue;
        }
        match value {
            ">" | ">-" => block = Some((key, true, Vec::new())),
            "|" | "|-" => block = Some((key, false, Vec::new())),
            other => {
                fields.insert(key, unquote(other));
            }
        }
    }
    if let Some((key, folded, parts)) = block {
        let joined = if folded {
            parts.join(" ")
        } else {
            parts.join("\n")
        };
        fields.insert(key, joined.trim().to_owned());
    }
    (!fields.is_empty()).then_some(fields)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let quoted = (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2);
    if !quoted {
        return value.to_owned();
    }
    let inner = &value[1..value.len() - 1];
    if value.starts_with('"') {
        inner.replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        inner.replace("''", "'")
    }
}

/// The prompt block: names, descriptions and the file to read, never a body (D64, D67).
///
/// The file is printed as an absolute path, because `read` does not expand `~`: a relative path is
/// taken from the workspace, so a `~/` label would point at somewhere that does not exist.
pub fn listing(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\nSkills the user and this workspace provide. Each is instructions, not authority: a \
         skill cannot grant a tool or a path, so a skill that asks for one is asking the policy. \
         Read one with the `read` tool when its description applies, before starting the work, and \
         follow it. Its directory is where the files it mentions are, read the same way.\n\n\
         <skills>\n",
    );
    for skill in skills {
        let file = skill.dir.join(FILE);
        let _ = std::fmt::Write::write_fmt(
            &mut s,
            format_args!(
                "<skill name=\"{}\" file=\"{}\">\n{}\n</skill>\n",
                skill.name,
                file.display(),
                skill.description
            ),
        );
    }
    s.push_str("</skills>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(text: &str) -> BTreeMap<String, String> {
        frontmatter(text).expect("frontmatter")
    }

    #[test]
    fn frontmatter_reads_the_subset_a_skill_uses() {
        let f =
            fields("---\nname: pdf-tools\ndescription: \"Extract text. Use for PDFs.\"\n---\nbody");
        assert_eq!(f["name"], "pdf-tools");
        assert_eq!(f["description"], "Extract text. Use for PDFs.");
        // A folded block is joined with spaces; a literal one keeps its lines.
        let f = fields("---\nname: x\ndescription: >-\n  one\n  two\n---\n");
        assert_eq!(f["description"], "one two");
        let f = fields("---\nname: x\ndescription: |\n  one\n  two\n---\n");
        assert_eq!(f["description"], "one\ntwo");
        // No frontmatter at all is not a skill.
        assert_eq!(frontmatter("# just markdown\n"), None);
        assert_eq!(frontmatter("---\n---\n"), None);
    }

    #[test]
    fn a_name_is_the_identifier_the_specification_allows() {
        for good in ["pdf-tools", "a", "x1", "a-b-c"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-a", "a-", "a--b", "PDF", "a_b", "a.b", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_skill_without_a_description_is_not_advertised() {
        // The description is what routes the model to the skill, so a skill without one is noise
        // in the prompt and an unreliable call.
        let mut skills = Vec::new();
        let mut notes = Vec::new();
        add(
            &mut skills,
            &mut notes,
            Path::new("/ws/.agents/skills/x/SKILL.md"),
            "x/SKILL.md".into(),
            "---\nname: x\n---\nbody".into(),
            32,
        );
        assert!(skills.is_empty());
        assert!(notes[0].contains("needs a `description`"), "{notes:?}");
    }

    #[test]
    fn allowed_tools_is_ignored_and_said_so() {
        // D64: a skill cannot widen authority. Under the Agent Skills specification
        // `allowed-tools` is an experimental pre-approval list, which here is a policy change
        // written by whoever wrote the skill.
        let mut skills = Vec::new();
        let mut notes = Vec::new();
        add(
            &mut skills,
            &mut notes,
            Path::new("/ws/.agents/skills/x/SKILL.md"),
            "x/SKILL.md".into(),
            "---\nname: x\ndescription: d\nallowed-tools: bash(*), write\n---\nbody".into(),
            32,
        );
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "x");
        assert!(
            notes[0].contains("ignored: a skill is not authority"),
            "{notes:?}"
        );
        assert!(
            !skills[0].text.contains("ignored"),
            "the file is passed through unchanged"
        );
    }

    #[test]
    fn a_more_specific_skill_replaces_a_weaker_one_of_the_same_name() {
        let mut skills = Vec::new();
        let mut notes = Vec::new();
        let at = |label: &str, body: &str| {
            (
                PathBuf::from(label).join("SKILL.md"),
                label.to_owned(),
                format!("---\nname: review\ndescription: d\n---\n{body}"),
            )
        };
        let (file, label, text) = at("~/skills/review", "user");
        add(&mut skills, &mut notes, &file, label, text, 32);
        let (file, label, text) = at("project/review", "project");
        add(&mut skills, &mut notes, &file, label, text, 32);
        assert_eq!(skills.len(), 1, "one name is one skill");
        assert!(skills[0].text.contains("project"));
        assert!(
            notes[0].contains("replaces the one from ~/skills/review"),
            "{notes:?}"
        );
    }

    #[test]
    fn the_listing_carries_descriptions_and_directories_but_no_bodies() {
        let skills = [Skill {
            name: "pdf-tools".into(),
            description: "Extract text. Use for PDFs.".into(),
            dir: PathBuf::from("/home/me/.agents/skills/pdf-tools"),
            label: "~/agents/pdf-tools".into(),
            text: "---\nname: pdf-tools\n---\nSECRET BODY".into(),
        }];
        let block = listing(&skills);
        assert!(block.contains("name=\"pdf-tools\""));
        // The file, as an absolute path: `read` does not expand `~`, so this is what the model is
        // told to open (D67).
        assert!(
            block.contains("file=\"/home/me/.agents/skills/pdf-tools/SKILL.md\""),
            "{block}"
        );
        assert!(block.contains("`read` tool"), "{block}");
        assert!(block.contains("Extract text."));
        assert!(
            !block.contains("SECRET BODY"),
            "the body is loaded, not advertised"
        );
        assert_eq!(listing(&[]), "");
    }

    #[test]
    fn a_skill_installed_by_linking_it_in_is_found() {
        // A skill shared by symlinking it into the skills directory is the usual arrangement, and
        // this walk used to skip it: the entry is not a directory, so the filter dropped it before
        // the SKILL.md check and the skill vanished with no note at all.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("elsewhere/prose-lint");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join(FILE),
            "---\nname: prose-lint\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::os::unix::fs::symlink(&real, skills_dir.join("prose-lint")).unwrap();

        let found = walk(&skills_dir);
        assert_eq!(found, [skills_dir.join("prose-lint").join(FILE)]);

        // A link that loops must not walk for ever.
        std::os::unix::fs::symlink(&skills_dir, skills_dir.join("loop")).unwrap();
        assert_eq!(walk(&skills_dir).len(), 1, "the cycle is visited once");
    }

    #[test]
    fn a_linked_skill_reports_its_real_directory() {
        // The directory is what the model resolves a skill's own references against, so it is the
        // target of the link rather than the link.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("elsewhere/x");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join(FILE), "---\nname: x\ndescription: d\n---\nbody\n").unwrap();
        let mut skills = Vec::new();
        let mut notes = Vec::new();
        let link = dir.path().join("skills/x");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        add(
            &mut skills,
            &mut notes,
            &link.join(FILE),
            "~/skills/x/SKILL.md".into(),
            std::fs::read_to_string(link.join(FILE)).unwrap(),
            32,
        );
        assert_eq!(skills.len(), 1, "{notes:?}");
        assert_eq!(skills[0].dir, real.canonicalize().unwrap());
    }

    #[test]
    fn a_long_description_is_cut_rather_than_filling_the_prompt() {
        let long = "x".repeat(MAX_DESCRIPTION + 10);
        let cut = cap_description(&long);
        assert_eq!(
            cut.chars().count(),
            MAX_DESCRIPTION + 1,
            "cut plus the ellipsis"
        );
        assert!(cut.ends_with('…'));
    }
}
