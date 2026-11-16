//! host-owned plugin frames with closed styles, never terminal programs (D27, D156).

use saddle_limits::Budgets;
use saddle_wit::saddle::widgets::types::{Panel, Style, Widget};
use serde::{Deserialize, Serialize};

use crate::term::clean;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Text,
    Muted,
    Success,
    Warning,
    Error,
    Emphasis,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    pub tone: Tone,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Row {
    Text(Vec<Span>),
    Progress {
        completed: u64,
        total: u64,
        label: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelView {
    pub plugin: String,
    pub title: String,
    pub rows: Vec<Row>,
}

fn plain(value: &str) -> String {
    clean(value)
        .chars()
        .filter(|c| !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .map(|c| if matches!(c, '\n' | '\t' | '\u{2028}' | '\u{2029}') { ' ' } else { c })
        .collect()
}

pub fn validate(plugin: &str, panel: Panel, limits: &Budgets) -> Result<PanelView, String> {
    if panel.widgets.len() > limits.ui_widgets {
        return Err("panel exceeds widget limit".into());
    }
    let mut bytes = panel.title.len();
    let mut rows = Vec::new();
    for widget in panel.widgets {
        match widget {
            Widget::Text(spans) => rows.push(Row::Text(
                spans
                    .into_iter()
                    .map(|span| {
                        bytes = bytes.saturating_add(span.text.len());
                        Span {
                            text: plain(&span.text),
                            tone: match span.style {
                                Style::Text => Tone::Text,
                                Style::Muted => Tone::Muted,
                                Style::Success => Tone::Success,
                                Style::Warning => Tone::Warning,
                                Style::Error => Tone::Error,
                                Style::Emphasis => Tone::Emphasis,
                            },
                        }
                    })
                    .collect(),
            )),
            Widget::Items(items) => {
                for item in items {
                    bytes = bytes.saturating_add(item.len());
                    rows.push(Row::Text(vec![Span {
                        text: format!("• {}", plain(&item)),
                        tone: Tone::Text,
                    }]));
                }
            }
            Widget::KeyValue(items) => {
                for item in items {
                    bytes = bytes
                        .saturating_add(item.key.len())
                        .saturating_add(item.value.len());
                    rows.push(Row::Text(vec![
                        Span {
                            text: plain(&item.key),
                            tone: Tone::Muted,
                        },
                        Span {
                            text: format!("  {}", plain(&item.value)),
                            tone: Tone::Text,
                        },
                    ]));
                }
            }
            Widget::Progress(value) => {
                if value.total == 0 || value.completed > value.total {
                    return Err("progress requires total > 0 and completed <= total".into());
                }
                bytes = bytes.saturating_add(value.label.len());
                rows.push(Row::Progress {
                    completed: value.completed,
                    total: value.total,
                    label: plain(&value.label),
                });
            }
        }
    }
    if bytes > limits.ui_bytes || rows.len() > limits.ui_rows {
        return Err("panel exceeds configured byte or row limit".into());
    }
    Ok(PanelView {
        plugin: plugin.into(),
        title: plain(&panel.title),
        rows,
    })
}

impl PanelView {
    pub fn text(&self) -> String {
        let mut output = format!("[{} panel] {}\n", self.plugin, self.title);
        for row in &self.rows {
            output.push_str("│ ");
            match row {
                Row::Text(spans) => {
                    for span in spans {
                        output.push_str(&span.text);
                    }
                }
                Row::Progress {
                    completed,
                    total,
                    label,
                } => output.push_str(&format!("{label}: {completed}/{total}")),
            }
            output.push('\n');
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use saddle_limits::Budgets;
    use saddle_wit::saddle::widgets::types::{Panel, Progress, Widget};

    use super::validate;

    #[test]
    fn panels_bound_expanded_rows_and_progress_and_keep_single_line_frames() {
        let limits = Budgets {
            ui_rows: 1,
            ..Budgets::default()
        };
        let oversized = Panel {
            title: "list".into(),
            widgets: vec![Widget::Items(vec!["a".into(), "b".into()])],
        };
        assert!(validate("plugin.real", oversized, &limits).is_err());
        for (completed, total) in [(0, 0), (2, 1)] {
            let invalid = Panel {
                title: "progress".into(),
                widgets: vec![Widget::Progress(Progress {
                    completed,
                    total,
                    label: "work".into(),
                })],
            };
            assert!(validate("plugin.real", invalid, &limits).is_err());
        }
        let value = Panel {
            title: "fake\n\u{2028}\u{202e}\u{1b}]52;clipboard\u{7}".into(),
            widgets: vec![Widget::Items(vec!["row\t\u{2066}text".into()])],
        };
        let panel = validate("plugin.real", value, &limits).unwrap();
        assert_eq!(panel.plugin, "plugin.real");
        assert_eq!(panel.title, "fake  ]52;clipboard");
        assert_eq!(panel.text().lines().count(), 2);
        assert!(!panel.text().contains('\u{2066}'));
    }
}
