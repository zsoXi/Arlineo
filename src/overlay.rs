//! Worktree overlays and views (`SPEC.md`, "Overlays and views").
//!
//! An overlay holds the facts of the files a worktree changed since its merge-base with the
//! base line, read from disk at question time and cached by size, modification time and hash.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::extract::rust::crate_name_from_manifest;
use crate::extract::{CrateRoot, ExtractCtx, extract};
use crate::git::{Change, ChangeKind, Git, TreeEntry, WorktreeInfo};
use crate::graph::build::build;
use crate::graph::file::{read_graph, write_graph};
use crate::index::{BaseIndex, LineRef, crate_roots, facts_from_blobs};
use crate::model::{
    CommitId, Confidence, Fact, FactIndex, FactKind, Graph, RelPath, UnparsedData, WorktreeId,
    sort_facts,
};
use crate::source::{Loaded, MAX_FILE_BYTES, load_from_disk};
use crate::store::{Store, write_atomic};
use crate::util::fnv::Fnv64;
use crate::util::jsonl;

/// Changes of one worktree relative to its merge-base with the line.
#[derive(Clone, Debug)]
pub struct Overlay {
    pub worktree: WorktreeInfo,
    pub merge_base: CommitId,
    /// Files changed since the merge-base: committed on the worktree's branch, staged,
    /// unstaged and untracked (not ignored), combined per path.
    pub changed: Vec<Change>,
    /// Facts of the changed files as they are on disk now (deleted files have none).
    pub facts: Vec<Fact>,
    /// Facts of the same files at the merge-base.
    pub before: Vec<Fact>,
}

/// Facts with their index and graph, ready for queries.
#[derive(Clone, Debug)]
pub struct View {
    pub facts: Vec<Fact>,
    pub fact_index: FactIndex,
    pub graph: Graph,
}

/// How a definition changed in a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TouchKind {
    Added,
    Removed,
    Changed,
}

/// A definition a worktree added, removed or changed (`SPEC.md`, "Touched definitions").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Touched {
    pub kind: FactKind,
    pub key: String,
    pub change: TouchKind,
    pub file: RelPath,
    pub line: u32,
}

/// Cached size, modification time and hash of one changed path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedPath {
    size: u64,
    mtime_ns: u128,
    hash: String,
}

/// Contents of the overlay `cache.json` file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayCache {
    merge_base: CommitId,
    config_hash: String,
    changed: Vec<RelPath>,
    paths: BTreeMap<String, CachedPath>,
}

/// A cache that read cleanly: its metadata plus both facts files.
struct CachedOverlay {
    cache: OverlayCache,
    facts: Vec<Fact>,
    before: Vec<Fact>,
}

/// Finds a worktree by absolute or relative path, directory name or branch name.
/// `Error::NotFound` when none matches, `Error::Usage` when several match.
pub fn find_worktree(git: &Git, spec: &str) -> Result<WorktreeInfo> {
    let worktrees = active_worktrees(git)?;
    // A spec that is not an existing path can still match by name or branch.
    let spec_path = std::fs::canonicalize(spec).ok();
    let mut matches: Vec<WorktreeInfo> = Vec::new();
    for worktree in worktrees {
        let by_branch = worktree.branch.as_deref() == Some(spec);
        let by_name = worktree.path.file_name().and_then(|name| name.to_str()) == Some(spec);
        let by_path = match &spec_path {
            Some(canonical) => {
                canonical == &worktree.path
                    || match std::fs::canonicalize(&worktree.path) {
                        Ok(worktree_canonical) => &worktree_canonical == canonical,
                        Err(_) => false,
                    }
            }
            None => false,
        };
        if by_branch || by_name || by_path {
            matches.push(worktree);
        }
    }
    match matches.len() {
        0 => Err(Error::NotFound {
            what: format!("worktree {spec}"),
        }),
        1 => match matches.into_iter().next() {
            Some(worktree) => Ok(worktree),
            None => Err(Error::Internal {
                message: "worktree match disappeared".to_owned(),
            }),
        },
        _ => {
            let paths = matches
                .iter()
                .map(|worktree| worktree.path.display().to_string())
                .collect::<Vec<String>>()
                .join(", ");
            Err(Error::Usage {
                message: format!("worktree {spec} matches several worktrees: {paths}"),
            })
        }
    }
}

