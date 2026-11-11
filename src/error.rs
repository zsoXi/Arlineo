//! The single error type of Arlineo.
//!
//! Every fallible function returns [`Result`]. Variants keep the operation context and,
//! where there is one, the underlying cause (available through `Error::source`).

use std::fmt;
use std::io;
use std::path::PathBuf;

/// All failures Arlineo reports.
#[derive(Debug)]
pub enum Error {
    /// Wrong command-line usage. Exit code 2.
    Usage { message: String },
    /// A git command failed or printed output Arlineo cannot read.
    Git {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    /// A file system operation failed.
    Io { context: String, source: io::Error },
    /// JSON could not be read or written.
    Json {
        context: String,
        source: serde_json::Error,
    },
    /// `arlineo.json` is invalid.
    Config { message: String },
    /// A stored file is corrupt, truncated or written by another format version.
    Format { path: PathBuf, message: String },
    /// A path is not a valid repository-relative path.
    InvalidPath { path: String },
    /// A string is not a full commit id.
    InvalidCommitId { value: String },
    /// Something the user asked for does not exist (a revision, a worktree, a symbol).
    NotFound { what: String },
    /// Another Arlineo process holds the store lock for too long.
    Locked { path: PathBuf },
    /// An internal invariant was broken. This is a bug in Arlineo.
    Internal { message: String },
}

/// Result with the Arlineo error type.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Builds an [`Error::Io`] with context.
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// Builds an [`Error::Json`] with context.
    pub fn json(context: impl Into<String>, source: serde_json::Error) -> Error {
        Error::Json {
            context: context.into(),
            source,
        }
    }

    /// Process exit code for this error: 2 for usage errors, 1 for everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage { .. } => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage { message } => write!(f, "usage: {message}"),
            Error::Git {
                command,
                status,
                stderr,
            } => {
                match status {
                    Some(code) => write!(f, "git {command} failed with exit code {code}")?,
                    None => write!(f, "git {command} failed")?,
                }
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Error::Io { context, .. } => write!(f, "{context}"),
            Error::Json { context, .. } => write!(f, "{context}"),
            Error::Config { message } => write!(f, "arlineo.json: {message}"),
            Error::Format { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            Error::InvalidPath { path } => write!(f, "invalid repository path {path:?}"),
            Error::InvalidCommitId { value } => write!(f, "invalid commit id {value:?}"),
            Error::NotFound { what } => write!(f, "not found: {what}"),
            Error::Locked { path } => write!(
                f,
                "another arlineo process holds the lock {}",
                path.display()
            ),
            Error::Internal { message } => {
                write!(f, "internal error (a bug in arlineo): {message}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}
