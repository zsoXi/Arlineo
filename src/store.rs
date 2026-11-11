//! The index store outside the repository (`SPEC.md`, "Store").

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::git::RepoInfo;
use crate::model::{CommitId, WorktreeId};

/// Version of the store layout and `meta.json`.
pub const STORE_FORMAT_VERSION: u32 = 1;

/// The store directory of one repository.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

/// Contents of `meta.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub format_version: u32,
    pub tool_version: String,
    /// Indexed base commits, newest first.
    pub indexed: Vec<IndexedCommit>,
}

/// One indexed commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexedCommit {
    pub commit: CommitId,
    /// Unix seconds when the index was written.
    pub indexed_unix: u64,
    /// `Config::hash` used for this index.
    pub config_hash: String,
    /// FNV-1a 64 hex of `facts.jsonl`.
    pub facts_hash: String,
    /// Files skipped when this index was built (bad git paths, binary, too large, ...).
    #[serde(default)]
    pub skipped: usize,
}

/// Exclusive store lock; released when dropped.
#[derive(Debug)]
pub struct StoreLock {
    path: PathBuf,
    /// What this process wrote into the lock file; the file is removed only while it still
    /// holds exactly this, so a lock taken over by another process is left alone.
    token: String,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Drop cannot report errors, so a failed read or removal is ignored here.
        if std::fs::read_to_string(&self.path).is_ok_and(|contents| contents == self.token) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// An environment variable holding an absolute path; empty and relative values count as
/// unset (a relative state directory would land inside whatever directory a command runs in).
fn absolute_env(name: &str) -> Option<PathBuf> {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            path.is_absolute().then_some(path)
        }
        _ => None,
    }
}

fn state_root() -> Result<PathBuf> {
    if let Some(root) = absolute_env("ARLINEO_STATE_DIR") {
        return Ok(root);
    }
    if let Some(root) = absolute_env("XDG_STATE_HOME") {
        return Ok(root);
    }
    if let Some(home) = absolute_env("HOME") {
        return Ok(home.join(".local").join("state"));
    }
    Err(Error::Config {
        message: "no state directory: ARLINEO_STATE_DIR, XDG_STATE_HOME and HOME are unset, \
                  empty or not absolute paths"
            .to_owned(),
    })
}

