//! Running `git` as an external program (`SPEC.md`, "Git").
//!
//! Every call uses `git -C <dir>` with arguments as separate process arguments (never a
//! shell), `-z` output where git offers it, and `LC_ALL=C`.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use crate::error::{Error, Result};
use crate::model::{CommitId, RelPath};

/// Environment variables that redirect git to another repository, index or work tree.
const INHERITED_GIT_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Runs git in one directory.
#[derive(Clone, Debug)]
pub struct Git {
    dir: PathBuf,
}

/// Repository facts for the directory a command runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// Top-level directory of the current worktree (absolute, canonical).
    pub top: PathBuf,
    /// The repository's common git directory (absolute, canonical); the same for all worktrees.
    pub common_dir: PathBuf,
    /// `HEAD` of the current worktree, `None` in an empty repository.
    pub head: Option<CommitId>,
}

/// A blob in a commit's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: RelPath,
    pub blob: String,
    pub size: u64,
}

/// Paths git reported that are not valid UTF-8 or not valid [`RelPath`]s are skipped and
/// counted here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub skipped: usize,
}

/// Kind of a file change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A changed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: RelPath,
    pub kind: ChangeKind,
}

/// A worktree from `git worktree list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Absolute path.
    pub path: PathBuf,
    pub head: Option<CommitId>,
    /// Short branch name (`feature/x`), `None` when detached.
    pub branch: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
    pub prunable: bool,
}

impl Git {
    /// Git runner for `dir`.
    pub fn new(dir: &Path) -> Git {
        Git {
            dir: dir.to_path_buf(),
        }
    }

    /// Builds the base command shared by every git call.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        // Variables a git hook or another tool may have set would point git at another
        // repository, index or work tree than `-C` names.
        for name in INHERITED_GIT_VARIABLES {
            command.env_remove(name);
        }
        command
            .arg("-C")
            .arg(&self.dir)
            // A repository's own configuration must not start programs on our behalf.
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .env("LC_ALL", "C")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            // Partial clones must not fetch missing objects over the network.
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs a command without checking its exit status.
    fn output(&self, args: &[&str]) -> Result<Output> {
        let mut command = self.command(args);
        command
            .output()
            .map_err(|error| Error::io("running git", error))
    }

