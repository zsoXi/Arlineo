//! `arlineo diff` (`SPEC.md`, "diff").

use std::collections::BTreeSet;

use crate::config::Config;
use crate::conflicts::{
    Collision, Party, change_kind_name, collect_pair, collision_json, collision_line, is_sql,
    sort_collisions, touch_kind_name,
};
use crate::error::{Error, Result};
use crate::git::{Change, ChangeKind, Git};
use crate::index::{self, BaseIndex, LineRef};
use crate::model::{CommitId, Fact, FactKind, RelPath};
use crate::output::{Report, ReportStatus};
use crate::overlay::{TouchKind, Touched};
use crate::store::Store;

/// Per-branch results, plus the line's own changes since this branch's merge-base.
struct BranchInfo {
    name: String,
    commit: String,
    merge_base: String,
    party: Party,
    line_party: Party,
}

/// For each branch: merge-base with the line, changed files, and definitions added, removed
/// or changed; then collisions between the given branches and with the line's changes since
/// each merge-base.
pub fn run(
    git: &Git,
    store: &Store,
    config: &Config,
    line: &LineRef,
    branches: &[String],
) -> Result<Report> {
    let line_index = if branches.is_empty() {
        None
    } else {
        Some(index::ensure_index(git, store, config, &line.commit)?)
    };
    let mut infos: Vec<BranchInfo> = Vec::new();
    for branch in branches {
        let commit = git.rev_parse(branch)?;
        let merge_base = match git.merge_base(&commit, &line.commit)? {
            Some(base) => base,
            None => {
                return Err(Error::NotFound {
                    what: format!("merge-base of branch {branch} and line {}", line.name),
                });
            }
        };
        let base_index = index::ensure_index(git, store, config, &merge_base)?;
        let branch_index = index::ensure_index(git, store, config, &commit)?;
        let line_ref = match &line_index {
            Some(index) => index,
            None => {
                return Err(Error::Internal {
                    message: "line index missing for a non-empty branch list".to_owned(),
                });
            }
        };
        infos.push(branch_info(
            git,
            branch,
            &commit,
            &merge_base,
            &base_index,
            &branch_index,
            line,
            line_ref,
        )?);
    }
    let mut collisions: Vec<Collision> = Vec::new();
    for (index, first) in infos.iter().enumerate() {
        for second in infos.iter().skip(index.saturating_add(1)) {
            collect_pair(&mut collisions, &first.party, &second.party);
        }
    }
    for info in &infos {
        collect_pair(&mut collisions, &info.party, &info.line_party);
    }
    sort_collisions(&mut collisions);
    Ok(Report {
        command: "diff".to_owned(),
        lines: report_lines(line, &infos, &collisions),
        json: report_json(line, &infos, &collisions),
        status: ReportStatus::Ok,
    })
}

/// Builds the per-branch section data for one branch.
#[expect(
    clippy::too_many_arguments,
    reason = "independent inputs of one step, used once"
)]
fn branch_info(
    git: &Git,
    branch: &str,
    commit: &CommitId,
    merge_base: &CommitId,
    base_index: &BaseIndex,
    branch_index: &BaseIndex,
    line: &LineRef,
    line_index: &BaseIndex,
) -> Result<BranchInfo> {
    let changed = git.diff_names(merge_base, commit)?.items;
    let party = make_party(branch.to_owned(), changed, base_index, branch_index);
    let line_changed = git.diff_names(merge_base, &line.commit)?.items;
    let line_party = make_party(line.name.clone(), line_changed, base_index, line_index);
    Ok(BranchInfo {
        name: branch.to_owned(),
        commit: commit.as_str().to_owned(),
        merge_base: merge_base.as_str().to_owned(),
        party,
        line_party,
    })
}

/// Builds a party from changed files and the indexes before and after them.
fn make_party(name: String, changed: Vec<Change>, before: &BaseIndex, after: &BaseIndex) -> Party {
    let before_facts = restricted_facts(&before.facts, &changed);
    let after_facts = restricted_facts(&after.facts, &changed);
    let touched = touched_between(&before_facts, &after_facts);
    let mut added_sql: BTreeSet<RelPath> = BTreeSet::new();
    for change in &changed {
        if change.kind == ChangeKind::Added && is_sql(&change.path) {
            added_sql.insert(change.path.clone());
        }
    }
    for item in &touched {
        if item.kind == FactKind::File && item.change == TouchKind::Added && is_sql(&item.file) {
            added_sql.insert(item.file.clone());
        }
    }
    Party {
        name,
        touched,
        changed,
        added_sql: added_sql.into_iter().collect(),
    }
}