/// Worktrees to compare: not bare, not prunable, with a HEAD.
pub fn active_worktrees(git: &Git) -> Result<Vec<WorktreeInfo>> {
    let mut active = Vec::new();
    for worktree in git.worktrees()? {
        if !worktree.is_bare && !worktree.prunable && worktree.head.is_some() {
            active.push(worktree);
        }
    }
    Ok(active)
}

/// Builds the overlay of a worktree against the line.
pub fn overlay_for(
    git: &Git,
    store: &Store,
    config: &Config,
    line: &LineRef,
    worktree: &WorktreeInfo,
) -> Result<Overlay> {
    let _ = git;
    let worktree_git = Git::new(&worktree.path);
    let head = match worktree.head.clone() {
        Some(head) => head,
        None => {
            return Err(Error::NotFound {
                what: format!("HEAD of worktree {}", worktree.path.display()),
            });
        }
    };
    let merge_base = match worktree_git.merge_base(&head, &line.commit)? {
        Some(merge_base) => merge_base,
        None => {
            return Err(Error::NotFound {
                what: format!(
                    "merge-base of {} and {}",
                    worktree.path.display(),
                    line.name
                ),
            });
        }
    };
    let id = WorktreeId::from_path(&worktree.path);

    let diff = worktree_git.diff_names(&merge_base, &head)?;
    let status = worktree_git.status()?;
    let mut combined: BTreeMap<RelPath, ChangeKind> = BTreeMap::new();
    for change in diff.items {
        combined.insert(change.path, change.kind);
    }
    for change in status.items {
        combined.insert(change.path, change.kind);
    }

    let tree = worktree_git.ls_tree(&merge_base)?;
    let base_paths: BTreeSet<RelPath> = tree.items.iter().map(|entry| entry.path.clone()).collect();
    let mut changed_map: BTreeMap<RelPath, ChangeKind> = BTreeMap::new();
    for (path, kind) in combined {
        let exists = std::fs::symlink_metadata(worktree.path.join(path.as_str())).is_ok();
        let kind = if !exists {
            ChangeKind::Deleted
        } else if base_paths.contains(&path) {
            ChangeKind::Modified
        } else {
            kind
        };
        changed_map.insert(path, kind);
    }
    let changed: Vec<Change> = changed_map
        .iter()
        .map(|(path, kind)| Change {
            path: path.clone(),
            kind: *kind,
        })
        .collect();
    let changed_paths: Vec<RelPath> = changed_map.keys().cloned().collect();

    let overlay_dir = store.overlay_dir(&id);
    let facts_path = overlay_dir.join("facts.jsonl");
    let before_path = overlay_dir.join("before.jsonl");
    let cache_path = overlay_dir.join("cache.json");
    let cached = load_cache(&cache_path, &facts_path, &before_path);
    let cache_valid = match &cached {
        Some(cached) => {
            cached.cache.merge_base == merge_base && cached.cache.config_hash == config.hash
        }
        None => false,
    };
    let before_reused = cache_valid
        && match &cached {
            Some(cached) => cached.cache.changed == changed_paths,
            None => false,
        };
    let mut cached_paths: BTreeMap<String, CachedPath> = BTreeMap::new();
    let mut cached_by_file: BTreeMap<RelPath, Vec<Fact>> = BTreeMap::new();
    let mut cached_before: Option<Vec<Fact>> = None;
    if cache_valid {
        if let Some(cached_overlay) = cached {
            cached_paths = cached_overlay.cache.paths;
            for fact in cached_overlay.facts {
                cached_by_file
                    .entry(fact.file.clone())
                    .or_default()
                    .push(fact);
            }
            if before_reused {
                cached_before = Some(cached_overlay.before);
            }
        }
    }

    // Crate roots need every `Cargo.toml` of the merge-base; they are read only when some file
    // is extracted again, not when everything comes from the cache.
    let mut base_crates: Option<Vec<CrateRoot>> = None;
    let mut before = match cached_before {
        Some(facts) => facts,
        None => {
            let mut cat_file = worktree_git.cat_file()?;
            let crates = crate_roots(&mut cat_file, &tree.items)?;
            let entries: Vec<TreeEntry> = tree
                .items
                .iter()
                .filter(|entry| changed_map.contains_key(&entry.path))
                .cloned()
                .collect();
            let facts = facts_from_blobs(&mut cat_file, &entries, config, &crates)?;
            base_crates = Some(crates);
            facts
        }
    };
    let mut disk_crates: Option<Vec<CrateRoot>> = None;

    let mut facts: Vec<Fact> = Vec::new();
    let mut new_paths: BTreeMap<String, CachedPath> = BTreeMap::new();
    let mut all_reused = true;
    for change in &changed {
        if change.kind == ChangeKind::Deleted {
            continue;
        }
        if !config.wants(&change.path) {
            continue;
        }
        let stamp_before = file_stamp(&worktree.path, &change.path);
        if let (Some(stamp), Some(entry)) = (&stamp_before, cached_paths.get(change.path.as_str()))
        {
            if entry.size == stamp.size && entry.mtime_ns == stamp.mtime_ns && !is_recent(stamp) {
                if let Some(cached_facts) = cached_by_file.get(&change.path) {
                    facts.extend(cached_facts.iter().cloned());
                }
                new_paths.insert(change.path.as_str().to_owned(), entry.clone());
                continue;
            }
        }
        all_reused = false;
        let loaded = load_from_disk(&worktree.path, &change.path)?;
        // Cache a path only when its size and time did not change while it was read, so a file
        // edited during the read is read again next time instead of keeping stale facts.
        let stamp_after = file_stamp(&worktree.path, &change.path);
        let steady = match (&stamp_before, &stamp_after) {
            (Some(first), Some(second)) => {
                first.size == second.size && first.mtime_ns == second.mtime_ns
            }
            _ => false,
        };
        match loaded {
            Some(Loaded::Text(source_file)) => {
                if disk_crates.is_none() {
                    let base = match base_crates.take() {
                        Some(crates) => crates,
                        None => crate_roots(&mut worktree_git.cat_file()?, &tree.items)?,
                    };
                    disk_crates = Some(crates_for_disk(&base, worktree, &changed));
                }
                let crates = disk_crates.as_deref().unwrap_or_default();
                let extracted = extract(&source_file, &ExtractCtx { config, crates })?;
                facts.extend(extracted);
                if let (true, Some(mut stamp)) = (steady, stamp_after) {
                    stamp.hash = source_file.hash.clone();
                    new_paths.insert(change.path.as_str().to_owned(), stamp);
                }
            }
            Some(Loaded::Skipped { path, reason }) => {
                facts.push(Fact::new(
                    &path,
                    0,
                    path.as_str(),
                    Confidence::Exact,
                    &UnparsedData { reason },
                )?);
                if let (true, Some(stamp)) = (steady, stamp_after) {
                    new_paths.insert(change.path.as_str().to_owned(), stamp);
                }
            }
            None => {}
        }
    }

    sort_facts(&mut facts);
    sort_facts(&mut before);
    // Nothing new to remember: skip the lock and the three writes (many agents run commands
    // at the same time, and an unchanged overlay is the common case).
    let unchanged = before_reused && all_reused && new_paths == cached_paths;
    if !unchanged {
        let cache = OverlayCache {
            merge_base: merge_base.clone(),
            config_hash: config.hash.clone(),
            changed: changed_paths,
            paths: new_paths,
        };
        let _lock = store.lock()?;
        jsonl::write_facts(&facts_path, &facts)?;
        jsonl::write_facts(&before_path, &before)?;
        let bytes = serde_json::to_vec(&cache)
            .map_err(|source| Error::json("writing overlay cache", source))?;
        write_atomic(&cache_path, &bytes)?;
    }

    Ok(Overlay {
        worktree: worktree.clone(),
        merge_base,
        changed,
        facts,
        before,
    })
}

