//! Loading source files from git blobs and from worktrees on disk (`SPEC.md`, "Sources").

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{Lang, RelPath};

/// Largest file Arlineo reads, in bytes.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A readable source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub path: RelPath,
    pub lang: Lang,
    pub text: String,
    /// FNV-1a 64 of the text (the bytes after a leading UTF-8 BOM is removed), 16 hex digits.
    pub hash: String,
}

/// Result of trying to load a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    Text(SourceFile),
    /// Not read: unknown language, secret file, too large, binary (contains NUL), invalid
    /// UTF-8, or changing while being read. The reason is a short English phrase.
    Skipped {
        path: RelPath,
        reason: String,
    },
}

/// Turns blob bytes into a source file.
pub fn load_from_blob(path: RelPath, bytes: Vec<u8>) -> Loaded {
    let lang = match Lang::from_path(&path) {
        Some(lang) => lang,
        None => {
            return Loaded::Skipped {
                path,
                reason: "unsupported language".to_owned(),
            };
        }
    };
    if is_secret_path(&path) {
        return Loaded::Skipped {
            path,
            reason: "secret file".to_owned(),
        };
    }
    let too_large = match u64::try_from(bytes.len()) {
        Ok(len) => len > MAX_FILE_BYTES,
        Err(_) => true,
    };
    if too_large {
        return Loaded::Skipped {
            path,
            reason: "too large".to_owned(),
        };
    }
    if bytes.contains(&0) {
        return Loaded::Skipped {
            path,
            reason: "binary".to_owned(),
        };
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return Loaded::Skipped {
                path,
                reason: "not utf-8".to_owned(),
            };
        }
    };
    let text = match text.strip_prefix('\u{feff}') {
        Some(stripped) => stripped,
        None => &text,
    };
    let hash = crate::util::fnv::hex(text.as_bytes());
    Loaded::Text(SourceFile {
        path,
        lang,
        text: text.to_owned(),
        hash,
    })
}

fn skipped(path: RelPath, reason: &str) -> Loaded {
    Loaded::Skipped {
        path,
        reason: reason.to_owned(),
    }
}

/// Reads `root/path` from disk. `Ok(None)` when the file does not exist. Never follows a
/// symlink (symlinks are skipped). Re-reads when size or modification time changed during
/// the read, at most 3 times, then skips with reason "changing while read".
pub fn load_from_disk(root: &Path, path: &RelPath) -> Result<Option<Loaded>> {
    // The secret check comes first, so a secret file is never touched on disk.
    if is_secret_path(path) {
        return Ok(Some(skipped(path.clone(), "secret file")));
    }
    let full = root.join(path.as_str());
    if !parent_inside_root(root, &full) {
        return Ok(Some(skipped(path.clone(), "not a regular file")));
    }
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        let before = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io(path.as_str().to_owned(), error)),
        };
        if before.file_type().is_symlink() || !before.file_type().is_file() {
            return Ok(Some(skipped(path.clone(), "not a regular file")));
        }
        let before_len = before.len();
        if before_len > MAX_FILE_BYTES {
            return Ok(Some(skipped(path.clone(), "too large")));
        }
        let before_modified = match before.modified() {
            Ok(time) => time,
            Err(error) => return Err(Error::io(path.as_str().to_owned(), error)),
        };
        let bytes = match std::fs::read(&full) {
            Ok(bytes) => bytes,
            Err(error) => return Err(Error::io(path.as_str().to_owned(), error)),
        };
        let after = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) => return Err(Error::io(path.as_str().to_owned(), error)),
        };
        let after_modified = match after.modified() {
            Ok(time) => time,
            Err(error) => return Err(Error::io(path.as_str().to_owned(), error)),
        };
        let bytes_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let changed = after.file_type().is_symlink()
            || !after.file_type().is_file()
            || before_len != after.len()
            || bytes_len != after.len()
            || before_modified != after_modified;
        if changed {
            if attempt >= 3 {
                return Ok(Some(skipped(path.clone(), "changing while read")));
            }
            continue;
        }
        return Ok(Some(load_from_blob(path.clone(), bytes)));
    }
}

/// Whether the directory holding `full` resolves to a place inside `root`, so a directory
/// replaced by a symlink cannot lead a read outside the worktree. A parent that cannot be
/// resolved (deleted meanwhile) counts as inside: the read itself then reports it.
fn parent_inside_root(root: &Path, full: &Path) -> bool {
    let Some(parent) = full.parent() else {
        return false;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return true;
    };
    match std::fs::canonicalize(parent) {
        Ok(parent) => parent.starts_with(&root),
        Err(_) => true,
    }
}

/// Whether a path names a secret file that must never be read (`SPEC.md`, "Security").
pub fn is_secret_path(path: &RelPath) -> bool {
    path.as_str().split('/').any(is_secret_segment)
}

fn is_secret_segment(segment: &str) -> bool {
    if segment == "secrets" || segment == ".ssh" {
        return true;
    }
    is_secret_name(segment)
}

fn is_secret_name(name: &str) -> bool {
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".keystore")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
}