fn repo_id(common_dir: &Path) -> String {
    #[cfg(unix)]
    let bytes: &[u8] = {
        use std::os::unix::ffi::OsStrExt;
        common_dir.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let lossy = common_dir.to_string_lossy();
    #[cfg(not(unix))]
    let bytes: &[u8] = lossy.as_bytes();
    crate::util::fnv::hex(bytes)
}

impl Store {
    /// Store for a repository: `<state>/arlineo/<repo-id>/`, where `<state>` is
    /// `$ARLINEO_STATE_DIR`, else `$XDG_STATE_HOME`, else `$HOME/.local/state`, and
    /// `<repo-id>` is FNV-1a 64 hex of `repo.common_dir`. Creates the directory.
    pub fn open(repo: &RepoInfo) -> Result<Store> {
        let arlineo_dir = state_root()?.join("arlineo");
        let root = arlineo_dir.join(repo_id(&repo.common_dir));
        for dir in [&arlineo_dir, &root] {
            create_private_dir(dir).map_err(|error| {
                Error::io(format!("create store directory {}", dir.display()), error)
            })?;
        }
        Ok(Store { root })
    }

    /// The store directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of a base index: `<root>/base/<commit>/`.
    pub fn base_dir(&self, commit: &CommitId) -> PathBuf {
        self.root.join("base").join(commit.as_str())
    }

    /// Directory of a worktree overlay: `<root>/overlay/<worktree-id>/`.
    pub fn overlay_dir(&self, worktree: &WorktreeId) -> PathBuf {
        self.root.join("overlay").join(worktree.as_str())
    }

    /// Takes the exclusive lock (`<root>/lock`), waiting up to 30 seconds; a lock older than
    /// 10 minutes is taken over. `Error::Locked` on timeout.
    pub fn lock(&self) -> Result<StoreLock> {
        let lock_path = self.root.join("lock");
        let started = std::time::Instant::now();
        let deadline = Duration::from_secs(30);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    let contents = format!("{} {}\n", std::process::id(), now_unix());
                    match file.write_all(contents.as_bytes()) {
                        Ok(()) => {
                            return Ok(StoreLock {
                                path: lock_path,
                                token: contents,
                            });
                        }
                        Err(error) => {
                            // Do not leave a lock nobody holds; removal is best effort.
                            let _ = std::fs::remove_file(&lock_path);
                            return Err(Error::io(
                                format!("write lock file {}", lock_path.display()),
                                error,
                            ));
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let seen = std::fs::read_to_string(&lock_path).unwrap_or_default();
                    // Re-read just before removing: if another process took the stale lock over
                    // meanwhile, its new contents differ and the lock is left alone.
                    let still_same =
                        || std::fs::read_to_string(&lock_path).is_ok_and(|now| now == seen);
                    if is_stale_lock(&lock_path, &seen) && still_same() {
                        match std::fs::remove_file(&lock_path) {
                            Ok(()) => {
                                if started.elapsed() >= deadline {
                                    return Err(Error::Locked { path: lock_path });
                                }
                                continue;
                            }
                            Err(remove_error) if remove_error.kind() == ErrorKind::NotFound => {
                                if started.elapsed() >= deadline {
                                    return Err(Error::Locked { path: lock_path });
                                }
                                continue;
                            }
                            Err(remove_error) => {
                                return Err(Error::io(
                                    format!("remove stale lock file {}", lock_path.display()),
                                    remove_error,
                                ));
                            }
                        }
                    }
                    if started.elapsed() >= deadline {
                        return Err(Error::Locked { path: lock_path });
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => {
                    return Err(Error::io(
                        format!("create lock file {}", lock_path.display()),
                        error,
                    ));
                }
            }
        }
    }

    /// Reads `meta.json`; `Ok(None)` when it does not exist, is not readable JSON, or has
    /// another format version.
    pub fn read_meta(&self) -> Result<Option<Meta>> {
        let path = self.root.join("meta.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::io(format!("read {}", path.display()), error));
            }
        };
        // The store is a cache: unreadable or foreign-version metadata counts as missing.
        match serde_json::from_slice::<Meta>(&bytes) {
            Ok(meta) if meta.format_version == STORE_FORMAT_VERSION => Ok(Some(meta)),
            Ok(_) => Ok(None),
            Err(_) => Ok(None),
        }
    }

    /// Writes `meta.json` atomically.
    pub fn write_meta(&self, meta: &Meta) -> Result<()> {
        let path = self.root.join("meta.json");
        let bytes = serde_json::to_vec_pretty(meta)
            .map_err(|error| Error::json(format!("write {}", path.display()), error))?;
        write_atomic(&path, &bytes)
    }
}

/// Writes a file atomically: a temporary file in the same directory, flushed and synced,
/// then renamed over the target. Creates missing parent directories.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::io(format!("create directory {}", parent.display()), error)
            })?;
        }
    }
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(format!(".tmp-{}", std::process::id()));
    let tmp_path = PathBuf::from(tmp_os);
    match write_atomic_attempt(path, &tmp_path, bytes) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Keep the first error; removing the temporary file is best effort.
            let _ = std::fs::remove_file(&tmp_path);
            Err(error)
        }
    }
}

fn write_atomic_attempt(path: &Path, tmp_path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp_path)
        .map_err(|error| {
            Error::io(
                format!("create temporary file {}", tmp_path.display()),
                error,
            )
        })?;
    file.write_all(bytes).map_err(|error| {
        Error::io(
            format!("write temporary file {}", tmp_path.display()),
            error,
        )
    })?;
    file.sync_all()
        .map_err(|error| Error::io(format!("sync temporary file {}", tmp_path.display()), error))?;
    drop(file);
    std::fs::rename(tmp_path, path).map_err(|error| {
        Error::io(
            format!("rename {} to {}", tmp_path.display(), path.display()),
            error,
        )
    })?;
    Ok(())
}

/// Whether a lock file is abandoned: older than 10 minutes and, where `/proc` exists, its
/// holder process is gone (a live holder of a long build keeps its lock).
fn is_stale_lock(lock_path: &Path, contents: &str) -> bool {
    let metadata = match std::fs::metadata(lock_path) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(_) => return false,
    };
    let old = match modified.elapsed() {
        Ok(age) => age > Duration::from_secs(600),
        Err(_) => false,
    };
    if !old {
        return false;
    }
    let proc_root = Path::new("/proc");
    if !proc_root.is_dir() {
        return true;
    }
    match contents
        .split_whitespace()
        .next()
        .and_then(|pid| pid.parse::<u32>().ok())
    {
        Some(pid) => !proc_root.join(pid.to_string()).exists(),
        None => true,
    }
}

/// Creates a directory and its missing parents; on Unix the store directories are private
/// to the user (mode 0700), because facts describe the source code.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Current Unix time in seconds (0 if the clock is before 1970).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