/// View of the base index alone.
pub fn base_view(base: BaseIndex) -> View {
    View {
        fact_index: base.fact_index,
        facts: base.facts,
        graph: base.graph,
    }
}

/// View of the line with the worktree's changed files swapped in: base facts of every path in
/// `overlay.changed` removed, overlay facts added, sorted, graph rebuilt.
///
/// The graph is cached in `overlay/<worktree-id>/view-graph.bin` under a key made of the base
/// facts hash, the changed paths and the overlay facts; a missing or stale cache is rebuilt and
/// a failed cache write does not fail the command.
pub fn worktree_view(store: &Store, base: &BaseIndex, overlay: &Overlay) -> Result<View> {
    let changed_paths: BTreeSet<RelPath> = overlay
        .changed
        .iter()
        .map(|change| change.path.clone())
        .collect();
    let mut facts: Vec<Fact> = Vec::new();
    for fact in &base.facts {
        if !changed_paths.contains(&fact.file) {
            facts.push(fact.clone());
        }
    }
    for fact in &overlay.facts {
        facts.push(fact.clone());
    }
    sort_facts(&mut facts);
    let fact_index = FactIndex::new(&facts);

    let mut hasher = Fnv64::new();
    hasher.update(base.facts_hash.as_bytes());
    hasher.update(b"\n");
    for path in &changed_paths {
        hasher.update(path.as_str().as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(&jsonl::facts_bytes(&overlay.facts)?);
    let key = format!("{:016x}", hasher.finish());
    let graph_path = store
        .overlay_dir(&WorktreeId::from_path(&overlay.worktree.path))
        .join("view-graph.bin");
    // An unreadable cache file is treated like a missing one: the graph is rebuilt below.
    let graph = match read_graph(&graph_path, &key).ok().flatten() {
        Some(graph) => graph,
        None => {
            let graph = build(&facts)?;
            // Best effort: the cache only saves time on the next command.
            let _ = write_graph(&graph_path, &graph, &key);
            graph
        }
    };
    Ok(View {
        facts,
        fact_index,
        graph,
    })
}

/// Definitions the worktree added, removed or changed, sorted by file, line, kind, key.
pub fn touched(overlay: &Overlay) -> Vec<Touched> {
    let mut result = touched_definitions(&overlay.before, &overlay.facts);
    for change in &overlay.changed {
        if change.kind != ChangeKind::Added || change.path.extension() != Some("sql") {
            continue;
        }
        let mut line = 0;
        for fact in &overlay.facts {
            if fact.kind == FactKind::File && fact.file == change.path {
                line = fact.line;
                break;
            }
        }
        result.push(Touched {
            kind: FactKind::File,
            key: change.path.as_str().to_owned(),
            change: TouchKind::Added,
            file: change.path.clone(),
            line,
        });
    }
    sort_touched(&mut result);
    result
}

/// Sorts touched definitions by file, line, kind, key, then change.
pub(crate) fn sort_touched(touched: &mut [Touched]) {
    touched.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.kind.cmp(&right.kind))
            .then_with(|| left.key.cmp(&right.key))
            .then(left.change.cmp(&right.change))
    });
}

