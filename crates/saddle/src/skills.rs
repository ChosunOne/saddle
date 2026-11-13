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
use crate::context::{cap, home, label_of};
use crate::ignore::Rules;
use crate::session::Session;
use crate::tools::fs::read_text;

/// The file that makes a directory a skill, and the only one discovery reads.
pub const FILE: &str = "SKILL.md";
/// The longest description advertised, which is the routing signal and so must stay short.
pub const MAX_DESCRIPTION: usize = 1024;
/// How deep a skills directory or the project scan descends. pi has no bound, because a directory
/// holding `SKILL.md` ends the walk; this bounds the pathological case of a tree of directories
/// with none.
const MAX_DEPTH: usize = 32;
/// Directories visited in one scan before it gives up, so a huge tree cannot stall a session.
const MAX_VISITED: usize = 100_000;

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
    if let Some(home) = home() {
        let base = home.join(".agents/skills");
        if base.is_dir() {
            load_dir(
                session,
                &base,
                "",
                &mut Rules::default(),
                &mut skills,
                &mut notes,
                cfg.max,
            )?;
        }
    }
    // A project skill may sit in any directory of the workspace, so the scan finds every
    // `.agents/skills` in the tree rather than only the ancestors of the start directory (D68).
    // Shallowest first, so a deeper one of the same name wins, as a more specific instruction file
    // does.
    let workspace = session.setup.workspace.clone();
    for base in project_bases(&workspace) {
        load_dir(
            session,
            &base,
            "",
            &mut Rules::default(),
            &mut skills,
            &mut notes,
            cfg.max,
        )?;
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

/// Every `.agents/skills` directory in the workspace, shallowest first (D68).
///
/// Walking the whole tree is what makes a skill scoped to a subdirectory discoverable from the
/// root, and the ignore files are what make that affordable: a build directory that a project
/// already ignores is not walked. Dot directories are skipped, which also stops the scan entering
/// `.agents` itself.
fn project_bases(workspace: &Path) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    let mut rules = Rules::default();
    let mut stack = vec![(workspace.to_path_buf(), String::new(), 0usize)];
    let mut seen = std::collections::HashSet::new();
    let mut visited = 0;
    while let Some((dir, rel, depth)) = stack.pop() {
        visited += 1;
        if visited > MAX_VISITED {
            break;
        }
        // A symlinked directory can point back at an ancestor; the resolved path is what repeats.
        let real = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if !seen.insert(real) {
            continue;
        }
        load_ignores(&dir, &rel, &mut rules);
        let candidate = dir.join(SKILLS_DIR);
        if candidate.is_dir() {
            bases.push(candidate);
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if skip_name(&name) {
                continue;
            }
            let child = dir.join(&name);
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let Ok(meta) = std::fs::metadata(&child) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            if rules.ignored(&child_rel, true) {
                continue;
            }
            stack.push((child, child_rel, depth + 1));
        }
    }
    // Shallowest first, so a deeper skill of the same name replaces a shallower one.
    bases.sort_by_key(|p| (p.components().count(), p.clone()));
    bases
}

/// Where a project's skills live, relative to the directory that scopes them.
const SKILLS_DIR: &str = ".agents/skills";

/// A name discovery never looks at: dot entries (which covers `.git` and the ignore files
/// themselves) and `node_modules`, whose skills are a dependency's rather than the project's.
fn skip_name(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules"
}

/// Adds the rules of every ignore file in `dir`, which sits at `rel` below the scan root.
fn load_ignores(dir: &Path, rel: &str, rules: &mut Rules) {
    for name in crate::ignore::FILES {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            rules.add(&text, rel);
        }
    }
}

