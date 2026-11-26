#[derive(Debug)]
pub enum Op {
    Add {
        path: String,
        text: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        destination: Option<String>,
        hunks: Vec<Hunk>,
    },
}
#[derive(Debug, Default)]
pub struct Hunk {
    at: String,
    old: Vec<String>,
    new: Vec<String>,
}

pub fn parse(text: &str) -> Result<Vec<Op>, String> {
    let mut lines = text.lines().peekable();
    let mut ops = Vec::new();
    while let Some(line) = lines.next() {
        let line = line.trim_end();
        if line == "*** Begin Patch" || line.trim().is_empty() {
            continue;
        }
        if line == "*** End Patch" {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File:") {
            let mut body = Vec::new();
            while let Some(line) = lines.peek() {
                if line.starts_with("*** ") {
                    break;
                }
                let line = lines.next().unwrap_or_default();
                body.push(line.strip_prefix('+').unwrap_or(line));
            }
            let mut text = body.join("\n");
            if !body.is_empty() {
                text.push('\n');
            }
            ops.push(Op::Add {
                path: path.trim().into(),
                text,
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            ops.push(Op::Delete {
                path: path.trim().into(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File:") {
            let mut destination = None;
            let mut hunks: Vec<Hunk> = Vec::new();
            while let Some(line) = lines.peek() {
                let line = line.trim_end();
                if line.starts_with("*** ") && !line.starts_with("*** Move to:") {
                    break;
                }
                let line = lines.next().unwrap_or_default();
                if let Some(to) = line.strip_prefix("*** Move to:") {
                    if destination.is_some() {
                        return Err("duplicate move destination".into());
                    }
                    destination = Some(to.trim().into());
                } else if line.starts_with("@@") {
                    hunks.push(Hunk {
                        at: line.into(),
                        ..Hunk::default()
                    });
                } else {
                    if hunks.is_empty() {
                        if line.is_empty() {
                            continue;
                        }
                        if !line.starts_with(['+', '-', ' ']) {
                            return Err("a change line has no hunk marker".into());
                        }
                        hunks.push(Hunk::default());
                    }
                    let hunk = hunks.last_mut().ok_or("missing patch hunk")?;
                    if let Some(line) = line.strip_prefix('+') {
                        hunk.new.push(line.into());
                    } else if let Some(line) = line.strip_prefix('-') {
                        hunk.old.push(line.into());
                    } else {
                        let line = line.strip_prefix(' ').unwrap_or(line);
                        hunk.old.push(line.into());
                        hunk.new.push(line.into());
                    }
                }
            }
            if hunks.iter().any(|hunk| hunk.old.is_empty()) {
                return Err("a hunk removes nothing, so there is nothing to match".into());
            }
            if hunks.is_empty() && destination.is_none() {
                return Err("an update with no hunks and no move".into());
            }
            ops.push(Op::Update {
                path: path.trim().into(),
                destination,
                hunks,
            });
        } else {
            return Err(format!("unexpected patch section: {line}"));
        }
    }
    if ops.is_empty() {
        return Err("the patch has no file sections".into());
    }
    if ops.len() > 32 {
        return Err("patch exceeds 32 file sections".into());
    }
    Ok(ops)
}

pub fn apply(text: &str, hunks: &[Hunk]) -> Result<String, String> {
    if hunks.is_empty() {
        return Ok(text.into());
    }
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let trailing = text.ends_with('\n') || text.is_empty();
    for hunk in hunks {
        let places: Vec<usize> = if hunk.old.is_empty() || hunk.old.len() > lines.len() {
            Vec::new()
        } else {
            lines
                .windows(hunk.old.len())
                .enumerate()
                .filter_map(|(i, window)| (window == hunk.old).then_some(i))
                .collect()
        };
        if places.len() != 1 {
            return Err(format!(
                "hunk `{}` matches {} places; include unique surrounding context",
                hunk.at,
                places.len()
            ));
        }
        let at = places[0];
        lines.splice(at..at + hunk.old.len(), hunk.new.iter().cloned());
    }
    let mut out = lines.join("\n");
    if trailing && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_context_and_moves_are_guest_owned() {
        let ops = parse("*** Begin Patch\n*** Add File: new\n+new\n*** Update File: old\n*** Move to: moved\n@@\n-old\n+updated\n keep\n*** Delete File: gone\n*** End Patch").unwrap();
        assert_eq!(ops.len(), 3);
        let Op::Update {
            destination, hunks, ..
        } = &ops[1]
        else {
            panic!("expected update");
        };
        assert_eq!(destination.as_deref(), Some("moved"));
        assert_eq!(apply("old\nkeep\n", hunks).unwrap(), "updated\nkeep\n");
        assert!(
            apply("old\nkeep\nold\nkeep\n", hunks)
                .unwrap_err()
                .contains("2 places")
        );
        assert!(apply("absent\n", hunks).unwrap_err().contains("0 places"));
    }

    #[test]
    fn hunk_text_keeps_significant_trailing_whitespace() {
        let ops = parse("*** Update File: file\n@@\n-old \t\n+new \t\n   \n").unwrap();
        let Op::Update { hunks, .. } = &ops[0] else {
            panic!("expected update");
        };
        assert_eq!(apply("old \t\n  \n", hunks).unwrap(), "new \t\n  \n");
        assert!(apply("old\n  \n", hunks).is_err());
    }

    #[test]
    fn malformed_or_unanchored_changes_are_not_guessed() {
        for text in [
            "",
            "*** Begin Patch\n*** End Patch",
            "*** Wat File: x",
            "*** Update File: x",
            "*** Update File: x\n@@\n+only",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        assert_eq!(apply("a\r\nb", &[]).unwrap(), "a\r\nb");
    }
}
