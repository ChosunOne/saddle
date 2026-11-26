use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use saddle_cap::Policy;
use saddle_cap::Roots;
use saddle_cap::policy::{HostPattern, POLICY_VERSION, UrlScheme};
use toml::{Table as TomlTable, Value as TomlValue};
use toml_edit::{DocumentMut, Item, Table, Value, value};

use super::permissions::Change;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub summary: String,
    pub line: String,
    pub note: Option<String>,
}

pub fn edit(path: &Path, change: &Change, roots: &Roots) -> Result<()> {
    let mut doc = read(path)?;
    match change {
        Change::Exclude { rule, patterns } => exclude(&mut doc, rule, patterns)?,
        Change::Toggle { rule, on } => toggle(&mut doc, rule, *on)?,
        Change::Add(what) => bail!("`{what}` is added with `grant::add`"),
    }
    publish(path, &doc, roots)
}

pub fn add(path: &Path, what: &str, roots: &Roots) -> Result<Preview> {
    let mut doc = read(path)?;
    let preview = append(&mut doc, what, roots, None)?;
    publish(path, &doc, roots)?;
    Ok(preview)
}

pub fn draft(text: &str, base: &str, what: &str, roots: &Roots) -> Result<Preview> {
    let mut doc = document(text).context("this project's grants file")?;
    let preview = append(&mut doc, what, roots, Some(base))?;
    validate(&doc.to_string(), roots).context("the change would leave the file unloadable")?;
    Ok(preview)
}

fn document(text: &str) -> Result<DocumentMut> {
    let mut doc: DocumentMut = text.parse().context("the file is not TOML")?;
    if doc.get("version").is_none() {
        doc.insert("version", value(i64::from(POLICY_VERSION)));
    }
    Ok(doc)
}

fn read(path: &Path) -> Result<DocumentMut> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    document(&text).with_context(|| format!("{}", path.display()))
}

