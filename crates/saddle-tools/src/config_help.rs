//! D171: saddle's own configuration reference, as a shipped standard component. The text is
//! compiled into the guest, and the only thing the guest imports for it is the budgets snapshot,
//! which is ambient and carries no authority (D145, D150): no path, no host, no grant. That is the
//! point — a session opened in someone else's project has no copy of saddle's documentation, and
//! answering from a place that holds none of the session's reach is how it stays a documentation
//! tool rather than one more surface to escape through.

use std::fmt::Write as _;

use saddle_limits::Budgets;
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use saddle_limits as budgets;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::configure_limits;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, Output, Tool};
#[cfg(target_arch = "wasm32")]
use serde_json::json;

/// `docs/settings.md`, the reference a person reads in a checkout of saddle.
const REFERENCE: &str = include_str!("../../../docs/settings.md");

pub struct ConfigHelp;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    /// One `## section` of the reference, or absent for all of them.
    #[serde(default)]
    section: Option<String>,
}

/// One `## name` section of the reference.
struct Section<'a> {
    name: &'a str,
    body: &'a str,
}

fn sections(text: &'static str) -> Vec<Section<'static>> {
    // The heading's own start, its name, and where its body begins.
    let mut found: Vec<(usize, &'static str, usize)> = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if let Some(name) = line.strip_prefix("## ") {
            found.push((at, name.trim(), at + line.len()));
        }
        at += line.len();
    }
    let mut out = Vec::new();
    for (i, (_, name, body)) in found.iter().enumerate() {
        let end = found.get(i + 1).map_or(text.len(), |next| next.0);
        out.push(Section {
            name,
            body: &text[*body..end],
        });
    }
    out
}

pub fn reference(arguments: &str, limits: &Budgets) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    answer(args.section.as_deref(), limits)
}

/// The reference, whole or one section of it, plus the budgets this session is running under.
pub fn answer(section: Option<&str>, limits: &Budgets) -> Result<String, String> {
    let asked = section.unwrap_or_default().trim();
    let all = sections(REFERENCE);
    let names: Vec<&str> = all.iter().map(|part| part.name).collect();
    let wanted: Vec<&Section> = if asked.is_empty() {
        all.iter().collect()
    } else {
        match all
            .iter()
            .find(|part| part.name.eq_ignore_ascii_case(asked))
        {
            Some(part) => vec![part],
            None => {
                return Err(format!(
                    "no section `{asked}`; the sections are: {}. Ask for one by name, or leave `section` out for all of them.",
                    names.join(", ")
                ));
            }
        }
    };
    let mut parts: Vec<String> = Vec::new();
    for part in wanted {
        let mut text = format!("## {}\n{}", part.name, part.body.trim_end());
        if part.name == "tool-limits" {
            text.push_str(&in_force(limits));
        }
        parts.push(text.trim_end().to_owned());
    }
    let mut out = String::new();
    if asked.is_empty() {
        let _ = write!(
            out,
            "saddle's configuration reference, in these sections: {}.\nAsk for one by name to get just that one.\n\n",
            names.join(", ")
        );
    }
    out.push_str(&parts.join("\n\n"));
    Ok(out.trim_end().to_owned())
}

/// The budgets in force, which `/reload` may have changed from the defaults, kebab-case as
/// `config.toml` spells them.
fn in_force(limits: &Budgets) -> String {
    let mut out = String::from("\n\nin force for this session:\n");
    match serde_json::to_value(limits) {
        Ok(serde_json::Value::Object(table)) => {
            for (name, value) in table {
                let _ = writeln!(out, "  {name} = {value}");
            }
        }
        Ok(_) => out.push_str("  the budgets could not be read\n"),
        Err(error) => {
            let _ = writeln!(out, "  the budgets could not be printed: {error}");
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
impl Tool for ConfigHelp {
    fn describe() -> Definition {
        Definition {
            name: "config_help".into(),
            description: "Read saddle's own configuration reference: where each settings file \
                          lives, every section and key with what it does, the grants and sandbox \
                          paths it accepts, and the resource budgets in force. Use it when a \
                          setting, a grant, a sandbox path, a limit or a provider looks wrong, and \
                          answer with the file and key to change rather than a guess. It reads no \
                          file of yours: it describes them."
                .into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "section":{"type":"string","description":"one section by name: files, top-level, agent, roots, sandbox, launch-profiles, standard-tools, plugins, ui, keys, compaction, context, skills, themes, providers, mcp, tool-limits, commands or problems; omitted returns all of them"}
                },
                "additionalProperties":false
            })
            .to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
        if let Err(text) = configure_limits() {
            return Output {
                text,
                is_error: true,
            };
        }
        match reference(&arguments, &budgets::current()) {
            Ok(text) => Output {
                text,
                is_error: false,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The names the schema offers, so a section the reference lost is a failing test rather than
    /// an argument the model is told about and refused.
    const OFFERED: &[&str] = &[
        "files",
        "top-level",
        "agent",
        "roots",
        "sandbox",
        "standard-tools",
        "plugins",
        "ui",
        "keys",
        "compaction",
        "context",
        "skills",
        "themes",
        "providers",
        "mcp",
        "tool-limits",
        "commands",
        "problems",
    ];

    #[test]
    fn the_reference_has_exactly_the_sections_the_schema_names() {
        let found: BTreeSet<&str> = sections(REFERENCE)
            .iter()
            .map(|section| section.name)
            .collect();
        let expected: BTreeSet<&str> = OFFERED.iter().copied().collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_section_is_answered_alone_and_an_unknown_one_lists_the_rest() {
        let all = answer(None, &Budgets::default()).unwrap();
        assert!(all.starts_with("saddle's configuration reference"), "{all}");
        for name in OFFERED {
            assert!(all.contains(&format!("## {name}\n")), "{name} is missing");
        }
        let agent = answer(Some("agent"), &Budgets::default()).unwrap();
        assert!(agent.starts_with("## agent\n"), "{agent}");
        assert!(agent.contains("fs-read") && agent.contains("transcript-read"));
        assert!(!agent.contains("## ui"), "a later section does not leak in");
        let unknown = answer(Some("nonsense"), &Budgets::default()).unwrap_err();
        assert!(unknown.contains("the sections are:"), "{unknown}");
    }

    #[test]
    fn the_budgets_in_force_are_the_ones_the_session_runs_under() {
        let limits = Budgets {
            query_rows: 7,
            ..Budgets::default()
        };
        let text = answer(Some("tool-limits"), &limits).unwrap();
        assert!(text.contains("  query-rows = 7"), "{text}");
        assert!(
            !text.contains(&format!("query-rows = {}", Budgets::default().query_rows)),
            "the changed value, not the default: {text}"
        );
    }

    #[test]
    fn invalid_arguments_are_refused_before_anything_is_answered() {
        for arguments in ["{}", r#"{"section":1}"#, r#"{"other":"agent"}"#] {
            if arguments == "{}" {
                assert!(reference(arguments, &Budgets::default()).is_ok());
                continue;
            }
            assert!(
                reference(arguments, &Budgets::default()).is_err(),
                "{arguments}"
            );
        }
    }
}
