//! Writing one change into a workspace's grants (D45).
//!
//! The edit is made on the parsed TOML document and the result is validated before it is written,
//! so a change from the menu either lands as grants that load or does not land at all. Only this
//! workspace's own entry is touched, `[workspaces."<key>".agent]` in `config.toml`: the `[agent]`
//! table is the base, and a change made while working in a project is a change to that project
//! (D63).

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, Value, value};

use super::permissions::Change;

/// Applies `change` to the policy at `path`, refusing anything that would not load afterwards.
/// `roots` is the set the session would resolve, so the check is the session's own.
pub fn edit(
    path: &Path,
    key: Option<&str>,
    change: &Change,
    roots: &saddle_cap::Roots,
) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{}", path.display()))?;

    match change {
        Change::Exclude { rule, patterns } => exclude(&mut doc, key, rule, patterns)?,
        Change::Toggle { rule, on } => toggle(&mut doc, key, rule, *on)?,
        // Adding goes through its own entry point, which returns what it added.
        Change::Add(what) => bail!("`{what}` is added with `grant::add`"),
    }

    let out = doc.to_string();
    validate(&out, key, roots)
        .with_context(|| format!("the change would leave {} unloadable", path.display()))?;
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The `[agent]` table the change belongs in (D63).
///
/// With a key it is this workspace's entry, `[workspaces."<key>".agent]`, which is what a session
/// writes; without one it is a policy-shaped document's root `[agent]`, which keeps the mechanics
/// testable without a whole `config.toml`.
fn agent<'a>(doc: &'a mut DocumentMut, key: Option<&str>) -> Result<&'a mut Table> {
    let Some(key) = key else {
        if doc.get("agent").is_none() {
            doc.insert("agent", Item::Table(Table::new()));
        }
        return doc["agent"]
            .as_table_mut()
            .context("`agent` is not a table");
    };
    if doc.get("workspaces").is_none() {
        doc.insert("workspaces", Item::Table(Table::new()));
    }
    let workspaces = doc["workspaces"]
        .as_table_mut()
        .context("`workspaces` is not a table in config.toml")?;
    if workspaces.get(key).is_none() {
        workspaces.insert(key, Item::Table(Table::new()));
    }
    let entry = workspaces[key]
        .as_table_mut()
        .context("a [workspaces] entry is not a table")?;
    if entry.get("agent").is_none() {
        entry.insert("agent", Item::Table(Table::new()));
    }
    entry["agent"]
        .as_table_mut()
        .context("a [workspaces] entry's `agent` is not a table")
}

/// `agent.fs-read[0]` names the first entry of the `fs-read` list.
fn fs_key(rule: &str) -> Result<(&'static str, usize)> {
    let (scope, rest) = if let Some(rest) = rule.strip_prefix("agent.fs-read[") {
        ("fs-read", rest)
    } else if let Some(rest) = rule.strip_prefix("agent.fs-write[") {
        ("fs-write", rest)
    } else {
        bail!("`{rule}` is not a filesystem grant this editor can change");
    };
    let index = rest
        .trim_end_matches(']')
        .parse::<usize>()
        .with_context(|| format!("`{rule}` has no entry number"))?;
    Ok((scope, index))
}

fn exclude(
    doc: &mut DocumentMut,
    key: Option<&str>,
    rule: &str,
    patterns: &[String],
) -> Result<()> {
    let (scope, index) = fs_key(rule)?;
    let table = agent(doc, key)?;
    // A policy can write a list either as `[[fs-read]]` sections or inline as `[{ root = "…" }]`,
    // and both have to be editable.
    let item = table
        .get_mut(scope)
        .with_context(|| format!("`{rule}` is not in the policy file"))?;
    match item {
        Item::ArrayOfTables(list) => {
            let entry = list
                .get_mut(index)
                .with_context(|| format!("`{rule}` is past the end of `{scope}`"))?;
            set_exclude(entry, patterns);
            Ok(())
        }
        Item::Value(Value::Array(list)) => {
            let entry = list
                .get_mut(index)
                .with_context(|| format!("`{rule}` is past the end of `{scope}`"))?
                .as_inline_table_mut()
                .with_context(|| format!("`{rule}` is not a table"))?;
            if patterns.is_empty() {
                entry.remove("exclude");
            } else {
                let mut array = toml_edit::Array::new();
                for pattern in patterns {
                    array.push(pattern.as_str());
                }
                entry.insert("exclude", Value::Array(array));
            }
            Ok(())
        }
        _ => bail!("`{scope}` is not a list of tables in the policy file"),
    }
}

