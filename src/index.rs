//! Base indexes of commits: facts and graph, stored and updated incrementally
//! (`SPEC.md`, "Indexing").

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::extract::rust::crate_name_from_manifest;
use crate::extract::{CrateRoot, ExtractCtx, extract};
use crate::git::{CatFile, ChangeKind, Git, TreeEntry};
use crate::graph::build::build;
use crate::graph::file::{read_graph, write_graph};
use crate::model::{
    CommitId, Confidence, Fact, FactIndex, FactKind, Graph, RelPath, UnparsedData, sort_facts,
};
use crate::source::{Loaded, MAX_FILE_BYTES, load_from_blob};
use crate::store::{IndexedCommit, Meta, STORE_FORMAT_VERSION, Store, now_unix};
use crate::util::fnv;
use crate::util::jsonl::{facts_bytes, read_facts_with_hash, write_facts};

/// The base line: a ref name and the commit it points to now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineRef {
    pub name: String,
    pub commit: CommitId,
}

/// Facts and graph of one commit.
#[derive(Clone, Debug)]
pub struct BaseIndex {
    pub commit: CommitId,
    pub facts: Vec<Fact>,
    pub fact_index: FactIndex,
    pub graph: Graph,
    /// Unix seconds when this index was written.
    pub indexed_unix: u64,
    /// Files skipped by git listing (bad paths) or by loading (binary, too large, ...).
    pub skipped: usize,
    /// FNV-1a 64 hex of the `facts.jsonl` bytes; identifies the facts for graph caches.
    pub facts_hash: String,
}

