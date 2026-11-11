//! Command output: the freshness stamp and reports in text or JSON (`SPEC.md`, "Output").

use std::error::Error as _;

use crate::error::{Error, Result};
use crate::model::CommitId;

/// Outcome of a report, for the exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportStatus {
    /// Exit code 0.
    Ok,
    /// `check` found new violations: exit code 3.
    Findings,
}

/// What a command produced. Every command fills both the text lines and the JSON value.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Command name, e.g. `fn` or `check`.
    pub command: String,
    /// Text lines, without the stamp.
    pub lines: Vec<String>,
    /// JSON value for `--json`, placed under `"result"`.
    pub json: serde_json::Value,
    pub status: ReportStatus,
}

/// Where the answer comes from and how fresh it is. First line of every text output and
/// `"stamp"` of every JSON output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub line_name: String,
    pub line_commit: CommitId,
    /// Unix seconds when the base index was written.
    pub indexed_unix: u64,
    /// Unix seconds now.
    pub now_unix: u64,
    pub worktree: Option<WorktreeStamp>,
}

/// Worktree part of the stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeStamp {
    /// Branch name, or the worktree directory's last path segment when detached.
    pub name: String,
    pub merge_base: CommitId,
    pub changed_files: usize,
}

/// Age of an index in the largest whole unit, rounded down.
fn age_text(now_unix: u64, indexed_unix: u64) -> String {
    let age = now_unix.saturating_sub(indexed_unix);
    if age < 60 {
        format!("{age}s")
    } else if age < 3_600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3_600)
    } else {
        format!("{}d", age / 86_400)
    }
}

/// Text output: stamp line, then the report lines, each ending with `\n`.
pub fn render_text(stamp: &Stamp, report: &Report) -> String {
    let mut out = format!(
        "arlineo: line {}@{} indexed {} ago",
        stamp.line_name,
        stamp.line_commit.short(),
        age_text(stamp.now_unix, stamp.indexed_unix)
    );
    if let Some(worktree) = &stamp.worktree {
        let changed = if worktree.changed_files == 1 {
            String::from("1 changed file")
        } else {
            format!("{} changed files", worktree.changed_files)
        };
        out.push_str(&format!(
            " | worktree {} (merge-base {}, {})",
            worktree.name,
            worktree.merge_base.short(),
            changed
        ));
    }
    out.push('\n');
    for line in &report.lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// JSON output: `{"stamp": {...}, "command": "...", "result": ...}` and a final `\n`.
pub fn render_json(stamp: &Stamp, report: &Report) -> Result<String> {
    let mut stamp_json = serde_json::Map::new();
    stamp_json.insert(
        String::from("line"),
        serde_json::Value::String(stamp.line_name.clone()),
    );
    stamp_json.insert(
        String::from("commit"),
        serde_json::Value::String(stamp.line_commit.as_str().to_owned()),
    );
    stamp_json.insert(
        String::from("indexed_unix"),
        serde_json::Value::Number(serde_json::Number::from(stamp.indexed_unix)),
    );
    stamp_json.insert(
        String::from("now_unix"),
        serde_json::Value::Number(serde_json::Number::from(stamp.now_unix)),
    );
    let worktree_json = match &stamp.worktree {
        Some(worktree) => {
            let mut worktree_map = serde_json::Map::new();
            worktree_map.insert(
                String::from("name"),
                serde_json::Value::String(worktree.name.clone()),
            );
            worktree_map.insert(
                String::from("merge_base"),
                serde_json::Value::String(worktree.merge_base.as_str().to_owned()),
            );
            // usize is at most 64 bits on the targets Rust supports here, so this
            // conversion to u64 cannot lose a count of changed files.
            let changed_files = worktree.changed_files as u64;
            worktree_map.insert(
                String::from("changed_files"),
                serde_json::Value::Number(serde_json::Number::from(changed_files)),
            );
            serde_json::Value::Object(worktree_map)
        }
        None => serde_json::Value::Null,
    };
    stamp_json.insert(String::from("worktree"), worktree_json);
    let mut root = serde_json::Map::new();
    root.insert(String::from("stamp"), serde_json::Value::Object(stamp_json));
    root.insert(
        String::from("command"),
        serde_json::Value::String(report.command.clone()),
    );
    root.insert(String::from("result"), report.json.clone());
    let mut out = serde_json::to_string(&serde_json::Value::Object(root))
        .map_err(|source| Error::json("serialize JSON output", source))?;
    out.push('\n');
    Ok(out)
}

/// JSON kind name of an error.
fn error_kind(error: &Error) -> &'static str {
    match error {
        Error::Usage { .. } => "usage",
        Error::Git { .. } => "git",
        Error::Io { .. } => "io",
        Error::Json { .. } => "json",
        Error::Config { .. } => "config",
        Error::Format { .. } => "format",
        Error::InvalidPath { .. } => "invalid_path",
        Error::InvalidCommitId { .. } => "invalid_commit_id",
        Error::NotFound { .. } => "not_found",
        Error::Locked { .. } => "locked",
        Error::Internal { .. } => "internal",
    }
}

/// Text and JSON for an error, without a stamp (`SPEC.md`, "Errors").
pub fn render_error(error: &crate::error::Error, json: bool) -> String {
    if json {
        let mut error_map = serde_json::Map::new();
        error_map.insert(
            String::from("kind"),
            serde_json::Value::String(String::from(error_kind(error))),
        );
        error_map.insert(
            String::from("message"),
            serde_json::Value::String(error.to_string()),
        );
        let mut root = serde_json::Map::new();
        root.insert(String::from("error"), serde_json::Value::Object(error_map));
        return match serde_json::to_string(&serde_json::Value::Object(root)) {
            Ok(text) => {
                let mut out = text;
                out.push('\n');
                out
            }
            Err(_) => String::from(
                "{\"error\":{\"kind\":\"internal\",\"message\":\"failed to serialize error\"}}\n",
            ),
        };
    }
    let mut out = format!("arlineo: error: {error}\n");
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(&format!("  caused by: {cause}\n"));
        source = cause.source();
    }
    out
}