/// Facts whose file is one of the changed paths, in facts order.
fn restricted_facts(facts: &[Fact], changed: &[Change]) -> Vec<Fact> {
    let paths: BTreeSet<&str> = changed.iter().map(|change| change.path.as_str()).collect();
    facts
        .iter()
        .filter(|fact| paths.contains(fact.file.as_str()))
        .cloned()
        .collect()
}

/// Definitions added, removed or changed between two fact slices that are already
/// restricted to the changed files, compared like `overlay::touched` (`SPEC.md` 17.4).
/// Added `.sql` files also count as an added `file` definition.
fn touched_between(before: &[Fact], after: &[Fact]) -> Vec<Touched> {
    let mut touched = crate::overlay::touched_definitions(before, after);
    push_added_sql_files(before, after, &mut touched);
    crate::overlay::sort_touched(&mut touched);
    touched
}

/// Adds an added `file` definition for every `.sql` file present only in `after`.
fn push_added_sql_files(before: &[Fact], after: &[Fact], touched: &mut Vec<Touched>) {
    let before_files: BTreeSet<&str> = before
        .iter()
        .filter(|fact| fact.kind == FactKind::File)
        .map(|fact| fact.file.as_str())
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for fact in after {
        if fact.kind != FactKind::File || !is_sql(&fact.file) {
            continue;
        }
        if before_files.contains(fact.file.as_str()) || !seen.insert(fact.file.as_str()) {
            continue;
        }
        touched.push(Touched {
            kind: FactKind::File,
            key: fact.key.clone(),
            change: TouchKind::Added,
            file: fact.file.clone(),
            line: fact.line,
        });
    }
}

/// Text lines: one section per branch, then the collisions.
fn report_lines(line: &LineRef, infos: &[BranchInfo], collisions: &[Collision]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for info in infos {
        lines.push(format!(
            "branch {}: {} changed files, {} touched definitions (merge-base {}, commit {})",
            info.name,
            info.party.changed.len(),
            info.party.touched.len(),
            info.merge_base,
            info.commit
        ));
        for item in &info.party.touched {
            lines.push(touched_line(item));
        }
        lines.push(format!(
            "line {} since merge-base of {}: {} changed files, {} touched definitions",
            line.name,
            info.name,
            info.line_party.changed.len(),
            info.line_party.touched.len()
        ));
        for item in &info.line_party.touched {
            lines.push(touched_line(item));
        }
    }
    if collisions.is_empty() {
        lines.push(format!(
            "no collisions between {} branches and the line",
            infos.len()
        ));
    } else {
        for collision in collisions {
            lines.push(collision_line(collision));
        }
    }
    lines
}

/// The whole JSON value of the report.
fn report_json(
    line: &LineRef,
    infos: &[BranchInfo],
    collisions: &[Collision],
) -> serde_json::Value {
    let branches: Vec<serde_json::Value> = infos
        .iter()
        .map(|info| {
            serde_json::json!({
                "branch": info.name,
                "commit": info.commit,
                "merge_base": info.merge_base,
                "changed_files": info.party.changed.len(),
                "changed": info.party.changed.iter().map(changed_json).collect::<Vec<_>>(),
                "touched": info.party.touched.iter().map(touched_json).collect::<Vec<_>>(),
                "line": {
                    "name": line.name,
                    "commit": line.commit.as_str(),
                    "changed_files": info.line_party.changed.len(),
                    "changed": info.line_party.changed.iter().map(changed_json).collect::<Vec<_>>(),
                    "touched": info.line_party.touched.iter().map(touched_json).collect::<Vec<_>>(),
                },
            })
        })
        .collect();
    let items: Vec<serde_json::Value> = collisions
        .iter()
        .map(|collision| collision_json(collision, "parties", "name"))
        .collect();
    serde_json::json!({
        "branches": branches,
        "collisions": items,
    })
}

/// One touched definition as a text line.
fn touched_line(item: &Touched) -> String {
    format!(
        "  {} {} {} at {}:{}",
        touch_kind_name(item.change),
        item.kind.as_str(),
        item.key,
        item.file,
        item.line
    )
}

/// One touched definition as JSON.
fn touched_json(item: &Touched) -> serde_json::Value {
    serde_json::json!({
        "kind": item.kind.as_str(),
        "key": item.key,
        "file": item.file.as_str(),
        "line": item.line,
        "change": touch_kind_name(item.change),
    })
}

/// One changed file as JSON.
fn changed_json(change: &Change) -> serde_json::Value {
    serde_json::json!({
        "kind": change_kind_name(change.kind),
        "file": change.path.as_str(),
        "line": 0,
    })
}