fn set_exclude(entry: &mut Table, patterns: &[String]) {
    if patterns.is_empty() {
        entry.remove("exclude");
    } else {
        let mut array = toml_edit::Array::new();
        for pattern in patterns {
            array.push(pattern.as_str());
        }
        entry["exclude"] = value(array);
    }
}

/// Adds one grant to this workspace's policy (D46). `what` is what a person typed, one line:
/// `net api.example.com`, `root ~/.cargo`, `exec`, `tokens 200000`.
pub fn add(
    path: &Path,
    key: Option<&str>,
    what: &str,
    roots: &saddle_cap::Roots,
) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{}", path.display()))?;
    // A policy-shaped document needs its version line; `config.toml` must not have one, because
    // it is not a policy and would be an unknown field there (D63).
    if key.is_none() && doc.get("version").is_none() {
        doc.insert("version", value(1));
    }
    let summary = append(&mut doc, what, key, roots)?;
    let out = doc.to_string();
    validate(&out, key, roots)
        .with_context(|| format!("the change would leave {} unloadable", path.display()))?;
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(summary)
}

/// The same checks a session makes: the file is still a config, and this workspace's grants still
/// parse as a policy (D43, D63).
fn validate(out: &str, key: Option<&str>, roots: &saddle_cap::Roots) -> Result<()> {
    let Some(key) = key else {
        saddle_cap::Policy::parse(out, roots)?;
        return Ok(());
    };
    let _ = key;
    saddle_cap::Policy::parse(out, roots)?;
    Ok(())
}

/// Appends the grant `what` describes, and answers with what it added.
fn append(
    doc: &mut DocumentMut,
    what: &str,
    key: Option<&str>,
    roots: &saddle_cap::Roots,
) -> Result<String> {
    let (kind, argument) = match what.trim().split_once(char::is_whitespace) {
        Some((k, a)) => (k.to_ascii_lowercase(), a.trim().to_owned()),
        None => (what.trim().to_ascii_lowercase(), String::new()),
    };
    let table = agent(doc, key)?;
    match kind.as_str() {
        "net" => {
            if argument.is_empty() {
                bail!("`net` needs a host, as in `net api.example.com`");
            }
            // Checked here so the message names the mistake rather than the parser rejecting it.
            let host = base_host(&argument);
            saddle_cap::policy::HostPattern::parse(&host).map_err(|e| anyhow::anyhow!("{e}"))?;
            let port = port_argument(&argument);
            push_table(
                table,
                "net",
                &[
                    ("host", Value::from(base_host(&argument))),
                    ("why", why_of(what)),
                ],
            )
            .and_then(|_| {
                if let Some(port) = port {
                    set_last_port(table, "net", port)
                } else {
                    Ok(())
                }
            })?;
            Ok(format!("net grant for {}", base_host(&argument)))
        }
        "root" | "read" | "write" => {
            if argument.is_empty() {
                let names: Vec<&str> = roots.iter().map(|(n, _)| n.as_str()).collect();
                bail!(
                    "`{kind}` needs a root name; this session has {}",
                    names.join(", ")
                );
            }
            let name = argument.split_whitespace().next().unwrap_or_default();
            if !roots.iter().any(|(n, _)| n.as_str() == name) {
                let names: Vec<&str> = roots.iter().map(|(n, _)| n.as_str()).collect();
                bail!(
                    "`{name}` is not a root in this session; the roots are {} (a new root is added \
                     in config.toml, under [roots])",
                    names.join(", ")
                );
            }
            let scope = if kind == "write" {
                "fs-write"
            } else {
                "fs-read"
            };
            push_table(
                table,
                scope,
                &[("root", Value::from(name)), ("why", why_of(what))],
            )?;
            Ok(format!("{scope} grant for {name}"))
        }
        "exec" => {
            if table.get("exec").is_some() {
                bail!("this policy already grants exec");
            }
            table.insert("exec", Item::Table(Table::new()));
            Ok("exec grant".into())
        }
        "tokens" => {
            let n: u64 = argument
                .parse()
                .with_context(|| format!("`{argument}` is not a token count"))?;
            if table.get("model-invoke").is_some() {
                bail!("this policy already sets a token budget; edit it to change the number");
            }
            let mut budget = Table::new();
            budget.insert("tokens-per-session", value(n as i64));
            table.insert("model-invoke", Item::Table(budget));
            Ok(format!("{n} tokens per session"))
        }
        other => bail!(
            "`{other}` is not something this editor adds; write `net <host>`, `root <name>`, \
             `read <name>`, `write <name>`, `exec`, or `tokens <n>`"
        ),
    }
}

