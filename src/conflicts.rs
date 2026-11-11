//! `arlineo conflicts` (`SPEC.md`, "conflicts").

use std::collections::{BTreeMap, BTreeSet};

use crate::error::Result;
use crate::git::{Change, ChangeKind};
use crate::model::{FactKind, RelPath};
use crate::output::{Report, ReportStatus};
use crate::overlay::{Overlay, TouchKind, Touched};

/// One side of a comparison: a worktree (or, in `diff`, a branch or the line) with its
/// touched definitions and changed files.
pub(crate) struct Party {
    pub(crate) name: String,
    pub(crate) touched: Vec<Touched>,
    pub(crate) changed: Vec<Change>,
    pub(crate) added_sql: Vec<RelPath>,
}

/// One side of a collision.
#[derive(Clone, Debug)]
struct Side {
    name: String,
    change: String,
    file: RelPath,
    line: u32,
}

/// One collision between two parties.
#[derive(Clone, Debug)]
pub(crate) struct Collision {
    kind: &'static str,
    fact_kind: Option<FactKind>,
    key: String,
    first: Side,
    second: Side,
}

/// Pairs of active worktrees that touch the same definition (`overlay::touched`), add
/// migrations with the same file name, or change the same file; each collision once, sorted.
pub fn run(overlays: &[Overlay]) -> Result<Report> {
    let parties: Vec<Party> = overlays.iter().map(party).collect();
    let mut collisions: Vec<Collision> = Vec::new();
    for (index, first) in parties.iter().enumerate() {
        for second in parties.iter().skip(index.saturating_add(1)) {
            collect_pair(&mut collisions, first, second);
        }
    }
    sort_collisions(&mut collisions);
    let lines = if collisions.is_empty() {
        vec![format!(
            "no collisions between {} active worktrees",
            overlays.len()
        )]
    } else {
        collisions.iter().map(collision_line).collect()
    };
    let items: Vec<serde_json::Value> = collisions
        .iter()
        .map(|collision| collision_json(collision, "worktrees", "worktree"))
        .collect();
    Ok(Report {
        command: "conflicts".to_owned(),
        lines,
        json: serde_json::json!({
            "worktrees": overlays.len(),
            "collisions": items,
        }),
        status: ReportStatus::Ok,
    })
}

/// Builds the comparable form of one overlay.
fn party(overlay: &Overlay) -> Party {
    let touched = crate::overlay::touched(overlay);
    let mut added_sql: BTreeSet<RelPath> = BTreeSet::new();
    for change in &overlay.changed {
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
        name: worktree_name(overlay),
        touched,
        changed: overlay.changed.clone(),
        added_sql: added_sql.into_iter().collect(),
    }
}

/// Name of a worktree: its branch, else the last segment of its directory path.
fn worktree_name(overlay: &Overlay) -> String {
    if let Some(branch) = &overlay.worktree.branch {
        if !branch.is_empty() {
            return branch.clone();
        }
    }
    let path = &overlay.worktree.path;
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    path.display().to_string()
}

/// Adds every collision between two parties: definitions, migration names and files.
pub(crate) fn collect_pair(collisions: &mut Vec<Collision>, first: &Party, second: &Party) {
    collect_definitions(collisions, first, second);
    collect_files(collisions, first, second);
    collect_migrations(collisions, first, second);
}

/// Same touched `(kind, key)` on both sides.
fn collect_definitions(collisions: &mut Vec<Collision>, first: &Party, second: &Party) {
    let first_map = touched_map(&first.touched);
    let second_map = touched_map(&second.touched);
    for (id, first_item) in &first_map {
        if let Some(second_item) = second_map.get(id) {
            collisions.push(Collision {
                kind: "definition",
                fact_kind: Some(first_item.kind),
                key: first_item.key.clone(),
                first: Side {
                    name: first.name.clone(),
                    change: touch_kind_name(first_item.change).to_owned(),
                    file: first_item.file.clone(),
                    line: first_item.line,
                },
                second: Side {
                    name: second.name.clone(),
                    change: touch_kind_name(second_item.change).to_owned(),
                    file: second_item.file.clone(),
                    line: second_item.line,
                },
            });
        }
    }
}

/// Same changed path on both sides.
fn collect_files(collisions: &mut Vec<Collision>, first: &Party, second: &Party) {
    let first_map = changed_map(&first.changed);
    let second_map = changed_map(&second.changed);
    for (path, first_change) in &first_map {
        if let Some(second_change) = second_map.get(path) {
            collisions.push(Collision {
                kind: "file",
                fact_kind: None,
                key: (*path).as_str().to_owned(),
                first: Side {
                    name: first.name.clone(),
                    change: change_kind_name(first_change.kind).to_owned(),
                    file: (*path).clone(),
                    line: 0,
                },
                second: Side {
                    name: second.name.clone(),
                    change: change_kind_name(second_change.kind).to_owned(),
                    file: (*path).clone(),
                    line: 0,
                },
            });
        }
    }
}

