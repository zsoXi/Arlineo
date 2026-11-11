//! Reading and writing `facts.jsonl` (`SPEC.md`, "Facts file").

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{FACTS_FORMAT_VERSION, Fact};

/// The exact first line of every facts file.
const HEADER: &str = "{\"format\":\"arlineo-facts\",\"version\":1}";

/// Reads a facts file: checks the header line, then parses one fact per line.
/// `Error::Format` for a missing or foreign header, `Error::Json` with the line number for a
/// bad line, `Error::Io` when the file cannot be read.
pub fn read_facts(path: &Path) -> Result<Vec<Fact>> {
    read_facts_with_hash(path).map(|(facts, _)| facts)
}

/// Like [`read_facts`], also returning the FNV-1a 64 hex of the file bytes, which equals the
/// hash of [`facts_bytes`] for a file this module wrote.
pub fn read_facts_with_hash(path: &Path) -> Result<(Vec<Fact>, String)> {
    let bytes = std::fs::read(path)
        .map_err(|source| Error::io(format!("reading {}", path.display()), source))?;
    let hash = crate::util::fnv::hex(&bytes);
    Ok((parse_facts(path, &bytes)?, hash))
}

fn parse_facts(path: &Path, bytes: &[u8]) -> Result<Vec<Fact>> {
    if bytes.is_empty() {
        return Err(Error::Format {
            path: path.to_path_buf(),
            message: "missing facts header".to_owned(),
        });
    }
    // Split on `\n` only, never with `str::lines`, so a carriage return before
    // the newline stays visible and can be rejected instead of being hidden.
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    // A final newline leaves one empty trailing piece; that one line is
    // allowed. Any empty piece left after dropping it is a real empty line.
    let trailing_is_empty = match lines.last() {
        Some(last) => last.is_empty(),
        None => false,
    };
    if trailing_is_empty {
        lines.pop();
    }
    let mut lines_iter = lines.iter();
    let header = match lines_iter.next() {
        Some(header) => *header,
        None => {
            return Err(Error::Format {
                path: path.to_path_buf(),
                message: "missing facts header".to_owned(),
            });
        }
    };
    check_no_carriage_return(path, header, 1)?;
    check_header(path, header)?;
    let mut facts = Vec::new();
    for (index, line) in lines_iter.enumerate() {
        // `index` counts from the second line, so the 1-based line number is
        // index + 2. Saturating arithmetic is used on this file-derived count.
        let line_number = index.saturating_add(2);
        check_no_carriage_return(path, line, line_number)?;
        let fact: Fact = serde_json::from_slice(line).map_err(|source| {
            Error::json(
                format!("parsing {} line {}", path.display(), line_number),
                source,
            )
        })?;
        facts.push(fact);
    }
    Ok(facts)
}

/// Rejects a carriage return at the end of a line, which is where a `\r`
/// stood immediately before the `\n` the file was split on.
fn check_no_carriage_return(path: &Path, line: &[u8], line_number: usize) -> Result<()> {
    if line.last() == Some(&b'\r') {
        return Err(Error::Format {
            path: path.to_path_buf(),
            message: format!("line {line_number} has a carriage return before the newline"),
        });
    }
    Ok(())
}

/// Checks the header line: it must name the `arlineo-facts` format, version 1.
fn check_header(path: &Path, header: &[u8]) -> Result<()> {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(header).ok();
    let header_ok = match parsed {
        Some(value) => {
            let format_ok = match value.get("format") {
                Some(serde_json::Value::String(name)) => name == "arlineo-facts",
                _ => false,
            };
            let version_ok = match value.get("version") {
                Some(version) => version.as_u64() == Some(u64::from(FACTS_FORMAT_VERSION)),
                None => false,
            };
            format_ok && version_ok
        }
        None => false,
    };
    if header_ok {
        Ok(())
    } else {
        Err(Error::Format {
            path: path.to_path_buf(),
            message: "missing or foreign facts header, expected {\"format\":\"arlineo-facts\",\"version\":1}".to_owned(),
        })
    }
}

/// Writes the header line and the facts, in the given order, atomically
/// (`crate::store::write_atomic`).
pub fn write_facts(path: &Path, facts: &[Fact]) -> Result<()> {
    crate::store::write_atomic(path, &facts_bytes(facts)?)
}

/// The facts file bytes (header plus facts), as `write_facts` would write them.
pub fn facts_bytes(facts: &[Fact]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(HEADER.as_bytes());
    bytes.push(b'\n');
    for fact in facts {
        let line = serde_json::to_string(fact)
            .map_err(|source| Error::json(format!("serializing fact {}", fact.key), source))?;
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    Ok(bytes)
}