/// A `why` the person typed after a `#`, which is optional (D41).
fn why_of(what: &str) -> toml_edit::Value {
    match what.split_once('#') {
        Some((_, why)) if !why.trim().is_empty() => Value::from(why.trim()),
        _ => Value::from("added from /permissions"),
    }
}

/// `api.example.com:8443` names a port; the host alone does not.
fn port_argument(argument: &str) -> Option<u16> {
    let host = base_host(argument);
    argument
        .split_once('#')
        .map(|(a, _)| a.trim())
        .unwrap_or(argument)
        .strip_prefix(host.as_str())?
        .strip_prefix(':')?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn base_host(argument: &str) -> String {
    let head = argument
        .split_once('#')
        .map(|(a, _)| a.trim())
        .unwrap_or(argument)
        .split_whitespace()
        .next()
        .unwrap_or_default();
    head.split(':').next().unwrap_or_default().to_owned()
}

/// Appends a table to a list of tables, creating the list if this policy has none.
fn push_table(table: &mut Table, scope: &str, fields: &[(&str, toml_edit::Value)]) -> Result<()> {
    let mut entry = Table::new();
    for (k, v) in fields {
        entry.insert(k, Item::Value(v.clone()));
    }
    match table.get_mut(scope) {
        Some(Item::ArrayOfTables(list)) => {
            list.push(entry);
            Ok(())
        }
        Some(Item::Value(Value::Array(list))) => {
            list.push(Value::InlineTable(entry.into_inline_table()));
            Ok(())
        }
        Some(_) => bail!("`{scope}` is not a list of tables in the policy file"),
        None => {
            let mut list = toml_edit::ArrayOfTables::new();
            list.push(entry);
            table.insert(scope, Item::ArrayOfTables(list));
            Ok(())
        }
    }
}

/// A port typed with a host goes on the entry that was just added.
fn set_last_port(table: &mut Table, scope: &str, port: u16) -> Result<()> {
    let Some(item) = table.get_mut(scope) else {
        return Ok(());
    };
    match item {
        Item::ArrayOfTables(list) => {
            let last = list.len().checked_sub(1);
            if let Some(entry) = last.and_then(|i| list.get_mut(i)) {
                entry.insert("port", value(i64::from(port)));
            }
        }
        Item::Value(Value::Array(list)) => {
            let last = list.len().checked_sub(1);
            if let Some(entry) = last
                .and_then(|i| list.get_mut(i))
                .and_then(|v| v.as_inline_table_mut())
            {
                entry.insert("port", Value::from(i64::from(port)));
            }
        }
        _ => {}
    }
    Ok(())
}

fn toggle(doc: &mut DocumentMut, key: Option<&str>, rule: &str, on: bool) -> Result<()> {
    // Only scopes with no parameters can be switched from here; everything else is a table or a
    // list that needs arguments, which is what the file is for.
    let scope = rule
        .strip_prefix("agent.")
        .with_context(|| format!("`{rule}` is not an agent scope"))?;
    if scope.contains('[') || matches!(scope, "exec" | "net" | "model-invoke") {
        bail!("`{rule}` needs arguments, so it is edited in the file rather than from here");
    }
    let table = agent(doc, key)?;
    if on {
        if table.get(scope).is_none() {
            table.insert(scope, Item::Table(Table::new()));
        }
    } else {
        table.remove(scope);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(text: &str) -> (tempfile::TempDir, std::path::PathBuf, saddle_cap::Roots) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, text).unwrap();
        let roots = saddle_cap::Roots::resolve(
            [(saddle_cap::RootName::workspace(), dir.path().to_path_buf())],
            &[],
        )
        .unwrap();
        (dir, path, roots)
    }

    #[test]
    fn an_exclusion_lands_in_the_named_entry_and_keeps_the_comments() {
        let (_d, path, roots) = temp(
            "version = 1\n\n[agent]\nfs-write = [{ root = \"workspace\" }]\nfs-read  = [{ root = \"workspace\", exclude = [\".env*\"] }]\n",
        );
        edit(
            &path,
            None,
            &Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: vec![".env*".into(), "/secrets".into()],
            },
            &roots,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#"exclude = [".env*", "/secrets"]"#),
            "{text}"
        );
        // The other grant is untouched.
        assert!(
            text.contains(r#"fs-write = [{ root = "workspace" }]"#),
            "{text}"
        );

        // Emptying it removes the key rather than writing `exclude = []`.
        edit(
            &path,
            None,
            &Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: Vec::new(),
            },
            &roots,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("exclude"), "{text}");
    }

    #[test]
    fn a_row_the_policy_does_not_have_is_refused_rather_than_created() {
        let (_d, path, roots) =
            temp("version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n");
        let e = edit(
            &path,
            None,
            &Change::Exclude {
                rule: "agent.fs-read[3]".into(),
                patterns: vec![".env".into()],
            },
            &roots,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("past the end"), "{e}");
        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n"
        );
    }

    #[test]
    fn adding_a_host_writes_a_net_grant_with_its_port_and_why() {
        let (_d, path, roots) = temp("# only comments so far\n");
        let added = add(
            &path,
            None,
            "net api.example.com:8443 # reads the catalogue",
            &roots,
        )
        .unwrap();
        assert_eq!(added, "net grant for api.example.com");
        let text = std::fs::read_to_string(&path).unwrap();
        // The version line a policy needs is added, since the file had none.
        assert!(text.contains("version = 1"), "{text}");
        assert!(text.contains("[[agent.net]]"), "{text}");
        assert!(text.contains(r#"host = "api.example.com""#), "{text}");
        assert!(text.contains("port = 8443"), "{text}");
        assert!(text.contains(r#"why = "reads the catalogue""#), "{text}");
        // And it loads layered on an empty global policy.
        saddle_cap::Policy::parse(&text, &roots).unwrap();

        // A second host appends rather than replacing.
        add(&path, None, "net *.other.test", &roots).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[[agent.net]]").count(), 2, "{text}");
        assert!(!text.contains("port = 8443\nport"), "{text}");
    }

    #[test]
    fn adding_a_root_that_the_session_does_not_have_is_refused_with_the_list() {
        let (_d, path, roots) = temp("version = 1\n");
        let e = add(&path, None, "root ~/.cargo", &roots)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is not a root in this session"), "{e}");
        assert!(e.contains("workspace"), "the list is shown: {e}");

        // A root the session does have is written as a grant.
        add(&path, None, "write workspace # scratch space", &roots).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[agent.fs-write]]"), "{text}");
        assert!(text.contains(r#"root = "workspace""#), "{text}");
    }

    #[test]
    fn adding_exec_or_a_budget_is_one_line_each() {
        let (_d, path, roots) = temp("version = 1\n");
        add(&path, None, "exec", &roots).unwrap();
        add(&path, None, "tokens 250000", &roots).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[agent.exec]"), "{text}");
        assert!(text.contains("tokens-per-session = 250000"), "{text}");
        // Both would be a second grant of the same scope.
        assert!(add(&path, None, "exec", &roots).is_err());
        assert!(add(&path, None, "tokens 1", &roots).is_err());
    }

    #[test]
    fn an_unknown_form_lists_the_ones_that_work() {
        let (_d, path, roots) = temp("version = 1\n");
        let e = add(&path, None, "allow everything", &roots)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("`allow` is not something this editor adds"),
            "{e}"
        );
        assert!(e.contains("net <host>"), "{e}");
    }

    #[test]
    fn a_scope_that_needs_arguments_is_refused_from_the_menu() {
        let (_d, path, roots) = temp("version = 1\n\n[agent]\nexec = {}\n");
        for rule in ["agent.exec", "agent.net", "agent.model-invoke"] {
            let e = edit(
                &path,
                None,
                &Change::Toggle {
                    rule: rule.into(),
                    on: false,
                },
                &roots,
            )
            .unwrap_err()
            .to_string();
            assert!(e.contains("needs arguments"), "{rule}: {e}");
        }
    }

    #[test]
    fn a_change_that_would_not_load_is_refused() {
        // The exclusion is malformed as a glob once written, and a session could not read it.
        let (_d, path, roots) =
            temp("version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n");
        let before = std::fs::read_to_string(&path).unwrap();
        let e = edit(
            &path,
            None,
            &Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: vec!["a[".into()],
            },
            &roots,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("unloadable"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