    /// Runs `git -C dir <args>` and returns stdout. `Error::Git` with stderr on a non-zero
    /// exit, `Error::Io` when git cannot be started.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self.output(args)?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(git_error(args, &output))
        }
    }

    /// Top level, common dir and HEAD.
    pub fn repo_info(&self) -> Result<RepoInfo> {
        let top_args: &[&str] = &["rev-parse", "--show-toplevel"];
        let top_output = self.run(top_args)?;
        let top_path = match path_from_bytes(trim_line_bytes(&top_output)) {
            Some(path) => path,
            None => return Err(output_error(top_args, "top-level path is not valid UTF-8")),
        };
        let top = std::fs::canonicalize(&top_path)
            .map_err(|error| Error::io(format!("canonicalizing {}", top_path.display()), error))?;

        let common_args: &[&str] = &["rev-parse", "--git-common-dir"];
        let common_output = self.run(common_args)?;
        let common_raw = match path_from_bytes(trim_line_bytes(&common_output)) {
            Some(path) => path,
            None => {
                return Err(output_error(
                    common_args,
                    "common dir path is not valid UTF-8",
                ));
            }
        };
        // A relative common dir is relative to the directory git ran in (`-C`), which may be
        // a subdirectory of the top level.
        let common_joined = if common_raw.is_absolute() {
            common_raw
        } else {
            self.dir.join(&common_raw)
        };
        let common_dir = std::fs::canonicalize(&common_joined).map_err(|error| {
            Error::io(format!("canonicalizing {}", common_joined.display()), error)
        })?;

        let head_args: &[&str] = &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"];
        let head_output = self.output(head_args)?;
        let head = if head_output.status.success() {
            let command = head_args.join(" ");
            Some(parse_commit_id(&head_output.stdout, &command)?)
        } else if head_output.stdout.is_empty() {
            None
        } else {
            return Err(git_error(head_args, &head_output));
        };

        Ok(RepoInfo {
            top,
            common_dir,
            head,
        })
    }

    /// Commit id of a revision (`rev^{commit}`); `Error::NotFound` when it does not exist.
    pub fn rev_parse(&self, rev: &str) -> Result<CommitId> {
        if rev.is_empty() || rev.starts_with('-') {
            return Err(Error::Usage {
                message: format!("invalid revision {rev:?}"),
            });
        }
        let rev_arg = format!("{rev}^{{commit}}");
        let args: &[&str] = &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            rev_arg.as_str(),
        ];
        let output = self.output(args)?;
        if output.status.success() {
            let command = args.join(" ");
            parse_commit_id(&output.stdout, &command)
        } else if output.status.code() == Some(1) && output.stdout.is_empty() {
            Err(Error::NotFound {
                what: format!("revision {rev}"),
            })
        } else {
            Err(git_error(args, &output))
        }
    }

    /// Best common ancestor, or `None` when the histories are unrelated.
    pub fn merge_base(&self, a: &CommitId, b: &CommitId) -> Result<Option<CommitId>> {
        let args: &[&str] = &["merge-base", a.as_str(), b.as_str()];
        let output = self.output(args)?;
        if output.status.success() {
            let command = args.join(" ");
            Ok(Some(parse_commit_id(&output.stdout, &command)?))
        } else if output.status.code() == Some(1) && output.stdout.is_empty() {
            Ok(None)
        } else {
            Err(git_error(args, &output))
        }
    }

    /// Regular files (modes 100644 and 100755) of a commit; symlinks and submodules are left out.
    pub fn ls_tree(&self, commit: &CommitId) -> Result<Listing<TreeEntry>> {
        let args: &[&str] = &["ls-tree", "-r", "-l", "-z", "--full-tree", commit.as_str()];
        let output = self.run(args)?;
        let mut listing = Listing {
            items: Vec::new(),
            skipped: 0,
        };
        for record in output.split(|byte| *byte == b'\0') {
            if record.is_empty() {
                continue;
            }
            let tab = match record.iter().position(|byte| *byte == b'\t') {
                Some(tab) => tab,
                None => return Err(ls_tree_error(record, "missing tab")),
            };
            let meta_bytes = match record.get(..tab) {
                Some(bytes) => bytes,
                None => return Err(ls_tree_error(record, "missing metadata")),
            };
            let after_tab = match record.get(tab..) {
                Some(bytes) => bytes,
                None => return Err(ls_tree_error(record, "missing path")),
            };
            let path_bytes = match after_tab.get(1..) {
                Some(bytes) => bytes,
                None => return Err(ls_tree_error(record, "missing path")),
            };
            let meta = match std::str::from_utf8(meta_bytes) {
                Ok(meta) => meta,
                Err(_) => return Err(ls_tree_error(record, "metadata is not valid UTF-8")),
            };
            let mut parts = meta.split_whitespace();
            let mode = match parts.next() {
                Some(part) => part,
                None => return Err(ls_tree_error(record, "missing mode")),
            };
            let object_type = match parts.next() {
                Some(part) => part,
                None => return Err(ls_tree_error(record, "missing object type")),
            };
            let oid = match parts.next() {
                Some(part) => part,
                None => return Err(ls_tree_error(record, "missing object id")),
            };
            let size_text = match parts.next() {
                Some(part) => part,
                None => return Err(ls_tree_error(record, "missing size")),
            };
            if parts.next().is_some() {
                return Err(ls_tree_error(record, "too many metadata fields"));
            }
            if object_type != "blob" {
                continue;
            }
            if mode != "100644" && mode != "100755" {
                continue;
            }
            let size = match size_text.parse::<u64>() {
                Ok(size) => size,
                Err(_) => return Err(ls_tree_error(record, "invalid size")),
            };
            let path = match valid_rel_path(path_bytes) {
                Some(path) => path,
                None => {
                    listing.skipped = listing.skipped.saturating_add(1);
                    continue;
                }
            };
            listing.items.push(TreeEntry {
                path,
                blob: oid.to_owned(),
                size,
            });
        }
        listing
            .items
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(listing)
    }

    /// Files changed between two commits, renames reported as delete plus add.
    pub fn diff_names(&self, from: &CommitId, to: &CommitId) -> Result<Listing<Change>> {
        let args: &[&str] = &[
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            from.as_str(),
            to.as_str(),
            "--",
        ];
        let output = self.run(args)?;
        let mut listing = Listing {
            items: Vec::new(),
            skipped: 0,
        };
        let mut fields = output.split(|byte| *byte == b'\0');
        while let Some(status) = fields.next() {
            if status.is_empty() {
                break;
            }
            let path_bytes = match fields.next() {
                Some(path) => path,
                None => {
                    return Err(Error::Format {
                        path: PathBuf::from("git diff"),
                        message: "incomplete name-status record in git diff output".to_owned(),
                    });
                }
            };
            let path = match valid_rel_path(path_bytes) {
                Some(path) => path,
                None => {
                    listing.skipped = listing.skipped.saturating_add(1);
                    continue;
                }
            };
            let kind = match status.first().copied() {
                Some(b'A') => ChangeKind::Added,
                Some(b'D') => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            listing.items.push(Change { path, kind });
        }
        listing
            .items
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(listing)
    }

    /// Uncommitted changes of this worktree against its HEAD, including untracked files that
    /// are not ignored; renames reported as delete plus add.
    pub fn status(&self) -> Result<Listing<Change>> {
        let args: &[&str] = &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ];
        let output = self.run(args)?;
        let mut listing = Listing {
            items: Vec::new(),
            skipped: 0,
        };
        for record in output.split(|byte| *byte == b'\0') {
            if record.is_empty() {
                continue;
            }
            let x = match record.first() {
                Some(byte) => *byte,
                None => return Err(status_error(record)),
            };
            let y = match record.get(1) {
                Some(byte) => *byte,
                None => return Err(status_error(record)),
            };
            if record.get(2) != Some(&b' ') {
                return Err(status_error(record));
            }
            if x == b'!' && y == b'!' {
                continue;
            }
            let path_bytes = match record.get(3..) {
                Some(bytes) => bytes,
                None => return Err(status_error(record)),
            };
            let path = match valid_rel_path(path_bytes) {
                Some(path) => path,
                None => {
                    listing.skipped = listing.skipped.saturating_add(1);
                    continue;
                }
            };
            // Unmerged entries (`U` on either side, `AA`, `DD`) are conflicts: the file is
            // usually on disk, so they count as modified; a missing file is found later.
            let unmerged =
                x == b'U' || y == b'U' || (x == b'A' && y == b'A') || (x == b'D' && y == b'D');
            let kind = if x == b'?' && y == b'?' {
                ChangeKind::Added
            } else if unmerged {
                ChangeKind::Modified
            } else if (x == b'D' || y == b'D') && x != b'A' && y != b'A' {
                ChangeKind::Deleted
            } else {
                ChangeKind::Modified
            };
            listing.items.push(Change { path, kind });
        }
        listing
            .items
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(listing)
    }

    /// All worktrees of the repository.
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let args: &[&str] = &["worktree", "list", "--porcelain", "-z"];
        let output = self.run(args)?;
        let mut worktrees: Vec<WorktreeInfo> = Vec::new();
        let mut draft = WorktreeDraft::default();
        let mut first_record = true;
        for field in output.split(|byte| *byte == b'\0') {
            if field.is_empty() {
                let touched = draft.touched;
                finish_worktree(std::mem::take(&mut draft), &mut worktrees, first_record);
                if touched {
                    first_record = false;
                }
                continue;
            }
            draft.touched = true;
            if let Some(rest) = strip_byte_prefix(field, b"worktree ") {
                draft.path = path_from_bytes(rest);
                if draft.path.is_none() {
                    draft.skip = true;
                }
            } else if let Some(rest) = strip_byte_prefix(field, b"HEAD ") {
                let text = match std::str::from_utf8(rest) {
                    Ok(text) => text,
                    Err(_) => return Err(output_error(args, "worktree HEAD is not valid UTF-8")),
                };
                if !text.is_empty() && !text.bytes().all(|byte| byte == b'0') {
                    draft.head = Some(CommitId::new(text)?);
                } else {
                    draft.head = None;
                }
            } else if let Some(rest) = strip_byte_prefix(field, b"branch ") {
                match std::str::from_utf8(rest) {
                    Ok(text) => {
                        let name = match text.strip_prefix("refs/heads/") {
                            Some(name) => name,
                            None => text,
                        };
                        draft.branch = Some(name.to_owned());
                    }
                    Err(_) => {
                        draft.branch = None;
                    }
                }
            } else if field == b"bare".as_slice() {
                draft.is_bare = true;
            } else if field == b"detached".as_slice() {
                draft.branch = None;
            } else if field == b"prunable".as_slice()
                || strip_byte_prefix(field, b"prunable ").is_some()
            {
                draft.prunable = true;
            } else if field == b"locked".as_slice()
                || strip_byte_prefix(field, b"locked ").is_some()
            {
                // A locked worktree is still listed; the lock state has no field here.
            }
        }
        finish_worktree(draft, &mut worktrees, first_record);
        Ok(worktrees)
    }

    /// Starts a `git cat-file --batch` reader.
    pub fn cat_file(&self) -> Result<CatFile> {
        let args: &[&str] = &["cat-file", "--batch"];
        let mut command = self.command(args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| Error::io("running git", error))?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => return Err(cat_file_error("git cat-file has no stdin pipe")),
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return Err(cat_file_error("git cat-file has no stdout pipe")),
        };
        Ok(CatFile {
            child,
            stdin: Some(BufWriter::new(stdin)),
            stdout: BufReader::new(stdout),
        })
    }
}