/// Definitions added, removed or changed from `before` to `after` (`SPEC.md` 17.4): the
/// first fact per `(kind, key)` of the definition kinds on each side, compared with
/// `definition_changed`. Unsorted; `touched` and `diff` add files and sort.
pub(crate) fn touched_definitions(before: &[Fact], after: &[Fact]) -> Vec<Touched> {
    const TOUCHED_KINDS: [FactKind; 8] = [
        FactKind::FnDef,
        FactKind::TypeDef,
        FactKind::SqlFunction,
        FactKind::Table,
        FactKind::Column,
        FactKind::Route,
        FactKind::Trigger,
        FactKind::Grant,
    ];
    let mut before_map: BTreeMap<(FactKind, String), &Fact> = BTreeMap::new();
    for fact in before {
        if TOUCHED_KINDS.contains(&fact.kind) {
            before_map
                .entry((fact.kind, fact.key.clone()))
                .or_insert(fact);
        }
    }
    let mut after_map: BTreeMap<(FactKind, String), &Fact> = BTreeMap::new();
    for fact in after {
        if TOUCHED_KINDS.contains(&fact.kind) {
            after_map
                .entry((fact.kind, fact.key.clone()))
                .or_insert(fact);
        }
    }

    let mut result: Vec<Touched> = Vec::new();
    for ((kind, key), after_fact) in &after_map {
        match before_map.get(&(*kind, key.clone())) {
            Some(before_fact) => {
                if definition_changed(before_fact, after_fact) {
                    result.push(Touched {
                        kind: *kind,
                        key: key.clone(),
                        change: TouchKind::Changed,
                        file: after_fact.file.clone(),
                        line: after_fact.line,
                    });
                }
            }
            None => result.push(Touched {
                kind: *kind,
                key: key.clone(),
                change: TouchKind::Added,
                file: after_fact.file.clone(),
                line: after_fact.line,
            }),
        }
    }
    for ((kind, key), before_fact) in &before_map {
        if !after_map.contains_key(&(*kind, key.clone())) {
            result.push(Touched {
                kind: *kind,
                key: key.clone(),
                change: TouchKind::Removed,
                file: before_fact.file.clone(),
                line: before_fact.line,
            });
        }
    }
    result
}