fn publish(path: &Path, doc: &DocumentMut, roots: &Roots) -> Result<()> {
    let out = doc.to_string();
    validate(&out, roots)
        .with_context(|| format!("the change would leave {} unloadable", path.display()))?;
    fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

fn validate(out: &str, roots: &Roots) -> Result<()> {
    let doc: TomlTable = toml::from_str(out).context("the file is not TOML")?;
    let version = doc
        .get("version")
        .cloned()
        .unwrap_or(TomlValue::Integer(i64::from(POLICY_VERSION)));
    let mut policy = TomlTable::new();
    policy.insert("version".into(), version);
    for key in ["agent", "plugin"] {
        if let Some(value) = doc.get(key) {
            policy.insert(key.into(), value.clone());
        }
    }
    Policy::parse(
        &toml::to_string(&policy).context("reading the grants")?,
        roots,
    )?;
    Ok(())
}

fn agent(doc: &mut DocumentMut) -> Result<&mut Table> {
    if doc.get("agent").is_none() {
        doc.insert("agent", Item::Table(Table::new()));
    }
    doc["agent"]
        .as_table_mut()
        .context("`agent` is not a table")
}

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

fn exclude(doc: &mut DocumentMut, rule: &str, patterns: &[String]) -> Result<()> {
    let (scope, index) = fs_key(rule)?;
    let table = agent(doc)?;
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

fn split_what(what: &str) -> (String, String) {
    let what = what.trim();
    match what.split_once(char::is_whitespace) {
        Some((kind, rest)) => (kind.to_ascii_lowercase(), rest.trim().to_owned()),
        None => (what.to_ascii_lowercase(), String::new()),
    }
}

fn append(doc: &mut DocumentMut, what: &str, roots: &Roots, base: Option<&str>) -> Result<Preview> {
    let (kind, argument) = split_what(what);
    match kind.as_str() {
        "net" => {
            let net = parse_net(&argument)?;
            let note = note_for(doc, base, "net");
            let fields = [
                ("host", Value::from(net.host.to_string())),
                ("scheme", Value::from(net.scheme.as_str())),
                ("port", Value::from(i64::from(net.port))),
                ("why", why_of(what)),
            ];
            push_table(agent(doc)?, "net", &fields)?;
            Ok(Preview {
                summary: net.summary(),
                line: net.line(),
                note,
            })
        }
        "root" | "read" | "write" => {
            let name = argument.split_whitespace().next().unwrap_or_default();
            if name.is_empty() {
                bail!(
                    "`{kind}` needs a root name; this session has {}",
                    root_names(roots)
                );
            }
            if !roots.iter().any(|(n, _)| n.as_str() == name) {
                bail!(
                    "`{name}` is not a root in this session; the roots are {} (a new root is added \
                     in config.toml, under [roots])",
                    root_names(roots)
                );
            }
            let scope = if kind == "write" {
                "fs-write"
            } else {
                "fs-read"
            };
            let note = note_for(doc, base, scope);
            let fields = [("root", Value::from(name)), ("why", why_of(what))];
            push_table(agent(doc)?, scope, &fields)?;
            Ok(Preview {
                summary: format!("{scope} grant for {name}"),
                line: format!("[[agent.{scope}]]  root = \"{name}\""),
                note,
            })
        }
        "exec" => {
            let table = agent(doc)?;
            if table.get("exec").is_some() {
                bail!("this policy already grants exec");
            }
            let mut entry = Table::new();
            entry.insert("why", Item::Value(why_of(what)));
            table.insert("exec", Item::Table(entry));
            Ok(Preview {
                summary: "exec grant".into(),
                line: "[agent.exec]".into(),
                note: None,
            })
        }
        "tokens" => {
            let amount = argument.split('#').next().unwrap_or_default().trim();
            let tokens: u64 = amount
                .parse()
                .with_context(|| format!("`{amount}` is not a token count"))?;
            let tokens =
                i64::try_from(tokens).context("that is more tokens than a policy can hold")?;
            let table = agent(doc)?;
            if table.get("model-invoke").is_none() {
                table.insert("model-invoke", Item::Table(Table::new()));
            }
            let budget = match table.get_mut("model-invoke") {
                Some(Item::Table(budget)) => budget,
                _ => bail!("model-invoke in this policy is not a table; edit it to add a budget"),
            };
            if budget.get("tokens-per-session").is_some() {
                bail!("this policy already sets a token budget; edit it to change the number");
            }
            budget.insert("tokens-per-session", value(tokens));
            Ok(Preview {
                summary: format!("{tokens} tokens per session"),
                line: format!("[agent.model-invoke]  tokens-per-session = {tokens}"),
                note: None,
            })
        }
        other => bail!(
            "`{other}` is not something this editor adds; write `net <host>`, `root <name>`, \
             `read <name>`, `write <name>`, `exec`, or `tokens <n>`"
        ),
    }
}

fn root_names(roots: &Roots) -> String {
    roots
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Net {
    host: HostPattern,
    scheme: UrlScheme,
    port: u16,
}

impl Net {
    fn summary(&self) -> String {
        let url = format!("{}://{}:{}", self.scheme.as_str(), self.host, self.port);
        if matches!(self.host, HostPattern::Any) {
            format!("net grant for {url} (any public host)")
        } else {
            format!("net grant for {url}")
        }
    }

    fn line(&self) -> String {
        format!(
            "[[agent.net]]  host = \"{}\"  scheme = \"{}\"  port = {}",
            self.host,
            self.scheme.as_str(),
            self.port
        )
    }
}

fn parse_net(argument: &str) -> Result<Net> {
    let address = argument.split('#').next().unwrap_or_default().trim();
    let (scheme, rest) = match address.split_once("://") {
        Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
            "https" => (UrlScheme::Https, rest),
            "http" => (UrlScheme::Http, rest),
            other => bail!(
                "`{other}://` is not a scheme a `net` grant can carry; write `http://` or \
                 `https://`, or leave the scheme out for https"
            ),
        },
        None => (UrlScheme::Https, address),
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    let (host, port) = host_and_port(authority)?;
    if host.is_empty() {
        bail!("`net` needs a host, as in `net api.example.com` or `net https://api.example.com/`");
    }
    let host = HostPattern::parse(&host).map_err(|e| anyhow::anyhow!("{e}"))?;
    let port = match port {
        Some(0) => bail!("0 is not a port a grant can name"),
        Some(port) => port,
        None => scheme.default_port(),
    };
    Ok(Net { host, scheme, port })
}

fn host_and_port(authority: &str) -> Result<(String, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .with_context(|| format!("`{authority}` has no closing `]`"))?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port_of(port)?),
            None => None,
        };
        return Ok((host.to_owned(), port));
    }
    if authority.matches(':').count() > 1 {
        return Ok((authority.to_owned(), None));
    }
    match authority.split_once(':') {
        Some((host, port)) => Ok((host.to_owned(), Some(port_of(port)?))),
        None => Ok((authority.to_owned(), None)),
    }
}

fn port_of(port: &str) -> Result<u16> {
    port.parse()
        .with_context(|| format!("`{port}` is not a port"))
}

fn note_for(doc: &DocumentMut, base: Option<&str>, scope: &str) -> Option<String> {
    let base = base?;
    let own = doc
        .get("agent")
        .and_then(Item::as_table)
        .and_then(|agent| agent.get(scope));
    if own.is_some() {
        return None;
    }
    let count = base_grants(base, scope);
    (count > 0).then(|| {
        format!(
            "this project's own list replaces config.toml's {count} `{scope}` {}: it will hold \
             only what you add here",
            if count == 1 { "entry" } else { "entries" }
        )
    })
}

