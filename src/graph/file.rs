//! The binary graph file `graph.bin` (`SPEC.md`, "Graph file").

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{Confidence, Edge, EdgeKind, Graph, Node, NodeId, NodeKind, RelPath};
use crate::util::fnv;

/// Magic bytes at the start of the file.
pub const MAGIC: [u8; 8] = *b"ARLNGRPH";

/// Version of the graph file layout.
pub const GRAPH_FORMAT_VERSION: u32 = 1;

/// Minimum size of one string record: its u32 length (the bytes may be empty).
const STRING_RECORD_MIN: usize = 4;
/// Minimum size of one node record: kind, key index, file index and line.
const NODE_RECORD_MIN: usize = 13;
/// Minimum size of one edge record: from, to, kind, confidence and line.
const EDGE_RECORD_MIN: usize = 14;
/// Size of the trailing checksum.
const CHECKSUM_LEN: usize = 8;

/// Serializes a graph; `facts_hash` ties the file to the facts it was built from.
pub fn graph_bytes(graph: &Graph, facts_hash: &str) -> Result<Vec<u8>> {
    let mut strings: Vec<&str> = Vec::new();
    let mut indexes: HashMap<&str, u32> = HashMap::new();
    let mut node_strings: Vec<(u32, Option<u32>)> = Vec::new();
    for node in graph.nodes() {
        let key_index = intern_string(&mut strings, &mut indexes, node.key.as_str())?;
        let file_index = match &node.file {
            Some(path) => Some(intern_string(&mut strings, &mut indexes, path.as_str())?),
            None => None,
        };
        node_strings.push((key_index, file_index));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    push_u32(&mut out, GRAPH_FORMAT_VERSION);
    push_u32(&mut out, to_u32(facts_hash.len(), "facts hash length")?);
    out.extend_from_slice(facts_hash.as_bytes());

    push_u32(&mut out, to_u32(strings.len(), "graph string count")?);
    for string in &strings {
        push_u32(&mut out, to_u32(string.len(), "graph string length")?);
        out.extend_from_slice(string.as_bytes());
    }

    push_u32(&mut out, to_u32(graph.nodes().len(), "graph node count")?);
    for (node, node_string) in graph.nodes().iter().zip(node_strings.iter()) {
        out.push(node.kind.code());
        push_u32(&mut out, node_string.0);
        match node_string.1 {
            Some(index) => push_u32(&mut out, index),
            None => push_u32(&mut out, u32::MAX),
        }
        push_u32(&mut out, node.line);
    }

    push_u32(&mut out, to_u32(graph.edges().len(), "graph edge count")?);
    for edge in graph.edges() {
        push_u32(&mut out, edge.from.raw());
        push_u32(&mut out, edge.to.raw());
        out.push(edge.kind.code());
        out.push(confidence_code(edge.confidence));
        push_u32(&mut out, edge.line);
    }

    let checksum = fnv::hash(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

/// Writes `graph.bin` atomically.
pub fn write_graph(path: &Path, graph: &Graph, facts_hash: &str) -> Result<()> {
    let bytes = graph_bytes(graph, facts_hash)?;
    crate::store::write_atomic(path, &bytes)
}

/// Reads `graph.bin`. `Ok(None)` when the file is missing, has another magic or version,
/// another facts hash, a bad checksum, or is truncated: the caller then rebuilds the graph
/// from the facts. `Err` only when the file exists but cannot be read.
pub fn read_graph(path: &Path, facts_hash: &str) -> Result<Option<Graph>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(parse_graph(&bytes, facts_hash)),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io(
            format!("reading graph file {}", path.display()),
            source,
        )),
    }
}

