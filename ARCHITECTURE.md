# Arlineo architecture

This document describes how Arlineo is built, following `SPEC.md`. Arlineo is one Rust binary with no background service. Every command runs the same pipeline: read the repository through git, load the wanted source files, lex and extract facts, build a graph, store the result outside the repository, and answer questions from a view (the line alone, or the line with one worktree's changed files swapped in).

```text
 git ------- commits, trees, blobs, worktree state, diffs
  |
 sources ---- wanted files from blobs (base index) or disk (overlay)
  |
 lexers ----- tokens (Rust, PostgreSQL, TypeScript/JavaScript)
  |
 extractors - facts per file (definitions, calls, routes, tables, ...)
  |
 facts ------ facts.jsonl per commit, sorted, deduplicated
  |
 graph ------ nodes and edges with confidence, graph.bin per commit
  |
 store ------ <state>/arlineo/<repo-id>/ : meta.json, lock, base/, overlay/
  |
 overlays --- facts of files a worktree changed since its merge-base
  |
 views ------ base index, or base index with overlay files swapped in
  |
 queries ---- status, index, find, table, route, fn, graph, impact,
              conflicts, check, diff
```

The stages below follow that order. Each stage names its module from the module map.

## Git

Module: `src/git.rs`. Arlineo reads the repository only through the external `git` program; it never writes into a repository or worktree. Every call is `git -C <dir> -c core.fsmonitor=false` with arguments as separate process arguments (never a shell), environment `LC_ALL=C`, `GIT_OPTIONAL_LOCKS=0` and `GIT_NO_LAZY_FETCH=1`, inherited variables that would redirect git (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and the like) removed, and stdin closed except for `cat-file --batch`. The fixed subcommands are `rev-parse --show-toplevel`, `rev-parse --git-common-dir`, `rev-parse --verify --quiet <rev>^{commit}`, `merge-base`, `ls-tree -r -l -z`, `diff --name-status -z --no-renames`, `status --porcelain=v1 -z --untracked-files=all --no-renames`, `worktree list --porcelain -z`, and `cat-file --batch`. Paths from git that are not UTF-8 or not valid repository-relative paths are skipped and counted, not guessed at.

## Sources

Module: `src/source.rs`. A file is read only when the configuration accepts it (`Config::wants`: a known language, matched by `include`, not matched by `exclude`, and not a secret file). Secret files (section "Security notes" in `README.md`) are never read. Files larger than `MAX_FILE_BYTES` (4 MiB), files containing a NUL byte, and files that are not valid UTF-8 are skipped with a reason; a UTF-8 byte order mark is removed. Base-index files come from git blobs through one `cat-file --batch` process; overlay files are read from disk at the worktree top. On disk, symlinks are never followed, and size and modification time are read before and after reading; if they differ, the file is read again, at most 3 times, then skipped as changing while read. A skipped file produces one `unparsed` fact at line 0 with the reason, and counts in `BaseIndex::skipped`.

## Lexers

Modules: `src/lex/mod.rs` (shared token types) and `src/lex/{rust,sql,ts}.rs`. There is one hand-written lexer per language. A lexer never fails and never panics: it returns tokens with byte spans and the 1-based line of the first byte, skipping whitespace and comments, and records problems as `LexError` values and continues (an unterminated string, comment or template ends at end of input as one token; a byte that starts no token becomes a one-character `Punct`). The Rust lexer handles raw identifiers and strings, lifetimes versus character literals, nested block comments, and number forms such as `1..2` and tuple field access. The SQL lexer handles quoted identifiers, `E'...'` and dollar-quoted strings as single tokens, positional parameters such as `$1`, and nested block comments. The TypeScript/JavaScript lexer handles template literals with nested `${ ... }`, regular expression literals (decided from the previous significant token), `#private` names, and, for `.tsx` and `.jsx` files, JSX. Every lexer error becomes an `unparsed` fact for the file.

## Extractors

Modules: `src/extract/mod.rs` and `src/extract/{rust,sql,ts}.rs`. Extractors work on tokens with brace, bracket and parenthesis matching, and recognize only the constructs listed in `SPEC.md` section 9. Per file they produce facts such as `module`, `fn_def`, `type_def`, `impl`, `import`, `call`, `route`, `api_call`, `env_var`, `sql_in_code`, and, for SQL files, `table`, `column`, `grant`, `trigger`, `sql_function` and `table_access`. Keys follow `SPEC.md` section 7 (for example a Rust function key is `<module key>::<name>`, a table key is the name with a leading `public.` schema removed). Definition hashes are FNV-1a 64 of the item's token texts joined with single spaces, so formatting changes do not change a hash. Anything an extractor cannot match where it expected a construct becomes an `unparsed` fact with a short reason; the extractor skips to the next `;` or closing brace at the same depth and continues instead of stopping at the first problem. SQL in string or template literals is recognized heuristically and yields `sql_in_code` and `table_access` facts with `heuristic` confidence. `extract` adds exactly one `file` fact per file and one `unparsed` fact per lexer error before the language extractor runs.

## Facts

Modules: `src/model.rs` (fact types and ordering) and `src/util/jsonl.rs` (the facts file). A fact has a `kind`, a `key`, a repository-relative `file`, a 1-based `line` (0 for whole-file facts), kind-specific `data`, and a `confidence` (`exact` for syntax, `heuristic` for pattern-based facts such as routes, API calls and SQL in strings). Facts are the only thing the later stages consume: queries, the graph and the rules all read facts, never source text. Facts have a total order (`model::compare_facts`: file, line, kind, key, data, confidence); a facts list is sorted in that order with exact duplicates removed.

## Graph

Modules: `src/graph/build.rs` (construction), `src/graph/file.rs` (the `graph.bin` format), and the graph types in `src/model.rs`. There is one node per `fn_def`, `type_def`, `module`, `sql_function`, `table` key and `route` key. Edges are `contains`, `calls`, `handles`, `requests`, `reads`, `writes`, `triggers` and `imports`. Call resolution tries, in order, `self`/`Self` methods, qualified paths resolved through `crate::`, `self::`, `super::`, imports, crate names and `Type::name`, bare names (a nested function, then the same module, then an import), and TypeScript imports resolved against the importing file. What remains is resolved by name: exactly one definition with that name gives a `unique_name` edge, 2 to 8 candidates give one `guessed` edge each, and more give no edge. Macros, `new Name` calls and common standard library method names never get `unique_name` or `guessed` edges. Every edge carries a confidence; edges are deduplicated on `(from, to, kind)` keeping the strongest confidence and the lowest line, and ordered by `from`, `kind`, `to`. Nodes are ordered by kind, then key, so the same facts always give the same graph.

## Store

Module: `src/store.rs`, with `src/util/fnv.rs` for hashing. The store is a cache outside the repository (see "Where the index lives" in `README.md`). All writes go through `store::write_atomic` (a temporary file in the same directory, flushed and synced, then renamed over the target), so readers never see a partial file and do not lock. Writers hold the store lock: the `lock` file is created with `create_new` and holds the process id and Unix time; on contention a writer retries every 100 ms for up to 30 s, and a lock file older than 10 minutes whose holder process is gone is replaced; a lock is removed on drop only while it still holds its owner's contents. A missing, unreadable or foreign-version `meta.json` (format version or tool version) is replaced by a fresh one under the lock; after every `meta.json` write, base directories of commits it does not list are removed, best effort. On Unix the store directories are created with mode 0700.

## Overlays

Module: `src/overlay.rs`. An active worktree is a worktree from `git worktree list` that is not bare, not prunable and has a HEAD. For each active worktree, `overlay_for` computes the merge-base of its HEAD and the line commit (unrelated histories are an error, `Error::NotFound`), combines `diff_names(merge_base, HEAD)` with the worktree's `status()` per path (the last state wins; a file deleted on disk is deleted), extracts facts of the changed paths from disk, and keeps the facts of the same files at the merge-base as the `before` set. Results are cached in the overlay directory and reused per path while size, modification time and merge-base are unchanged and the configuration hash matches; a file modified in the last 2 seconds is read again, and a file that changed while being read is not cached. Overlay directories of removed worktrees are cleaned up by `status`. Comparing `before` and the current overlay facts by `(kind, key)` for definition kinds gives the touched definitions (added, removed, changed); `overlay::touched_definitions` is shared by `conflicts` and `diff`; added `.sql` files also count as an added `file` definition.

## Views

Module: `src/overlay.rs` (`View`, `base_view`, `worktree_view`). A view is what a question is answered from: the base index of the line commit alone, or that index with one worktree's changed files swapped in (base facts whose file is a changed path are removed, the overlay facts are added, the list is sorted and the graph is built). The worktree view's graph is cached in `overlay/<worktree-id>/view-graph.bin` under a key over the base facts hash, the changed paths and the overlay facts, so repeated questions about an unchanged worktree do not rebuild it. A `View` bundles the facts, a `FactIndex` (facts grouped by kind) and the graph. The line view that `check` subtracts is simply the base index of the line's current commit. Because a view is rebuilt from facts, worktree answers include uncommitted changes exactly as they are on disk at question time.

## Queries

Modules: `src/query/*.rs`, `src/conflicts.rs`, `src/diff.rs`, `src/check.rs`, `src/output.rs` and `src/cli.rs`. Each command builds the view it needs, then produces a `Report` with text lines and a JSON value, plus a `ReportStatus` for the exit code. `find`, `table`, `route`, `fn`, `graph` and `impact` are read-only lookups over facts and graph edges. `conflicts` compares the overlays of all active worktrees pairwise (the worktree whose HEAD equals the line commit with no changes is left out). `check` computes rule hits (see `RULES.md`) in the worktree view and subtracts the hits of the line view. `diff` compares base indexes of branch commits against their merge-bases with the line, then reports collisions between the branches and with the line's own changes since each merge-base. Output renders the stamp first (see "How freshness works" in `README.md`), then the report lines; paths in output are repository-relative. Errors go to stderr as `arlineo: error: <message>` with `caused by:` lines when the error has a cause; with `--json`, errors are printed to stdout as `{"error": {"kind": "...", "message": "..."}}`.

## Module map

From `SPEC.md` section 3:

| File | Content |
|---|---|
| `src/main.rs` | Entry point; collects the process arguments and calls `cli::run`, whose return value is the exit code. |
| `src/error.rs` | `Error` and `Result`: the single error type of the binary. |
| `src/model.rs` | Paths (`RelPath`, `CommitId`, `WorktreeId`), ids, facts and fact data, `norm_path`, graph types, `FactIndex`. |
| `src/util/fnv.rs` | FNV-1a 64 hashing (content hashes, checksums, ids; not cryptographic). |
| `src/util/glob.rs` | Globs (`SPEC.md` section 11.3). |
| `src/util/jsonl.rs` | The facts file (`facts.jsonl`). |
| `src/lex/{rust,sql,ts}.rs` | The three lexers; shared token types in `src/lex/mod.rs`. |
| `src/extract/{rust,sql,ts}.rs` | The three extractors; shared entry point in `src/extract/mod.rs`. |
| `src/config.rs` | `arlineo.json`: file selection, patterns and rules (`SPEC.md` section 11). |
| `src/git.rs` | The git runner: fixed `git` subcommands with separate arguments. |
| `src/source.rs` | Loading files from blobs and from disk, with skip reasons. |
| `src/store.rs` | The state directory: layout, `meta.json`, the lock, atomic writes. |
| `src/index.rs` | Base indexes: resolving the line, building, updating and loading an index of one commit. |
| `src/overlay.rs` | Worktree overlays and views. |
| `src/graph/build.rs` | Graph construction and call resolution. |
| `src/graph/file.rs` | `graph.bin`, the binary graph file. |
| `src/query/*.rs` | The query commands `find`, `table`, `route`, `fn`, `graph`, `impact`. |
| `src/conflicts.rs`, `src/diff.rs`, `src/check.rs` | The `conflicts`, `diff` and `check` commands. |
| `src/output.rs` | The freshness stamp and text/JSON rendering of reports and errors. |
| `src/cli.rs` | Argument parsing and dispatch; owns the exit codes. |

## File formats

### Facts file: `facts.jsonl`

UTF-8 JSON Lines. The first line is the header `{"format":"arlineo-facts","version":1}`; every other line is one fact serialized with `serde_json::to_string` (no spaces), in `model::compare_facts` order, without duplicates, each line ending with `\n`. Readers reject a missing header, another format name or another version with `Error::Format`, and a bad line with `Error::Json` whose context names the line number.

### Graph file: `graph.bin`

A binary file; all integers are little-endian:

| Part | Content |
|---|---|
| Magic | 8 bytes `ARLNGRPH`. |
| Version | `u32`, `GRAPH_FORMAT_VERSION` (1). |
| Facts hash | `u32` length + UTF-8 bytes: the 16-hex-digit FNV-1a 64 of `facts.jsonl` this graph was built from. |
| Strings | `u32` count, then per string a `u32` length + UTF-8 bytes; each distinct key and path once, in first-use order. |
| Nodes | `u32` count, then per node: `u8` kind code, `u32` key string index, `u32` file string index (or `u32::MAX` for none), `u32` line. |
| Edges | `u32` count, then per edge: `u32` from, `u32` to, `u8` kind code, `u8` confidence code (0 `exact`, 1 `unique_name`, 2 `heuristic`, 3 `guessed`), `u32` line. |
| Checksum | `u64` FNV-1a 64 of every preceding byte. |

Readers check every length against the remaining bytes before allocating, reject unknown codes, out-of-range indexes and trailing bytes, and verify the checksum before building the graph. Any problem means the file is treated as absent (`None`) and the caller rebuilds the graph from the facts and rewrites the file; only a file that exists but cannot be read at all is an error.

### Store layout

```text
<state>/arlineo/<repo-id>/
  meta.json
  lock
  base/<commit>/facts.jsonl
  base/<commit>/graph.bin
  overlay/<worktree-id>/facts.jsonl     facts of changed files on disk
  overlay/<worktree-id>/before.jsonl     facts of the same files at the merge-base
  overlay/<worktree-id>/cache.json       per path: size, mtime (ns), hash; merge-base; config hash
  overlay/<worktree-id>/view-graph.bin   cached graph of the worktree view
```

`meta.json` records the store format version, the tool version, and the indexed base commits newest first, each with its commit, the Unix time it was indexed, the configuration hash used, and the FNV-1a 64 hex of its `facts.jsonl`. At most the 5 newest base indexes are kept.

## Determinism

- The same commit and configuration give byte-identical facts files on every machine: facts are sorted by `model::compare_facts` and deduplicated, and definition hashes ignore formatting.
- Graph nodes are ordered by kind (`NodeKind::ALL` order), then key; edges are deduplicated and ordered by `from`, `kind`, `to`; the graph file's string table is in first-use order.
- Output order is always sorted or in facts order; it never depends on `HashMap` iteration. There is no global mutable state and there are no threads.
- Configuration content that changes extraction is part of the index identity: the configuration hash (FNV-1a 64 hex of a canonical JSON text of `include`, `exclude`, `routes` and `api_clients`, without `note` fields) is stored per index, and an index built with another hash is rebuilt. The tool version is stored in `meta.json` as well, so a new Arlineo version rebuilds the indexes.

## Error handling

- Fallible functions return `crate::error::Result`. Errors keep the context of the operation and, where there is one, the underlying cause (`Error::io`, `Error::json`, `Error::Git`), reachable through `Error::source`; error text is never parsed.
- Exit codes: 0 success, 1 error, 2 usage error (unknown options, missing values, extra arguments, non-UTF-8 arguments; a one-line message plus the command's usage), 3 when `check` finds new violations.
- Lexers never fail; lexer and extractor problems become `unparsed` facts with reasons. Skipped files become `unparsed` facts and are counted. Unknown git paths are skipped and counted. Ignoring a failure is allowed only where `SPEC.md` says the failure is skipped and counted.
- A corrupt stored file is not fatal where the store can recover: a bad `graph.bin` is rebuilt from the facts, and a missing, unreadable or foreign-version `meta.json` is replaced under the lock. A bad `facts.jsonl` line is an error, because facts are the input everything else is rebuilt from.
- No `unsafe` code; no `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` or `unreachable!` in the implementation; indexing that can go out of bounds is not used, and arithmetic on data from files, git or the command line is checked or saturating, or a plain operation guarded by a comparison just before it (for example `remaining -= want` after `want` is limited to `remaining`). Integers from files are checked against the remaining bytes before they size an allocation (see the graph file reader above).

## Tests

`cargo test` runs unit tests. They sit at the end of the modules they test (`#[cfg(test)] mod tests`): lexers, extractors (through `extract::test_support`, which extracts one in-memory file with the default configuration or a given one), path normalization, globs, FNV, graph building and the graph file. Test code may use `expect` and assertions.