/// One skills directory, with pi's rules: a directory holding `SKILL.md` is a skill and is not
/// descended into; otherwise its subdirectories are searched, and at the top level a bare `.md`
/// file is a skill too (D68).
///
/// Symlinks are followed, unlike the file tools' walk: a skill installed by linking it in is the
/// usual way to share one, and discovery is a host read of the user's own directory rather than a
/// path the model chose. The depth cap bounds a tree of directories with no skill in it; a project
/// skill is still read through the policy, so a link leading out of the workspace is refused there.
fn load_dir(
    session: &mut Session,
    dir: &Path,
    rel: &str,
    rules: &mut Rules,
    skills: &mut Vec<Skill>,
    notes: &mut Vec<String>,
    max: usize,
) -> Result<()> {
    load_ignores(dir, rel, rules);
    let declared = dir.join(FILE);
    // `is_file` follows a link, so a linked skill is found even though the entry is not a
    // directory. A directory with SKILL.md is a skill root: nothing beneath it is searched, which
    // is what stops a skill's own `references/` being mistaken for more skills.
    if declared.is_file() {
        let child_rel = if rel.is_empty() {
            FILE.to_owned()
        } else {
            format!("{rel}/{FILE}")
        };
        if rel.is_empty() || !rules.ignored(&child_rel, false) {
            load_file(session, &declared, skills, notes, max)?;
        }
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if skip_name(&name) {
            continue;
        }
        let child = dir.join(&name);
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        // `metadata` follows a link, so a linked directory is searched and a linked file read.
        let Ok(meta) = std::fs::metadata(&child) else {
            continue;
        };
        if meta.is_dir() {
            if rel.matches('/').count() >= MAX_DEPTH {
                continue;
            }
            if !rules.ignored(&child_rel, true) {
                load_dir(session, &child, &child_rel, rules, skills, notes, max)?;
            }
        } else if rel.is_empty() && name.ends_with(".md") && !rules.ignored(&child_rel, false) {
            // A bare markdown file in the top level of a skills directory is a skill too, which is
            // the portable-enough form pi accepts beside the directory layout.
            load_file(session, &child, skills, notes, max)?;
        }
    }
    Ok(())
}

/// Reads one skill file through the gate and offers it to [`add`].
fn load_file(
    session: &mut Session,
    file: &Path,
    skills: &mut Vec<Skill>,
    notes: &mut Vec<String>,
    max: usize,
) -> Result<()> {
    let label = label_of(file);
    let raw = file.display().to_string();
    match read_text(session, &raw)? {
        Ok(text) => add(skills, notes, file, label, text, max),
        Err(out) => notes.push(format!("{label} was not advertised: {}", out.content)),
    }
    Ok(())
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
    fn a_project_skill_is_found_in_any_directory_of_the_workspace() {
        // D68: the project tier used to be the ancestors of the start directory only, so a skill
        // scoped to `crates/saddle` was invisible from the root. The scan finds them all.
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        for base in ["", "crates", "crates/saddle", "tmp"] {
            let skills = ws.join(base).join(SKILLS_DIR);
            std::fs::create_dir_all(&skills).unwrap();
            std::fs::write(skills.join(FILE), "---\nname: x\ndescription: d\n---\n").unwrap();
        }
        std::fs::create_dir_all(ws.join("no-skills-here")).unwrap();
        let found = project_bases(ws);
        assert_eq!(
            found,
            [
                ws.join(SKILLS_DIR),
                ws.join("crates").join(SKILLS_DIR),
                ws.join("tmp").join(SKILLS_DIR),
                ws.join("crates/saddle").join(SKILLS_DIR),
            ],
            "every directory, shallowest first"
        );
    }

    #[test]
    fn the_project_scan_skips_dot_directories_node_modules_and_ignored_paths() {
        // The ignore files are what make a whole-tree scan affordable, and the dot rule is what
        // stops the scan entering `.agents` and re-reading the skills it is looking for.
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join(".gitignore"), "build/\nvendor\n").unwrap();
        for base in ["build", "vendor", "node_modules/pkg", ".hidden", "keep"] {
            let skills = ws.join(base).join(SKILLS_DIR);
            std::fs::create_dir_all(&skills).unwrap();
            std::fs::write(skills.join(FILE), "---\nname: x\ndescription: d\n---\n").unwrap();
        }
        assert_eq!(
            project_bases(ws),
            [ws.join("keep").join(SKILLS_DIR)],
            "only the directory nothing excludes"
        );
    }

    #[test]
    fn a_linked_skills_directory_is_read_and_a_loop_terminates() {
        // A skill installed by linking it in is the usual arrangement, and the entry is not a
        // directory, so a filter on the file type drops it before the SKILL.md check.
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let real = dir.path().join("elsewhere/skills");
        std::fs::create_dir_all(real.join("prose-lint")).unwrap();
        std::fs::write(
            real.join("prose-lint").join(FILE),
            "---\nname: prose-lint\ndescription: d\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.join(".agents")).unwrap();
        std::os::unix::fs::symlink(&real, ws.join(SKILLS_DIR)).unwrap();
        assert_eq!(project_bases(&ws), [ws.join(SKILLS_DIR)]);

        // A link that loops must not walk for ever.
        std::os::unix::fs::symlink(&ws, ws.join("loop")).unwrap();
        assert_eq!(project_bases(&ws).len(), 1, "the cycle is visited once");
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