/// Parses graph file bytes; same `None` cases as [`read_graph`].
pub fn parse_graph(bytes: &[u8], facts_hash: &str) -> Option<Graph> {
    let body_len = bytes.len().checked_sub(CHECKSUM_LEN)?;
    let (body, checksum_bytes) = bytes.split_at_checked(body_len)?;
    let mut checksum_cursor = Cursor::new(checksum_bytes);
    let stored_checksum = checksum_cursor.u64()?;
    if !checksum_cursor.is_empty() {
        return None;
    }

    let mut cursor = Cursor::new(body);
    if cursor.take(MAGIC.len())? != MAGIC.as_slice() {
        return None;
    }
    if cursor.u32()? != GRAPH_FORMAT_VERSION {
        return None;
    }
    let hash_len = usize::try_from(cursor.u32()?).ok()?;
    let hash_bytes = cursor.take(hash_len)?;
    let stored_hash = std::str::from_utf8(hash_bytes).ok()?;
    if stored_hash != facts_hash {
        return None;
    }
    // The checksum covers every byte before it; verify it before decoding the rest.
    if fnv::hash(body) != stored_checksum {
        return None;
    }

    let string_count_raw = cursor.u32()?;
    let string_count = record_count(&cursor, string_count_raw, STRING_RECORD_MIN)?;
    let mut strings: Vec<String> = Vec::with_capacity(string_count);
    for _ in 0..string_count {
        let len = usize::try_from(cursor.u32()?).ok()?;
        let text = std::str::from_utf8(cursor.take(len)?).ok()?;
        strings.push(text.to_owned());
    }

    let node_count_raw = cursor.u32()?;
    let node_count = record_count(&cursor, node_count_raw, NODE_RECORD_MIN)?;
    let mut nodes: Vec<Node> = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let kind = NodeKind::from_code(cursor.u8()?)?;
        let key_index = usize::try_from(cursor.u32()?).ok()?;
        let key = strings.get(key_index)?.clone();
        let file_index = cursor.u32()?;
        let file = if file_index == u32::MAX {
            None
        } else {
            let file_index = usize::try_from(file_index).ok()?;
            let text = strings.get(file_index)?;
            Some(RelPath::new(text).ok()?)
        };
        let line = cursor.u32()?;
        nodes.push(Node {
            key,
            kind,
            file,
            line,
        });
    }

    let edge_count_raw = cursor.u32()?;
    let edge_count = record_count(&cursor, edge_count_raw, EDGE_RECORD_MIN)?;
    let mut edges: Vec<Edge> = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        let from_index = usize::try_from(cursor.u32()?).ok()?;
        let to_index = usize::try_from(cursor.u32()?).ok()?;
        let kind = EdgeKind::from_code(cursor.u8()?)?;
        let confidence = confidence_from_code(cursor.u8()?)?;
        let line = cursor.u32()?;
        if from_index >= nodes.len() || to_index >= nodes.len() {
            return None;
        }
        let from = NodeId::from_index(from_index)?;
        let to = NodeId::from_index(to_index)?;
        edges.push(Edge {
            from,
            to,
            kind,
            confidence,
            line,
        });
    }

    if !cursor.is_empty() {
        return None;
    }
    Graph::from_parts(nodes, edges).ok()
}

/// Converts a count or length to `u32`; overflow is an internal error.
fn to_u32(value: usize, what: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::Internal {
        message: format!("{what} does not fit in u32: {value}"),
    })
}

/// Appends `value` as little-endian bytes.
fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Returns the index of `value` in the string table, adding it on first use.
fn intern_string<'a>(
    strings: &mut Vec<&'a str>,
    indexes: &mut HashMap<&'a str, u32>,
    value: &'a str,
) -> Result<u32> {
    if let Some(index) = indexes.get(value) {
        return Ok(*index);
    }
    let index = to_u32(strings.len(), "graph string count")?;
    strings.push(value);
    indexes.insert(value, index);
    Ok(index)
}

/// Byte code of a confidence level in the graph file.
fn confidence_code(confidence: Confidence) -> u8 {
    match confidence {
        Confidence::Exact => 0,
        Confidence::UniqueName => 1,
        Confidence::Heuristic => 2,
        Confidence::Guessed => 3,
    }
}

/// Confidence level for a byte code from the graph file.
fn confidence_from_code(code: u8) -> Option<Confidence> {
    match code {
        0 => Some(Confidence::Exact),
        1 => Some(Confidence::UniqueName),
        2 => Some(Confidence::Heuristic),
        3 => Some(Confidence::Guessed),
        _ => None,
    }
}

/// Checks that `count` records of at least `min_record` bytes fit in the bytes left,
/// and returns the count as `usize`.
fn record_count(cursor: &Cursor<'_>, count: u32, min_record: usize) -> Option<usize> {
    let count = usize::try_from(count).ok()?;
    let needed = count.checked_mul(min_record)?;
    if needed > cursor.remaining() {
        None
    } else {
        Some(count)
    }
}

/// A cursor over bytes being parsed; every read is bounds-checked.
struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { rest: bytes }
    }

    fn remaining(&self) -> usize {
        self.rest.len()
    }

    fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.rest.split_at_checked(len)?;
        self.rest = rest;
        Some(taken)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(u64::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::{graph_bytes, parse_graph};
    use crate::extract::test_support::facts;
    use crate::graph::build::build;

    #[test]
    fn roundtrip_and_rejections() {
        let all = facts("src/lib.rs", "pub fn a() { b(); }\npub fn b() {}\n");
        let graph = build(&all).expect("graph");
        let bytes = graph_bytes(&graph, "hash-1").expect("bytes");
        let parsed = parse_graph(&bytes, "hash-1").expect("same facts hash parses");
        assert_eq!(parsed.nodes(), graph.nodes());
        assert_eq!(parsed.edges(), graph.edges());
        assert!(parse_graph(&bytes, "hash-2").is_none(), "other facts hash");
        let mut damaged = bytes.clone();
        if let Some(last) = damaged.last_mut() {
            *last ^= 0xff;
        }
        assert!(parse_graph(&damaged, "hash-1").is_none(), "bad checksum");
        assert!(
            parse_graph(&bytes[..bytes.len() / 2], "hash-1").is_none(),
            "truncated"
        );
    }
}