/// Added `.sql` files on both sides with the same migration id, where both paths are under
/// a `migrations` directory or neither path is. Under `migrations` the id is the version
/// number the file name starts with (`0004` for `0004_add_orders.sql`), since two
/// migrations with one number collide when both are applied; elsewhere it is the file name.
fn collect_migrations(collisions: &mut Vec<Collision>, first: &Party, second: &Party) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for first_path in &first.added_sql {
        for second_path in &second.added_sql {
            if has_migrations_dir(first_path) != has_migrations_dir(second_path) {
                continue;
            }
            let name = migration_id(first_path);
            if name != migration_id(second_path) {
                continue;
            }
            let name = name.to_owned();
            if !seen.insert(name.clone()) {
                continue;
            }
            collisions.push(Collision {
                kind: "migration_name",
                fact_kind: None,
                key: name,
                first: Side {
                    name: first.name.clone(),
                    change: "added".to_owned(),
                    file: first_path.clone(),
                    line: 0,
                },
                second: Side {
                    name: second.name.clone(),
                    change: "added".to_owned(),
                    file: second_path.clone(),
                    line: 0,
                },
            });
        }
    }
}

/// First touched definition per `(kind, key)`, in deterministic key order.
fn touched_map(touched: &[Touched]) -> BTreeMap<(FactKind, String), &Touched> {
    let mut map: BTreeMap<(FactKind, String), &Touched> = BTreeMap::new();
    for item in touched {
        map.entry((item.kind, item.key.clone())).or_insert(item);
    }
    map
}

/// First change per path, in deterministic path order.
fn changed_map(changed: &[Change]) -> BTreeMap<&RelPath, &Change> {
    let mut map: BTreeMap<&RelPath, &Change> = BTreeMap::new();
    for change in changed {
        map.entry(&change.path).or_insert(change);
    }
    map
}

/// Sorts by collision kind, key, then worktree names, with file details as tie-breakers.
pub(crate) fn sort_collisions(collisions: &mut [Collision]) {
    collisions.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.first.name.cmp(&b.first.name))
            .then_with(|| a.second.name.cmp(&b.second.name))
            .then_with(|| a.fact_kind.cmp(&b.fact_kind))
            .then_with(|| a.first.file.cmp(&b.first.file))
            .then_with(|| a.first.line.cmp(&b.first.line))
            .then_with(|| a.second.file.cmp(&b.second.file))
            .then_with(|| a.second.line.cmp(&b.second.line))
    });
}

/// One text line for a collision.
pub(crate) fn collision_line(collision: &Collision) -> String {
    let subject = match collision.fact_kind {
        Some(kind) => format!("{} {} ({})", collision.kind, collision.key, kind.as_str()),
        None => format!("{} {}", collision.kind, collision.key),
    };
    format!(
        "{}: {} {} {}:{}, {} {} {}:{}",
        subject,
        collision.first.name,
        collision.first.change,
        collision.first.file,
        collision.first.line,
        collision.second.name,
        collision.second.change,
        collision.second.file,
        collision.second.line
    )
}

/// One JSON item for a collision. `names_key` holds both party names (`worktrees` in
/// `conflicts`, `parties` in `diff`), `name_key` the name in each side.
pub(crate) fn collision_json(
    collision: &Collision,
    names_key: &str,
    name_key: &str,
) -> serde_json::Value {
    let mut item = serde_json::json!({
        "kind": collision.kind,
        "fact_kind": collision.fact_kind.map(|kind| kind.as_str()),
        "key": collision.key,
        "first": side_json(&collision.first, name_key),
        "second": side_json(&collision.second, name_key),
    });
    if let Some(map) = item.as_object_mut() {
        map.insert(
            names_key.to_owned(),
            serde_json::json!([&collision.first.name, &collision.second.name]),
        );
    }
    item
}

/// One side of a collision as JSON.
fn side_json(side: &Side, name_key: &str) -> serde_json::Value {
    let mut item = serde_json::json!({
        "change": side.change,
        "file": side.file.as_str(),
        "line": side.line,
    });
    if let Some(map) = item.as_object_mut() {
        map.insert(
            name_key.to_owned(),
            serde_json::Value::String(side.name.clone()),
        );
    }
    item
}

/// Whether the path names a `.sql` file.
pub(crate) fn is_sql(path: &RelPath) -> bool {
    path.extension() == Some("sql")
}

/// Migration id of an added `.sql` file (see `collect_migrations`).
fn migration_id(path: &RelPath) -> &str {
    let name = path.file_name();
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || !has_migrations_dir(path) {
        return name;
    }
    name.get(..digits).unwrap_or(name)
}

/// Whether any directory segment of the path is named `migrations`.
fn has_migrations_dir(path: &RelPath) -> bool {
    match path.parent() {
        Some(parent) => parent.split('/').any(|segment| segment == "migrations"),
        None => false,
    }
}

/// Serialized name of a touch kind.
pub(crate) fn touch_kind_name(kind: TouchKind) -> &'static str {
    match kind {
        TouchKind::Added => "added",
        TouchKind::Removed => "removed",
        TouchKind::Changed => "changed",
    }
}

/// Serialized name of a change kind.
pub(crate) fn change_kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

#[cfg(test)]
mod tests {
    use super::migration_id;
    use crate::model::RelPath;

    fn id(path: &str) -> String {
        migration_id(&RelPath::new(path).expect("test path")).to_owned()
    }

    #[test]
    fn migrations_collide_on_their_number() {
        assert_eq!(id("db/migrations/0004_add_notes.sql"), "0004");
        assert_eq!(id("db/migrations/0004_drop_notes.sql"), "0004");
        assert_eq!(id("db/seed/0004_data.sql"), "0004_data.sql");
        assert_eq!(id("db/migrations/init.sql"), "init.sql");
    }
}