/// Reads the whole overlay cache; a cache that does not read cleanly is ignored and rebuilt.
fn load_cache(cache_path: &Path, facts_path: &Path, before_path: &Path) -> Option<CachedOverlay> {
    let bytes = std::fs::read(cache_path).ok()?;
    let cache: OverlayCache = serde_json::from_slice(&bytes).ok()?;
    let facts = jsonl::read_facts(facts_path).ok()?;
    let before = jsonl::read_facts(before_path).ok()?;
    Some(CachedOverlay {
        cache,
        facts,
        before,
    })
}

/// Whether a file was modified in the last 2 seconds: a write in the same timestamp tick
/// can keep size and time unchanged, so such a file is always read again.
fn is_recent(stamp: &CachedPath) -> bool {
    match std::time::SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_nanos().saturating_sub(stamp.mtime_ns) < 2_000_000_000,
        Err(_) => true,
    }
}

/// Size and modification time of a file on disk, for the overlay cache.
fn file_stamp(root: &Path, path: &RelPath) -> Option<CachedPath> {
    let metadata = std::fs::symlink_metadata(root.join(path.as_str())).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(CachedPath {
        size: metadata.len(),
        mtime_ns: duration.as_nanos(),
        hash: String::new(),
    })
}

/// Crate roots on disk: the merge-base roots, updated by changed `Cargo.toml` files.
fn crates_for_disk(
    base: &[CrateRoot],
    worktree: &WorktreeInfo,
    changed: &[Change],
) -> Vec<CrateRoot> {
    let mut crates = base.to_vec();
    for change in changed {
        if change.path.file_name() != "Cargo.toml" {
            continue;
        }
        let dir = match change.path.parent() {
            Some(parent) => parent.to_owned(),
            None => String::new(),
        };
        if change.kind == ChangeKind::Deleted {
            crates.retain(|root| root.dir != dir);
            continue;
        }
        let disk_path = worktree.path.join(change.path.as_str());
        let metadata = match std::fs::symlink_metadata(&disk_path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() || metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let bytes = match std::fs::read(&disk_path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => continue,
        };
        match crate_name_from_manifest(&text) {
            Some(name) => {
                let mut replaced = false;
                for root in crates.iter_mut() {
                    if root.dir == dir {
                        root.name = name.clone();
                        replaced = true;
                    }
                }
                if !replaced {
                    crates.push(CrateRoot { dir, name });
                }
            }
            None => crates.retain(|root| root.dir != dir),
        }
    }
    crates
}

/// Whether two facts of a definition kind differ: by their `hash` data field when both
/// have one, otherwise by all of their data except positions (`end_line`), so a definition
/// that only moved is not reported as changed.
fn definition_changed(before: &Fact, after: &Fact) -> bool {
    match (before.data.get("hash"), after.data.get("hash")) {
        (Some(before_hash), Some(after_hash)) => before_hash != after_hash,
        _ => {
            let mut before_data = before.data.clone();
            let mut after_data = after.data.clone();
            before_data.remove("end_line");
            after_data.remove("end_line");
            before_data != after_data
        }
    }
}

/// Removes the overlay directories of worktrees that no longer exist. Best effort: a
/// directory that cannot be removed is left for the next call.
pub fn prune_overlays(store: &Store, active: &[WorktreeInfo]) {
    let keep: BTreeSet<String> = active
        .iter()
        .map(|worktree| WorktreeId::from_path(&worktree.path).as_str().to_owned())
        .collect();
    let overlay_root = store.root().join("overlay");
    let Ok(entries) = std::fs::read_dir(&overlay_root) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Only names this program creates: 16 lowercase hex digits.
        let ours = name.len() == 16
            && name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ours || keep.contains(&name) {
            continue;
        }
        let dir = overlay_root.join(&name);
        if std::fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir()) {
            // Ignored on purpose: see the doc comment.
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
