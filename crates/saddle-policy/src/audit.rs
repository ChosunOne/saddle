//! The audit log (D24): JSON Lines, one record per decision or host event, each record carrying
//! the SHA-256 of the line before it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use saddle_cap::{Principal, Scope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::{Decision, Request};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// The session opened: resolved roots, policy hash, sandbox status.
    SessionStart,
    /// A policy decision.
    Decision,
    /// The host restored a reserved path that a sandboxed command changed (D20).
    Restore,
    /// Instruction files added to the system prompt (D32), with their hashes.
    Context,
    /// A skill (D64): the ones advertised at startup, and each body loaded into the conversation.
    Skill,
    /// The policy changed during the session (D47): its path, and the hash before and after.
    Policy,
    /// A request to the session's model provider: model, stop reason and token usage, or the
    /// error.
    Model,
    SessionEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub ts: String,
    pub session: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    pub args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub explanation: String,
    pub prev: String,
}

pub struct AuditLog {
    file: File,
    path: PathBuf,
    session: String,
    seq: u64,
    prev: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl AuditLog {
    /// Creates `<dir>/<session>.jsonl`. Fails if it already exists.
    pub fn create(dir: &Path, session: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{session}.jsonl"));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            session: session.to_owned(),
            seq: 0,
            prev: GENESIS.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn head(&self) -> &str {
        &self.prev
    }

    pub fn decision(&mut self, who: &Principal, req: &Request<'_>, d: &Decision) -> io::Result<()> {
        self.call_decision(who, req, d, None, None, None)
    }

    /// correlates a plugin host operation without replacing its resolved arguments (D129).
    pub fn decision_for_call(
        &mut self,
        who: &Principal,
        req: &Request<'_>,
        d: &Decision,
        call: &str,
    ) -> io::Result<()> {
        self.call_decision(who, req, d, Some(call), None, None)
    }

    /// records a bounded byte read with its resolved path and host-assigned call identity (D131).
    pub fn decision_for_read_range(
        &mut self,
        who: &Principal,
        req: &Request<'_>,
        d: &Decision,
        call: &str,
        range: (u64, u32),
    ) -> io::Result<()> {
        self.call_decision(who, req, d, Some(call), Some(range), None)
    }

    /// supplements resolved arguments without allowing details to replace them (D132).
    pub fn decision_for_call_details(
        &mut self,
        who: &Principal,
        req: &Request<'_>,
        d: &Decision,
        call: &str,
        details: Value,
    ) -> io::Result<()> {
        self.call_decision(who, req, d, Some(call), None, Some(details))
    }

    fn call_decision(
        &mut self,
        who: &Principal,
        req: &Request<'_>,
        d: &Decision,
        call: Option<&str>,
        range: Option<(u64, u32)>,
        details: Option<Value>,
    ) -> io::Result<()> {
        let mut args = req.args();
        if let Some(call) = call {
            args["call_id"] = Value::String(call.to_owned());
        }
        if let Some((offset, limit)) = range {
            args["offset"] = offset.into();
            args["limit"] = limit.into();
        }
        if let (Some(args), Some(Value::Object(details))) = (args.as_object_mut(), details) {
            for (key, value) in details {
                args.entry(key).or_insert(value);
            }
        }
        self.write(Record {
            kind: Kind::Decision,
            principal: Some(who.to_string()),
            scope: Some(d.scope),
            args,
            allow: Some(d.allow),
            rule: d.rule.as_ref().map(|r| r.to_string()),
            explanation: d.explanation.clone(),
            ..self.blank()
        })
    }

    /// A host event that is not a decision. `allow` is set for events that act as one, such as a
    /// restore (a denial after the fact).
    pub fn event(
        &mut self,
        kind: Kind,
        who: Option<&Principal>,
        scope: Option<Scope>,
        allow: Option<bool>,
        args: Value,
        explanation: impl Into<String>,
    ) -> io::Result<()> {
        self.write(Record {
            kind,
            principal: who.map(ToString::to_string),
            scope,
            args,
            allow,
            explanation: explanation.into(),
            ..self.blank()
        })
    }

    fn blank(&self) -> Record {
        Record {
            seq: self.seq,
            ts: humantime::format_rfc3339_millis(SystemTime::now()).to_string(),
            session: self.session.clone(),
            kind: Kind::Decision,
            principal: None,
            scope: None,
            args: Value::Null,
            allow: None,
            rule: None,
            explanation: String::new(),
            prev: self.prev.clone(),
        }
    }

    fn write(&mut self, record: Record) -> io::Result<()> {
        let line = serde_json::to_string(&record).map_err(io::Error::other)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.prev = sha256_hex(line.as_bytes());
        self.seq += 1;
        Ok(())
    }

    /// Writes the end record and appends the final hash to the sessions index.
    pub fn close(mut self, sessions_index: &Path) -> io::Result<String> {
        self.event(
            Kind::SessionEnd,
            None,
            None,
            None,
            Value::Null,
            "session closed",
        )?;
        let entry = serde_json::json!({
            "session": self.session,
            "log": self.path,
            "records": self.seq,
            "head": self.prev,
        });
        let mut index = OpenOptions::new()
            .append(true)
            .create(true)
            .open(sessions_index)?;
        writeln!(index, "{entry}")?;
        Ok(self.prev)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Verified {
    pub records: u64,
    pub head: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("line {line}: not a record: {source}")]
    Parse {
        line: u64,
        source: serde_json::Error,
    },
    #[error("line {line}: seq is {found}, expected {expected}")]
    Seq {
        line: u64,
        found: u64,
        expected: u64,
    },
    #[error("line {line}: prev does not match the hash of line {}", line - 1)]
    Chain { line: u64 },
    #[error("line {line}: session `{found}` differs from `{expected}`")]
    Session {
        line: u64,
        found: String,
        expected: String,
    },
}

/// Walks the chain. Detects edited, inserted, reordered and removed lines; truncation of the tail
/// shows up as a head that differs from the one in `sessions.jsonl`.
pub fn verify(path: &Path) -> Result<Verified, VerifyError> {
    let reader = BufReader::new(File::open(path)?);
    let mut prev = GENESIS.to_owned();
    let mut session: Option<String> = None;
    let mut n = 0u64;
    for line in reader.lines() {
        let line = line?;
        let lineno = n + 1;
        let record: Record = serde_json::from_str(&line).map_err(|source| VerifyError::Parse {
            line: lineno,
            source,
        })?;
        if record.seq != n {
            return Err(VerifyError::Seq {
                line: lineno,
                found: record.seq,
                expected: n,
            });
        }
        if record.prev != prev {
            return Err(VerifyError::Chain { line: lineno });
        }
        match &session {
            None => session = Some(record.session.clone()),
            Some(s) if *s != record.session => {
                return Err(VerifyError::Session {
                    line: lineno,
                    found: record.session,
                    expected: s.clone(),
                });
            }
            Some(_) => {}
        }
        prev = sha256_hex(line.as_bytes());
        n += 1;
    }
    Ok(Verified {
        records: n,
        head: prev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use saddle_cap::{Location, RelPath, RootName};
    use serde_json::json;

    #[test]
    fn operation_details_cannot_replace_resolved_paths_or_call_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = AuditLog::create(dir.path(), "details").unwrap();
        let loc = Location::new(RootName::workspace(), RelPath::new("file").unwrap());
        let decision = Decision {
            allow: true,
            scope: Scope::FsRead,
            rule: None,
            explanation: "allowed".into(),
        };
        log.decision_for_call_details(
            &Principal::Agent,
            &Request::FsRead { loc: &loc },
            &decision,
            "host-call",
            json!({"path":"spoofed","call_id":"spoofed","operation":"read-directory"}),
        )
        .unwrap();
        let record: Record =
            serde_json::from_str(fs::read_to_string(log.path()).unwrap().trim()).unwrap();
        assert_eq!(record.args["path"], "workspace:file");
        assert_eq!(record.args["call_id"], "host-call");
        assert_eq!(record.args["operation"], "read-directory");
        verify(log.path()).unwrap();
    }

    fn sample(dir: &Path) -> (PathBuf, String) {
        let mut log = AuditLog::create(dir, "s1").unwrap();
        let loc = Location::new(RootName::workspace(), RelPath::new("a.txt").unwrap());
        let req = Request::FsRead { loc: &loc };
        let d = Decision {
            allow: true,
            scope: Scope::FsRead,
            rule: None,
            explanation: "ok".into(),
        };
        log.event(
            Kind::SessionStart,
            None,
            None,
            None,
            serde_json::json!({"roots": {}}),
            "start",
        )
        .unwrap();
        log.decision(&Principal::Agent, &req, &d).unwrap();
        log.decision(&Principal::Agent, &req, &d).unwrap();
        let path = log.path().to_path_buf();
        let head = log.close(&dir.join("sessions.jsonl")).unwrap();
        (path, head)
    }

    #[test]
    fn chain_verifies_and_matches_index() {
        let dir = tempfile::tempdir().unwrap();
        let (path, head) = sample(dir.path());
        let v = verify(&path).unwrap();
        assert_eq!(v.records, 4);
        assert_eq!(v.head, head);
        let index = fs::read_to_string(dir.path().join("sessions.jsonl")).unwrap();
        assert!(index.contains(&head));
    }

    #[test]
    fn edits_and_deletions_break_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = sample(dir.path());
        let text = fs::read_to_string(&path).unwrap();

        fs::write(&path, text.replacen("\"allow\":true", "\"allow\":false", 1)).unwrap();
        assert!(matches!(verify(&path), Err(VerifyError::Chain { line: 3 })));

        let mut lines: Vec<&str> = text.lines().collect();
        lines.remove(1);
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert!(matches!(
            verify(&path),
            Err(VerifyError::Seq { line: 2, .. })
        ));
    }
}