fn base_grants(base: &str, scope: &str) -> usize {
    let Ok(doc) = base.parse::<TomlTable>() else {
        return 0;
    };
    doc.get("agent")
        .and_then(|agent| agent.get(scope))
        .and_then(TomlValue::as_array)
        .map_or(0, Vec::len)
}

fn why_of(what: &str) -> toml_edit::Value {
    match what.split_once('#') {
        Some((_, why)) if !why.trim().is_empty() => Value::from(why.trim()),
        _ => Value::from("added from /permissions"),
    }
}

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

fn toggle(doc: &mut DocumentMut, rule: &str, on: bool) -> Result<()> {
    let scope = rule
        .strip_prefix("agent.")
        .with_context(|| format!("`{rule}` is not an agent scope"))?;
    if scope.contains('[') || matches!(scope, "exec" | "net" | "model-invoke") {
        bail!("`{rule}` needs arguments, so it is edited in the file rather than from here");
    }
    let table = agent(doc)?;
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
    use crate::session::workspace_header;
    use saddle_cap::RootName;
    use std::fs;
    use std::path::PathBuf;

    fn temp(text: &str) -> (tempfile::TempDir, PathBuf, Roots) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        fs::write(&path, text).unwrap();
        let roots =
            Roots::resolve([(RootName::workspace(), dir.path().to_path_buf())], &[]).unwrap();
        (dir, path, roots)
    }

    #[test]
    fn an_exclusion_lands_in_the_named_entry_and_keeps_the_comments() {
        let (_d, path, roots) = temp(
            "version = 1\n\n[agent]\nfs-write = [{ root = \"workspace\" }]\nfs-read  = [{ root = \"workspace\", exclude = [\".env*\"] }]\n",
        );
        edit(
            &path,
            &Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: vec![".env*".into(), "/secrets".into()],
            },
            &roots,
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#"exclude = [".env*", "/secrets"]"#),
            "{text}"
        );
        assert!(
            text.contains(r#"fs-write = [{ root = "workspace" }]"#),
            "{text}"
        );

        edit(
            &path,
            &Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: Vec::new(),
            },
            &roots,
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("exclude"), "{text}");
    }

    #[test]
    fn a_row_the_policy_does_not_have_is_refused_rather_than_created() {
        let (_d, path, roots) =
            temp("version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n");
        let e = edit(
            &path,
            &Change::Exclude {
                rule: "agent.fs-read[3]".into(),
                patterns: vec![".env".into()],
            },
            &roots,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("past the end"), "{e}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n"
        );
    }

    #[test]
    fn the_file_a_session_creates_can_be_edited() {
        let text = workspace_header(Path::new("/home/me/proj"));
        let (_d, path, roots) = temp(&text);
        add(&path, "net api.example.com", &roots).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 1"), "{text}");
        assert!(text.contains(r#"host = "api.example.com""#), "{text}");
        assert!(text.contains(r#"workspace = "/home/me/proj""#), "{text}");
        let session_shape = text
            .parse::<TomlTable>()
            .unwrap()
            .get("agent")
            .cloned()
            .map(|agent| {
                let mut policy = TomlTable::new();
                policy.insert("version".into(), TomlValue::Integer(1));
                policy.insert("agent".into(), agent);
                toml::to_string(&policy).unwrap()
            })
            .unwrap();
        Policy::parse(&session_shape, &roots).unwrap();
    }

    #[test]
    fn adding_a_host_writes_its_scheme_and_port_and_why() {
        let (_d, path, roots) = temp("# only comments so far\n");
        let added = add(
            &path,
            "net api.example.com:8443 # reads the catalogue",
            &roots,
        )
        .unwrap();
        assert_eq!(added.summary, "net grant for https://api.example.com:8443");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 1"), "{text}");
        assert!(text.contains("[[agent.net]]"), "{text}");
        assert!(text.contains(r#"host = "api.example.com""#), "{text}");
        assert!(text.contains(r#"scheme = "https""#), "{text}");
        assert!(text.contains("port = 8443"), "{text}");
        assert!(text.contains(r#"why = "reads the catalogue""#), "{text}");
        Policy::parse(&text, &roots).unwrap();

        add(&path, "net *.other.test", &roots).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[[agent.net]]").count(), 2, "{text}");
        assert!(text.contains(r#"host = "*.other.test""#), "{text}");
        assert!(text.contains("port = 443"), "the scheme's own port: {text}");
    }

    #[test]
    fn a_url_names_the_host_the_scheme_and_the_port() {
        let (_d, path, roots) = temp("version = 1\n");
        let added = add(
            &path,
            "net https://api.example.com/v2/catalogue?q=1",
            &roots,
        )
        .unwrap();
        assert_eq!(added.summary, "net grant for https://api.example.com:443");
        let added = add(&path, "net http://localhost", &roots).unwrap();
        assert_eq!(added.summary, "net grant for http://localhost:80");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[[agent.net]]").count(), 2, "{text}");
        assert!(text.contains(r#"scheme = "http""#), "{text}");
        assert!(text.contains(r#"host = "api.example.com""#), "{text}");
        assert!(text.contains("port = 80"), "{text}");
        let added = add(&path, "net *", &roots).unwrap();
        assert!(added.summary.contains("any public host"), "{added:?}");
    }

    #[test]
    fn a_net_line_that_could_not_be_a_request_is_refused_with_the_part_to_fix() {
        let (_d, path, roots) = temp("version = 1\n");
        for (what, wanted) in [
            ("net", "needs a host"),
            ("net https://", "needs a host"),
            ("net ftp://files.example.com", "not a scheme"),
            ("net api.example.com:abc", "is not a port"),
            ("net api.example.com:0", "0 is not a port"),
            ("net api.exa mple.com", "is not a hostname"),
            ("net localhost:80:90", "is not a hostname"),
        ] {
            let e = add(&path, what, &roots).unwrap_err().to_string();
            assert!(e.contains(wanted), "`{what}`: {e}");
        }
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "version = 1\n",
            "nothing was written"
        );
    }

    #[test]
    fn the_prompt_preview_is_the_entry_the_file_gets() {
        let (_d, path, roots) = temp("version = 1\n");
        for what in [
            "net https://api.example.com/v2",
            "read workspace # reads the project",
            "write workspace",
            "exec",
            "tokens 250000",
        ] {
            let preview = draft("version = 1\n", "", what, &roots).unwrap();
            add(&path, what, &roots).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            let mut fields = preview.line.split("  ");
            let head = fields.next().unwrap_or_default();
            assert!(text.contains(head), "`{what}`: {head} missing from {text}");
            for field in fields {
                assert!(
                    text.contains(field),
                    "`{what}`: {field} missing from {text}"
                );
            }
        }
    }

    #[test]
    fn the_prompt_says_when_the_project_list_replaces_the_bases() {
        let (_d, _path, roots) = temp("version = 1\n");
        let base = "[agent]\nnet = [{ host = \"*\" }]\n";
        let preview = draft("version = 1\n", base, "net api.example.com", &roots).unwrap();
        let note = preview.note.unwrap();
        assert!(note.contains("replaces"), "{note}");
        assert!(note.contains("1 `net` entry"), "{note}");

        assert!(
            draft("version = 1\n", "", "net api.example.com", &roots)
                .unwrap()
                .note
                .is_none()
        );
        let own = "version = 1\n[agent]\nnet = [{ host = \"api.example.com\" }]\n";
        assert!(
            draft(own, base, "net other.example.com", &roots)
                .unwrap()
                .note
                .is_none()
        );
    }

    #[test]
    fn adding_a_root_that_the_session_does_not_have_is_refused_with_the_list() {
        let (_d, path, roots) = temp("version = 1\n");
        let e = add(&path, "root ~/.cargo", &roots).unwrap_err().to_string();
        assert!(e.contains("is not a root in this session"), "{e}");
        assert!(e.contains("workspace"), "the list is shown: {e}");

        add(&path, "write workspace # scratch space", &roots).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[agent.fs-write]]"), "{text}");
        assert!(text.contains(r#"root = "workspace""#), "{text}");
    }

    #[test]
    fn adding_exec_or_a_budget_is_one_line_each() {
        let (_d, path, roots) = temp("version = 1\n");
        add(&path, "exec", &roots).unwrap();
        add(&path, "tokens 250000 # for a long refactor", &roots).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[agent.exec]"), "{text}");
        assert!(text.contains("tokens-per-session = 250000"), "{text}");
        assert!(add(&path, "exec", &roots).is_err());
        assert!(add(&path, "tokens 1", &roots).is_err());
    }

    #[test]
    fn a_budget_is_added_into_an_existing_grant_that_has_none() {
        let (_d, path, roots) = temp("version = 1\n[agent.model-invoke]\nwhy = \"workers\"\n");
        add(&path, "tokens 5000", &roots).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("tokens-per-session = 5000"), "{text}");
        assert!(text.contains(r#"why = "workers""#), "{text}");
        assert!(add(&path, "tokens 1", &roots).is_err());
    }

    #[test]
    fn an_unknown_form_lists_the_ones_that_work() {
        let (_d, path, roots) = temp("version = 1\n");
        let e = add(&path, "allow everything", &roots)
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
        let (_d, path, roots) =
            temp("version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n");
        let before = fs::read_to_string(&path).unwrap();
        let e = edit(
            &path,
            &Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: vec!["a[".into()],
            },
            &roots,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("unloadable"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
}