/// A running `git cat-file --batch` process. Dropping it closes stdin and waits for git.
#[derive(Debug)]
pub struct CatFile {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    /// Discards `size` body bytes and the single newline after them.
    fn discard_body(&mut self, size: u64) -> Result<()> {
        let mut remaining = size;
        let mut buffer = [0_u8; 65536];
        while remaining > 0 {
            // `want` is at most the buffer length, so it always fits in `usize`.
            let want = remaining.min(65536);
            let want_len = match usize::try_from(want) {
                Ok(len) => len,
                Err(_) => return Err(cat_file_error("blob size does not fit in memory")),
            };
            let chunk = match buffer.get_mut(..want_len) {
                Some(chunk) => chunk,
                None => return Err(cat_file_error("blob size does not fit in memory")),
            };
            self.stdout
                .read_exact(chunk)
                .map_err(|error| cat_file_error(&format!("reading from git cat-file: {error}")))?;
            // `want` is at most `remaining`, so this subtraction cannot underflow.
            remaining -= want;
        }
        read_body_newline(&mut self.stdout)
    }

    /// Contents of a blob; `Error::NotFound` for a missing object, `Error::Git` when the
    /// object is not a blob or the output cannot be read.
    pub fn read_blob(&mut self, oid: &str) -> Result<Vec<u8>> {
        let valid_oid = (oid.len() == 40 || oid.len() == 64)
            && oid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !valid_oid {
            return Err(Error::Usage {
                message: format!("invalid object id {oid:?}"),
            });
        }
        {
            let writer = match self.stdin.as_mut() {
                Some(writer) => writer,
                None => return Err(cat_file_error("git cat-file stdin is closed")),
            };
            writer
                .write_all(format!("{oid}\n").as_bytes())
                .map_err(|error| cat_file_error(&format!("writing to git cat-file: {error}")))?;
            writer
                .flush()
                .map_err(|error| cat_file_error(&format!("writing to git cat-file: {error}")))?;
        }
        let mut header = String::new();
        let read = self
            .stdout
            .read_line(&mut header)
            .map_err(|error| cat_file_error(&format!("reading from git cat-file: {error}")))?;
        if read == 0 {
            return Err(cat_file_error("unexpected end of git cat-file output"));
        }
        let header_line = strip_line_end(&header);
        if header_line == format!("{oid} missing").as_str() {
            return Err(Error::NotFound {
                what: format!("object {oid}"),
            });
        }
        let mut parts = header_line.split_whitespace();
        let echoed_oid = match parts.next() {
            Some(part) => part,
            None => return Err(cat_file_error("invalid empty header from git cat-file")),
        };
        let object_type = match parts.next() {
            Some(part) => part,
            None => return Err(cat_file_error("invalid header from git cat-file")),
        };
        let size_text = match parts.next() {
            Some(part) => part,
            None => return Err(cat_file_error("invalid header from git cat-file")),
        };
        if parts.next().is_some() {
            return Err(cat_file_error("invalid header from git cat-file"));
        }
        if echoed_oid != oid {
            return Err(cat_file_error(
                "header from git cat-file names another object",
            ));
        }
        let size = match size_text.parse::<u64>() {
            Ok(size) => size,
            Err(_) => return Err(cat_file_error("invalid blob size from git cat-file")),
        };
        if object_type != "blob" {
            self.discard_body(size)?;
            return Err(cat_file_error(&format!(
                "object {oid} is a {object_type}, not a blob"
            )));
        }
        if size > crate::source::MAX_FILE_BYTES {
            self.discard_body(size)?;
            return Err(Error::Format {
                path: PathBuf::from("git cat-file"),
                message: "blob too large".to_owned(),
            });
        }
        // `size` is at most MAX_FILE_BYTES (4 MiB), so it fits in `usize` on supported targets.
        let size_len = match usize::try_from(size) {
            Ok(len) => len,
            Err(_) => return Err(cat_file_error("blob size does not fit in memory")),
        };
        let mut body = vec![0_u8; size_len];
        self.stdout
            .read_exact(&mut body)
            .map_err(|error| cat_file_error(&format!("reading from git cat-file: {error}")))?;
        read_body_newline(&mut self.stdout)?;
        Ok(body)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        // Drop cannot report errors: dropping the writer closes stdin, and the wait
        // result is deliberately ignored here.
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// Worktree attributes collected before the record separator.
#[derive(Default)]
struct WorktreeDraft {
    touched: bool,
    path: Option<PathBuf>,
    head: Option<CommitId>,
    branch: Option<String>,
    is_bare: bool,
    prunable: bool,
    skip: bool,
}

fn finish_worktree(draft: WorktreeDraft, worktrees: &mut Vec<WorktreeInfo>, is_main: bool) {
    if draft.skip {
        return;
    }
    let path = match draft.path {
        Some(path) => path,
        None => return,
    };
    worktrees.push(WorktreeInfo {
        path,
        head: draft.head,
        branch: draft.branch,
        is_main,
        is_bare: draft.is_bare,
        prunable: draft.prunable,
    });
}

fn git_error(args: &[&str], output: &Output) -> Error {
    Error::Git {
        command: args.join(" "),
        status: output.status.code(),
        stderr: truncated_stderr(&output.stderr),
    }
}

fn output_error(args: &[&str], message: &str) -> Error {
    Error::Git {
        command: args.join(" "),
        status: None,
        stderr: message.to_owned(),
    }
}

fn cat_file_error(message: &str) -> Error {
    Error::Git {
        command: "cat-file --batch".to_owned(),
        status: None,
        stderr: message.to_owned(),
    }
}

fn ls_tree_error(record: &[u8], reason: &str) -> Error {
    Error::Format {
        path: PathBuf::from("git ls-tree"),
        message: format!("{reason} in record {:?}", String::from_utf8_lossy(record)),
    }
}

fn status_error(record: &[u8]) -> Error {
    Error::Format {
        path: PathBuf::from("git status"),
        message: format!("invalid record {:?}", String::from_utf8_lossy(record)),
    }
}

fn truncated_stderr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.len() <= 2000 {
        return trimmed.to_owned();
    }
    let mut end = 2000_usize;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match trimmed.get(..end) {
        Some(cut) => cut.to_owned(),
        None => String::new(),
    }
}

fn strip_line_end(text: &str) -> &str {
    let without_lf = match text.strip_suffix('\n') {
        Some(text) => text,
        None => text,
    };
    match without_lf.strip_suffix('\r') {
        Some(text) => text,
        None => without_lf,
    }
}

fn trim_line_bytes(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if bytes.last() == Some(&b'\n') {
        end = end.saturating_sub(1);
    }
    if end > 0 {
        if let Some(last) = end.checked_sub(1) {
            if bytes.get(last) == Some(&b'\r') {
                end = last;
            }
        }
    }
    match bytes.get(..end) {
        Some(trimmed) => trimmed,
        None => bytes,
    }
}

fn text_line(bytes: &[u8], command: &str) -> Result<String> {
    match std::str::from_utf8(trim_line_bytes(bytes)) {
        Ok(text) => Ok(text.to_owned()),
        Err(_) => Err(Error::Git {
            command: command.to_owned(),
            status: None,
            stderr: "output is not valid UTF-8".to_owned(),
        }),
    }
}

fn parse_commit_id(bytes: &[u8], command: &str) -> Result<CommitId> {
    let text = text_line(bytes, command)?;
    CommitId::new(&text)
}

fn valid_rel_path(bytes: &[u8]) -> Option<RelPath> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return None,
    };
    RelPath::new(text).ok()
}

fn strip_byte_prefix<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if bytes.len() < prefix.len() {
        return None;
    }
    match bytes.get(..prefix.len()) {
        Some(head) if head == prefix => bytes.get(prefix.len()..),
        _ => None,
    }
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(PathBuf::from(text)),
        Err(_) => None,
    }
}

fn read_body_newline(stdout: &mut BufReader<ChildStdout>) -> Result<()> {
    let mut newline = [0_u8; 1];
    stdout
        .read_exact(&mut newline)
        .map_err(|error| cat_file_error(&format!("reading from git cat-file: {error}")))?;
    if newline.first() == Some(&b'\n') {
        Ok(())
    } else {
        Err(cat_file_error(
            "blob from git cat-file is not followed by a newline",
        ))
    }
}