/// Resolves the base line: the `--base` flag, else `base` from the configuration, else
/// `main`, else `master`, else `HEAD` of the main worktree.
pub fn resolve_line(git: &Git, config: &Config, flag: Option<&str>) -> Result<LineRef> {
    if let Some(name) = flag {
        let commit = git.rev_parse(name)?;
        return Ok(LineRef {
            name: name.to_owned(),
            commit,
        });
    }
    if let Some(name) = config.base.as_deref() {
        let commit = git.rev_parse(name)?;
        return Ok(LineRef {
            name: name.to_owned(),
            commit,
        });
    }
    for name in ["main", "master"] {
        match git.rev_parse(name) {
            Ok(commit) => {
                return Ok(LineRef {
                    name: name.to_owned(),
                    commit,
                });
            }
            Err(Error::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    let worktrees = git.worktrees()?;
    for worktree in &worktrees {
        if worktree.is_main {
            if let Some(head) = &worktree.head {
                return Ok(LineRef {
                    name: "HEAD".to_owned(),
                    commit: head.clone(),
                });
            }
            break;
        }
    }
    Err(Error::NotFound {
        what: "base line".to_owned(),
    })
}

/// Returns the index of `commit`: loads it when stored with the current configuration hash
/// and format, otherwise builds it (incrementally from the newest stored index when one
/// exists, else in full), stores it under the store lock and records it in `meta.json`.
/// Keeps at most the 5 newest base indexes.
pub fn ensure_index(
    git: &Git,
    store: &Store,
    config: &Config,
    commit: &CommitId,
) -> Result<BaseIndex> {
    match load_stored(store, config, commit)? {
        Stored::Ready(index) => return Ok(index),
        Stored::FactsOnly(stored) => {
            let _lock = store.lock()?;
            return finish_stored(store, commit, stored);
        }
        Stored::Missing => {}
    }
    let _lock = store.lock()?;
    let meta = match meta_for_read(store) {
        Some(meta) => meta,
        None => fresh_meta(),
    };
    match load_with_meta(store, config, commit, &meta)? {
        Stored::Ready(index) => return Ok(index),
        Stored::FactsOnly(stored) => return finish_stored(store, commit, stored),
        Stored::Missing => {}
    }
    build_and_store(git, store, config, commit, &meta)
}

/// Facts of the given tree entries, read through one `cat-file` process. Entries the
/// configuration does not want are left out; skipped files produce one `unparsed` fact each.
pub fn facts_from_blobs(
    cat: &mut CatFile,
    entries: &[TreeEntry],
    config: &Config,
    crates: &[CrateRoot],
) -> Result<Vec<Fact>> {
    let (facts, _load_skipped) = facts_from_blobs_counted(cat, entries, config, crates)?;
    Ok(facts)
}

/// Rust crates of a tree: every `Cargo.toml` blob with a `[package]` name.
pub fn crate_roots(cat: &mut CatFile, entries: &[TreeEntry]) -> Result<Vec<CrateRoot>> {
    let mut roots = Vec::new();
    for entry in entries {
        if entry.path.file_name() != "Cargo.toml" {
            continue;
        }
        if entry.size > MAX_FILE_BYTES {
            continue;
        }
        let bytes = cat.read_blob(&entry.blob)?;
        if u64::try_from(bytes.len()).map_or(true, |size| size > MAX_FILE_BYTES) {
            continue;
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let name = match crate_name_from_manifest(&text) {
            Some(name) => name,
            None => continue,
        };
        let dir = match entry.path.parent() {
            Some(parent) => parent.to_owned(),
            None => String::new(),
        };
        roots.push(CrateRoot { dir, name });
    }
    roots.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(roots)
}

// ---------------------------------------------------------------------------------------------
// Loading a stored index

struct StoredFacts {
    facts: Vec<Fact>,
    facts_hash: String,
    indexed_unix: u64,
    skipped: usize,
}

enum Stored {
    Ready(BaseIndex),
    FactsOnly(StoredFacts),
    Missing,
}

fn fresh_meta() -> Meta {
    Meta {
        format_version: STORE_FORMAT_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        indexed: Vec::new(),
    }
}

/// Meta usable for reading; a missing, unreadable or foreign-version meta, or one written by
/// another version of Arlineo (whose extractors may differ), is treated as absent and
/// replaced by a fresh one under the lock (SPEC 15.5).
fn meta_for_read(store: &Store) -> Option<Meta> {
    match store.read_meta() {
        Ok(Some(meta))
            if meta.format_version == STORE_FORMAT_VERSION
                && meta.tool_version == env!("CARGO_PKG_VERSION") =>
        {
            Some(meta)
        }
        _ => None,
    }
}

fn load_stored(store: &Store, config: &Config, commit: &CommitId) -> Result<Stored> {
    let meta = match meta_for_read(store) {
        Some(meta) => meta,
        None => return Ok(Stored::Missing),
    };
    load_with_meta(store, config, commit, &meta)
}

fn load_with_meta(
    store: &Store,
    config: &Config,
    commit: &CommitId,
    meta: &Meta,
) -> Result<Stored> {
    if meta.format_version != STORE_FORMAT_VERSION {
        return Ok(Stored::Missing);
    }
    let entry = match meta
        .indexed
        .iter()
        .find(|entry| &entry.commit == commit && entry.config_hash == config.hash)
    {
        Some(entry) => entry,
        None => return Ok(Stored::Missing),
    };
    let indexed_unix = entry.indexed_unix;
    let skipped = entry.skipped;
    let facts_hash = entry.facts_hash.clone();
    let dir = store.base_dir(commit);
    // A facts file that does not read cleanly means the index is rebuilt, not an error.
    let facts = match read_facts_with_hash(&dir.join("facts.jsonl")) {
        Ok((facts, hash)) if hash == facts_hash => facts,
        _ => return Ok(Stored::Missing),
    };
    let graph = match read_graph(&dir.join("graph.bin"), &facts_hash)? {
        Some(graph) => graph,
        None => {
            return Ok(Stored::FactsOnly(StoredFacts {
                facts,
                facts_hash,
                indexed_unix,
                skipped,
            }));
        }
    };
    Ok(Stored::Ready(make_index(
        commit,
        facts,
        graph,
        indexed_unix,
        skipped,
        facts_hash,
    )))
}

/// Rebuilds the graph of a stored index whose facts read cleanly; the caller holds the lock.
fn finish_stored(store: &Store, commit: &CommitId, stored: StoredFacts) -> Result<BaseIndex> {
    let graph = build(&stored.facts)?;
    let dir = create_base_dir(store, commit)?;
    write_graph(&dir.join("graph.bin"), &graph, &stored.facts_hash)?;
    Ok(make_index(
        commit,
        stored.facts,
        graph,
        stored.indexed_unix,
        stored.skipped,
        stored.facts_hash,
    ))
}

fn make_index(
    commit: &CommitId,
    facts: Vec<Fact>,
    graph: Graph,
    indexed_unix: u64,
    skipped: usize,
    facts_hash: String,
) -> BaseIndex {
    let fact_index = FactIndex::new(&facts);
    BaseIndex {
        commit: commit.clone(),
        facts,
        fact_index,
        graph,
        indexed_unix,
        skipped,
        facts_hash,
    }
}

/// Unparsed facts at line 0 that came from loading, as far as a stored index can tell:
/// every such reason except lexer errors, which carry a line of their own origin.
fn loaded_skipped_count(facts: &[Fact]) -> usize {
    facts
        .iter()
        .filter(|fact| is_loading_unparsed(fact))
        .count()
}

fn is_loading_unparsed(fact: &Fact) -> bool {
    if fact.kind != FactKind::Unparsed || fact.line != 0 {
        return false;
    }
    match fact.data_as::<UnparsedData>() {
        Ok(data) => !data.reason.starts_with("lexer:"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Building an index

fn build_and_store(
    git: &Git,
    store: &Store,
    config: &Config,
    commit: &CommitId,
    meta: &Meta,
) -> Result<BaseIndex> {
    let listing = git.ls_tree(commit)?;
    let listing_skipped = listing.skipped;
    let entries = listing.items;
    let mut cat = git.cat_file()?;
    let crates = crate_roots(&mut cat, &entries)?;
    let (facts, load_skipped) = build_facts(
        git, store, config, commit, meta, &entries, &crates, &mut cat,
    )?;
    let skipped = listing_skipped.saturating_add(load_skipped);
    write_built_index(store, config, commit, meta, facts, skipped)
}

/// Facts of a new index and how many of them are line-0 unparsed facts from loading.
#[expect(
    clippy::too_many_arguments,
    reason = "independent inputs of one step, used once"
)]
fn build_facts(
    git: &Git,
    store: &Store,
    config: &Config,
    commit: &CommitId,
    meta: &Meta,
    entries: &[TreeEntry],
    crates: &[CrateRoot],
    cat: &mut CatFile,
) -> Result<(Vec<Fact>, usize)> {
    let start = find_start(store, config, commit, meta)?;
    let (start_commit, start_facts) = match start {
        Some(start) => start,
        None => return facts_from_blobs_counted(cat, entries, config, crates),
    };
    let diff = git.diff_names(&start_commit, commit)?;
    let manifest_changed = diff
        .items
        .iter()
        .any(|change| change.path.file_name() == "Cargo.toml");
    if manifest_changed || diff.skipped > 0 {
        return facts_from_blobs_counted(cat, entries, config, crates);
    }
    let changed: BTreeSet<RelPath> = diff
        .items
        .iter()
        .map(|change| change.path.clone())
        .collect();
    let kept: Vec<Fact> = start_facts
        .into_iter()
        .filter(|fact| !changed.contains(&fact.file))
        .collect();
    let kept_skipped = loaded_skipped_count(&kept);
    let reextract: BTreeSet<RelPath> = diff
        .items
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Added | ChangeKind::Modified))
        .map(|change| change.path.clone())
        .collect();
    let changed_entries: Vec<TreeEntry> = entries
        .iter()
        .filter(|entry| reextract.contains(&entry.path))
        .cloned()
        .collect();
    let (new_facts, new_skipped) = facts_from_blobs_counted(cat, &changed_entries, config, crates)?;
    let mut facts = kept;
    facts.extend(new_facts);
    Ok((facts, kept_skipped.saturating_add(new_skipped)))
}

/// The newest stored index with the same config hash whose facts read cleanly and match
/// their recorded hash, other than `commit` itself.
fn find_start(
    store: &Store,
    config: &Config,
    commit: &CommitId,
    meta: &Meta,
) -> Result<Option<(CommitId, Vec<Fact>)>> {
    for entry in &meta.indexed {
        if entry.config_hash != config.hash || &entry.commit == commit {
            continue;
        }
        let path = store.base_dir(&entry.commit).join("facts.jsonl");
        match read_facts_with_hash(&path) {
            Ok((facts, hash)) if hash == entry.facts_hash => {
                return Ok(Some((entry.commit.clone(), facts)));
            }
            _ => continue,
        }
    }
    Ok(None)
}

fn write_built_index(
    store: &Store,
    config: &Config,
    commit: &CommitId,
    meta: &Meta,
    mut facts: Vec<Fact>,
    skipped: usize,
) -> Result<BaseIndex> {
    sort_facts(&mut facts);
    let facts_hash = fnv::hex(&facts_bytes(&facts)?);
    let graph = build(&facts)?;
    let indexed_unix = now_unix();
    let dir = create_base_dir(store, commit)?;
    write_facts(&dir.join("facts.jsonl"), &facts)?;
    write_graph(&dir.join("graph.bin"), &graph, &facts_hash)?;
    let indexed = pruned_meta_entries(meta, commit, indexed_unix, config, &facts_hash, skipped);
    let new_meta = Meta {
        format_version: STORE_FORMAT_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        indexed,
    };
    // Meta is written after the files it points to; unused directories go only after that, so
    // a failed removal can never keep the new index out of `meta.json`.
    store.write_meta(&new_meta)?;
    remove_unused_base_dirs(store, &new_meta.indexed);
    let fact_index = FactIndex::new(&facts);
    Ok(BaseIndex {
        commit: commit.clone(),
        facts,
        fact_index,
        graph,
        indexed_unix,
        skipped,
        facts_hash,
    })
}

/// New entry at the front, older entries for the same commit removed, at most 5 entries.
fn pruned_meta_entries(
    meta: &Meta,
    commit: &CommitId,
    indexed_unix: u64,
    config: &Config,
    facts_hash: &str,
    skipped: usize,
) -> Vec<IndexedCommit> {
    let mut indexed = vec![IndexedCommit {
        commit: commit.clone(),
        indexed_unix,
        config_hash: config.hash.clone(),
        facts_hash: facts_hash.to_owned(),
        skipped,
    }];
    for entry in &meta.indexed {
        if &entry.commit == commit {
            continue;
        }
        if indexed.len() >= 5 {
            break;
        }
        indexed.push(entry.clone());
    }
    indexed
}

/// Removes every directory under `<store root>/base/` whose name is a commit id that no kept
/// entry uses (one directory per commit, so the same commit kept with another configuration
/// keeps its directory). Also clears directories left behind when `meta.json` was replaced.
/// Best effort: a directory that cannot be removed now is tried again after the next build.
fn remove_unused_base_dirs(store: &Store, kept: &[IndexedCommit]) {
    let Ok(entries) = std::fs::read_dir(store.root().join("base")) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(commit) = CommitId::new(&name) else {
            continue;
        };
        if kept.iter().any(|keep| keep.commit == commit) {
            continue;
        }
        // Ignored on purpose: see the doc comment.
        let _ = remove_base_dir(store, &commit);
    }
}

fn create_base_dir(store: &Store, commit: &CommitId) -> Result<PathBuf> {
    let dir = store.base_dir(commit);
    std::fs::create_dir_all(&dir)
        .map_err(|source| Error::io(format!("create directory {}", dir.display()), source))?;
    Ok(dir)
}

/// Removes a dropped index directory, after checking it is a directory directly under
/// `<store root>/base/` named by the dropped commit id.
fn remove_base_dir(store: &Store, commit: &CommitId) -> Result<()> {
    let base = store.root().join("base");
    let dir = store.base_dir(commit);
    if dir.parent() != Some(base.as_path()) {
        return Ok(());
    }
    let name_matches = match dir.file_name() {
        Some(name) => name.to_str() == Some(commit.as_str()),
        None => false,
    };
    if !name_matches {
        return Ok(());
    }
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Ok(());
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::io(
                format!("check directory {}", dir.display()),
                source,
            ));
        }
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io(
            format!("remove directory {}", dir.display()),
            source,
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Facts from blobs

/// Like [`facts_from_blobs`], also counting the line-0 unparsed facts that came from loading
/// (too large, unreadable blob, or a load skip), as opposed to extractor facts.
fn facts_from_blobs_counted(
    cat: &mut CatFile,
    entries: &[TreeEntry],
    config: &Config,
    crates: &[CrateRoot],
) -> Result<(Vec<Fact>, usize)> {
    let mut facts = Vec::new();
    let mut load_skipped: usize = 0;
    for entry in entries {
        if !config.wants(&entry.path) {
            continue;
        }
        if entry.size > MAX_FILE_BYTES {
            facts.push(unparsed_fact(&entry.path, "too large".to_owned())?);
            load_skipped = load_skipped.saturating_add(1);
            continue;
        }
        let bytes = match cat.read_blob(&entry.blob) {
            Ok(bytes) => bytes,
            Err(error) => {
                if matches!(error, Error::NotFound { .. } | Error::Format { .. }) {
                    facts.push(unparsed_fact(&entry.path, format!("{error}"))?);
                    load_skipped = load_skipped.saturating_add(1);
                    continue;
                }
                return Err(error);
            }
        };
        match load_from_blob(entry.path.clone(), bytes) {
            Loaded::Text(file) => {
                let ctx = ExtractCtx { config, crates };
                facts.extend(extract(&file, &ctx)?);
            }
            Loaded::Skipped { path, reason } => {
                facts.push(unparsed_fact(&path, reason)?);
                load_skipped = load_skipped.saturating_add(1);
            }
        }
    }
    Ok((facts, load_skipped))
}

fn unparsed_fact(path: &RelPath, reason: String) -> Result<Fact> {
    Fact::new(
        path,
        0,
        path.as_str(),
        Confidence::Exact,
        &UnparsedData { reason },
    )
}
