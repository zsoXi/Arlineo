# Arlineo specification

Arlineo is a code map for teams of humans and AI coding agents that work in many git
worktrees of one repository at the same time. It reads the repository through `git`,
extracts facts from Rust, PostgreSQL and TypeScript/JavaScript sources with hand-written
lexers, builds a call graph with a confidence level on every edge, and answers questions:
where something is, who calls it, what a change can reach, whether two worktrees change the
same thing, and whether a change breaks the project's own rules.

This document is the contract for every module. Shared types are real code in
`src/model.rs`, `src/error.rs`, `src/lex/mod.rs`, `src/extract/mod.rs`, `src/config.rs`
and `src/output.rs`. Every other module has its public signatures fixed in its file. Do not
change a public signature or a shared type; if one is wrong, say so in your report.

## 1. Principles

1. **Read only.** Arlineo never writes into a repository or worktree it reads. Its index
   lives in a state directory outside the repository (section 15).
2. **Deterministic.** The same commit and configuration give byte-identical facts files on
   every machine.
3. **Fresh by construction.** There is no background service. Every command checks the
   current state of git and the worktrees and updates what changed before answering. Every
   answer starts with a stamp that says what it is based on.
4. **Honest about uncertainty.** Every graph edge has a confidence level. Anything a lexer or
   extractor does not understand becomes an `unparsed` fact with a reason; nothing is dropped
   silently.
5. **No surprises.** No network access. No dependencies beyond `serde` and `serde_json`. No
   `unsafe`. Git is an external program. Configuration is data, never code.
6. **Relative paths.** Facts, reports and errors name repository files by their path relative
   to the repository root. Absolute paths appear only where unavoidable (worktree locations in
   `status`).

## 2. Code rules

1. Rust edition 2024, standard library plus `serde` (with `derive`) and `serde_json`.
2. Fallible functions return `crate::error::Result`. Keep the context of the operation and the
   underlying error (`Error::io`, `Error::json`, `Error::Git`). Never parse error text.
3. No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, indexing that can go out of
   bounds, or unchecked arithmetic on data from files, git or the command line. Use `get`,
   `checked_*`, `saturating_*` and `try_from`. Integer casts with `as` only where the range is
   proven in a comment.
4. Ignoring a `Result` (`.ok()`, `let _ =`, `filter_map(Result::ok)`) is allowed only where this
   spec says the failure is skipped and counted.
5. No global mutable state, no threads, no `unsafe`.
6. Output order is always sorted or in facts order; never depends on `HashMap` iteration.
7. Doc comments on public items; short comments only where the code is not obvious.
8. English only, in code, comments, messages and documents. ASCII only in source files,
   except test data that checks Unicode handling.

## 3. Modules

| File | Content |
|---|---|
| `src/main.rs` | entry point, calls `cli::run` |
| `src/error.rs` | `Error`, `Result` |
| `src/model.rs` | paths, ids, facts, fact data, `norm_path`, graph types, `FactIndex` |
| `src/util/fnv.rs` | FNV-1a 64 |
| `src/util/glob.rs` | globs (section 11.3) |
| `src/util/jsonl.rs` | facts file (section 6.1) |
| `src/lex/{rust,sql,ts}.rs` | lexers (section 8) |
| `src/extract/{rust,sql,ts}.rs` | extractors (section 9) |
| `src/config.rs` | `arlineo.json` (section 11) |
| `src/git.rs` | git runner (section 14) |
| `src/source.rs` | loading files (section 13) |
| `src/store.rs` | state directory (section 15) |
| `src/index.rs` | base indexes (section 16) |
| `src/overlay.rs` | worktree overlays and views (section 17) |
| `src/graph/build.rs` | graph construction (section 12) |
| `src/graph/file.rs` | `graph.bin` (section 12.5) |
| `src/query/*.rs` | `find`, `table`, `route`, `fn`, `graph`, `impact` (section 18) |
| `src/conflicts.rs`, `src/diff.rs`, `src/check.rs` | section 18 |
| `src/output.rs` | stamp, text and JSON output (section 19) |
| `src/cli.rs` | argument parsing and dispatch (section 5) |

## 4. Terms

- **Line**: the base branch the team integrates into (for example `main`).
- **Base index**: facts and graph of one commit of the line.
- **Worktree**: any `git worktree` of the repository, including the main one.
- **Merge-base**: `git merge-base <worktree HEAD> <line commit>`.
- **Overlay**: facts of the files a worktree changed since its merge-base.
- **View**: the facts and graph a question is answered from: the line alone, or the line with
  one worktree's changed files swapped in.

## 5. Command line

```
arlineo <command> [options] [arguments]

Commands:
  status                              base, store and worktree state
  index [REV]                         build or update the index of REV (default: the line)
  find [--kind K] [--where F=GLOB]... facts matching conditions
  table NAME                          one table
  route PATH                          routes and the calls that reach them
  fn NAME                             one function: callers, callees, facts inside
  graph FROM [--up N] [--down N]      walk the graph (defaults: --up 1 --down 1)
  impact TARGET [--depth N]           what a change can reach (default depth 5)
  conflicts                           collisions between active worktrees
  check [--worktree W]                new rule violations against the line
  diff BRANCH...                      what branches change and where they collide
  help [COMMAND]                      usage

Common options:
  --worktree W   answer from the view of worktree W (path, directory name or branch)
  --base REF     use REF as the line instead of the configured one
  --json         JSON output
  --limit N      at most N results (default 200)
```

1. Options may appear anywhere after the command. `--where` may repeat. Unknown options,
   missing values, extra arguments and non-UTF-8 arguments are usage errors (exit 2) with a
   one-line message and the command's usage.
2. `F` in `--where F=GLOB` is `key`, `file`, `line`, or a data field name.
3. Exit codes: 0 success, 1 error, 2 usage error, 3 `check` found new violations.
4. Text output goes to stdout. Errors go to stderr as `arlineo: error: <message>` plus, for
   errors with a cause, `  caused by: <cause>` lines. With `--json`, errors are printed to
   stdout as `{"error": {"kind": "...", "message": "..."}}`.
5. Every command except `help` runs in the current directory's repository: `git rev-parse
   --show-toplevel` from the current directory.

## 6. Facts

### 6.1 Facts file

`facts.jsonl` is UTF-8 JSON Lines. The first line is the header
`{"format":"arlineo-facts","version":1}`; every other line is one `Fact` as serialized by
`serde_json::to_string` (no spaces), in `model::compare_facts` order, without duplicates, each
line ending with `\n`. Readers reject a missing header, another format name or another version
with `Error::Format`, and a bad line with `Error::Json` whose context names the line number.

### 6.2 Fact fields

| Field | Meaning |
|---|---|
| `kind` | `FactKind` |
| `key` | identity of the thing the fact is about (section 7) |
| `file` | repository-relative path |
| `line` | 1-based line of the fact; 0 for whole-file facts |
| `data` | kind-specific fields, the `*Data` structs in `model.rs` |
| `confidence` | `exact` for syntax, `heuristic` for pattern-based facts (routes, API calls, SQL in strings) |

### 6.3 Kinds

| Kind | Key | Line | Data |
|---|---|---|---|
| `file` | the path | 0 | `FileData` |
| `unparsed` | the path | where the problem is, 0 for the whole file | `UnparsedData` |
| `module` | module key | 0 | `ModuleData` (last segment) |
| `fn_def` | function key | `fn`/`function` keyword or name | `FnDefData` |
| `type_def` | type key | keyword | `TypeDefData` |
| `impl` | `<module key>::impl <Type>` or `<module key>::impl <Trait> for <Type>` | `impl` keyword | `ImplData` |
| `import` | imported path joined with `::` (Rust) or `<specifier>#<name>` (TS) | statement | `ImportData` |
| `call` | callee as written (`CallData::callee`) | call site | `CallData` |
| `route` | `<METHOD> <norm_path>` | registration | `RouteData` |
| `api_call` | `<METHOD> <norm_path>` | call site | `ApiCallData` |
| `env_var` | variable name | access | `EnvVarData` |
| `sql_in_code` | owner key | string literal | `SqlInCodeData` |
| `table` | table key | statement | `TableData` |
| `column` | `<table key>.<column>` | statement or column line | `ColumnData` |
| `grant` | `<PRIVILEGE>[(<column>)] <object> <role>` (revokes: prefixed `REVOKE `) | statement | `GrantData` |
| `trigger` | `<table key>.<trigger name>` | statement | `TriggerData` |
| `sql_function` | `sql:<function name>` | statement | `SqlFunctionData` |
| `table_access` | table key | statement or string literal | `TableAccessData` |
| `const_def` | `<scope key>::<NAME>` like a function key (Rust `const` and `static` items) | `const`/`static` keyword | `ConstDefData` |

`hash` fields (`FileData`, `FnDefData`, `TypeDefData`, `ConstDefData`, `SqlFunctionData`) are FNV-1a 64 as 16
hex digits. For definitions it is the hash of the item's token texts from its first token to
its last, joined with single spaces, so formatting changes do not change it. For `file` it is
the hash of the file's bytes after a leading UTF-8 byte order mark is removed.

## 7. Keys

### 7.1 Rust

1. A file belongs to the crate whose directory is the longest prefix of the file path among
   crates found by `index::crate_roots`. Within the crate directory, files under `src/`,
   `tests/`, `examples/` and `benches/` have module paths:
   - `src/lib.rs`, `src/main.rs` → `""` (crate root);
   - `src/a.rs`, `src/a/mod.rs` → `a`; `src/a/b.rs` → `a::b`;
   - `src/bin/x.rs`, `src/bin/x/main.rs` → `bin::x`;
   - `tests/x.rs` → `tests::x`, `examples/x.rs` → `examples::x`, `benches/x.rs` → `benches::x`.
   Files outside any crate get the module path made from their repository path without
   extension, `/` replaced by `::`, under the crate name `_`.
2. Module key: `<crate>` for the root, else `<crate>::<module path>`. Inline modules (`mod x
   { ... }`) extend the path.
3. Function key: `<module key>::<name>`. Methods in `impl Type` or `impl Trait for Type`:
   `<module key>::<Type>::<name>` where `Type` is the last path segment of the self type
   without generics (`impl<T> Foo<T>` → `Foo`). Trait methods with a default body:
   `<module key>::<Trait>::<name>`. Functions nested inside functions:
   `<outer key>::<name>`.
4. Type key: `<module key>::<name>`.

### 7.2 TypeScript and JavaScript

1. Module key: the repository path, e.g. `web/src/api/orders.ts`.
2. Function key: `<module key>::<name>`; class methods `<module key>::<Class>.<name>`;
   functions nested in functions `<outer key>::<name>`. Named functions are function
   declarations, `const`/`let`/`var` bindings initialized with an arrow function or function
   expression, and class methods (including getters, setters and constructors as
   `constructor`). Default-exported anonymous functions are named `default`.
3. Type key: `<module key>::<name>` for classes, interfaces, type aliases and enums.

### 7.3 SQL

1. Table key: the name as written, unquoted identifiers lowercased, quoted identifiers with
   quotes removed and case kept, a leading `public.` schema removed. Other schemas stay
   (`audit.events`).
2. SQL function key: `sql:<name>` with the same name rules.
3. Owners: statements inside a function body have the function key; statements inside a `DO`
   block have the owner `<file>#do@<line>`; top-level statements have the file path.

### 7.4 Routes and API calls

`<METHOD> <norm_path>` where `METHOD` is uppercase or `ANY`, and `norm_path` is
`model::norm_path` of the path after applying the pattern's `prefix` or `strip_prefix`, or
`model::norm_path_in` with the pattern's `base_path` when it has one (the base path is put in
front unless the normalized path already starts with it as whole segments).

## 8. Lexers

All lexers: `lex(&str) -> Lexed` never fails and never panics. Tokens carry byte spans and
the 1-based line of their first byte. Whitespace and comments are skipped. On a problem the
lexer records a `LexError` and continues: an unterminated string, comment or template ends at
end of input as one token; a byte that starts no token becomes a one-character `Punct`.

### 8.1 Rust

- Identifiers: Unicode-aware like `rustc` (start with `_` or alphabetic, continue with `_` or
  alphanumeric); raw identifiers `r#name` are one `Ident` token.
- Lifetimes and labels `'a` (`Lifetime`) versus character literals `'a'`, `'\n'`, `'\u{1F600}'`
  (`Char`), byte literals `b'a'` (`Char`).
- Strings (`Str`): `"..."` with escapes, `b"..."`, `c"..."`, raw `r"..."`, `r#"..."#` with any
  number of `#`, `br#"..."#`, `cr#"..."#`.
- Comments: `//` to end of line; `/* */` nested.
- Numbers: decimal, `0x`, `0o`, `0b`, `_` separators, suffixes, floats with exponent; `1..2`
  is `1`, `..`, `2`; `x.0.1` tuple access is `x`, `.`, `0`, `.`, `1` (a number directly after
  `.` never contains a `.`).
- Multi-character punctuation: `::` `->` `=>` `==` `!=` `<=` `>=` `&&` `||` `+=` `-=` `*=`
  `/=` `%=` `^=` `&=` `|=` `<<=` `>>=` `..=` `...` `..`. `<<` and `>>` are two tokens each (so
  generics close correctly). Everything else is one character.
- A `#!` shebang on the first line is skipped; `#![...]` is not a shebang.

### 8.2 SQL (PostgreSQL)

- Identifiers: letters, digits, `_`, `$` (not first). Keywords are identifiers.
- Quoted identifiers `"..."` with `""` escapes (`QuotedIdent`).
- Strings `'...'` with `''` escapes; `E'...'` with backslash escapes; `U&'...'`; `B'...'`,
  `X'...'` (`Str`).
- Dollar-quoted strings `$$...$$` and `$tag$...$tag$` (tag: identifier characters) are one
  `DollarStr` token. A positional parameter `$1` is one `Punct` token with text `$1`.
- Comments: `--` to end of line; `/* */` nested.
- Numbers including decimals and exponents.
- Multi-character punctuation: `::` `<=` `>=` `<>` `!=` `||` `->` `->>` `#>` `#>>` `:=` `=>`.

### 8.3 TypeScript and JavaScript

- Identifiers including `$` and Unicode letters; `#private` names are one `Ident`.
- Strings `'...'`, `"..."` with escapes and line continuations.
- Template literals: one `Template` token from the opening to the closing backtick, tracking
  nested `${ ... }` with brace depth, strings, comments and nested templates inside.
- Regular expression literals: a `/` starts a regex when the previous significant token is
  none, a `Punct` other than `)`, `]`, `}`, or an `Ident` that is one of `return typeof
  instanceof in of new delete void throw case do else yield await`. A regex extends to the
  closing unescaped `/` outside a character class, plus flags.
- Comments `//` and `/* */` (not nested). Hashbang on the first line is skipped.
- Numbers: decimal, hex, octal, binary, `_` separators, `n` suffix, exponent.
- Multi-character punctuation: `===` `!==` `**=` `...` `=>` `==` `!=` `<=` `>=` `&&` `||`
  `??` `?.` `++` `--` `+=` `-=` `*=` `/=` `%=` `&=` `|=` `^=` `<<=` `>>=` `>>>=` `**` `<<` `&&=`
  `||=` `??=`. `>>` and `>>>` are separate `>` tokens (so generics close correctly).
- JSX (when enabled): after `<` in expression position followed by an identifier or `>`,
  text between tags is skipped until the next `<` or `{`; attribute strings are `Str` (no
  escapes, may span lines); `{ ... }` inside JSX is lexed as code. `<T,>` and
  `<T extends ...>` are generic parameters of an arrow function, not JSX. A failure to follow
  JSX anywhere inside a top-level JSX element, including JSX nested more than 256 levels
  deep, records one `LexError` and lexes that whole element as plain code from after its `<`.

## 9. Extraction

Extractors work on tokens with brace, bracket and parenthesis matching. They recognize the
constructs below and nothing else; anything they cannot match where they expected a
construct becomes an `unparsed` fact with a short reason. They must not stop at the first
problem: they skip to the next `;` or closing brace at the same depth and continue.

### 9.1 Rust

1. One `module` fact per file (key from 7.1) and per inline `mod x { }`.
2. `fn_def` for `fn` items at module level, in `impl` blocks, in `trait` blocks with a body,
   and nested in function bodies. `is_pub` for any `pub` visibility; `is_async` for `async
   fn`. `end_line` is the line of the closing brace (or `;` for bodiless trait functions,
   which are not facts). `owner_type` and `trait_name` from the enclosing `impl`/`trait`.
3. `type_def` for `struct`, `enum`, `union`, `trait`, `type` aliases at module level
   (associated types `type X = ...;` inside `impl` and `trait` are not type definitions).
4. `impl` facts for `impl` blocks.
5. `import` facts for `use` trees, flattened: `use a::{b, c::d as e, f::*};` gives `a::b`,
   `a::c::d` (alias `e`), `a::f::*` (name `*`). `ImportData::path` is the path without the last
   segment, `name` the last segment. `self` in a group refers to the group path itself.
6. `call` facts for every call expression inside function bodies, and at module level
   inside `const`/`static` initializers (owner: module key):
   - `name(...)`: `callee` = `name`, no qualifier.
   - `a::b::name(...)` and `Type::name(...)`, including turbofish `name::<T>(...)` (generics
     dropped from `callee`): qualifier `["a","b"]`.
   - `recv.name(...)`, including chained calls: `is_method`, `receiver` = `self` or a single
     identifier when the receiver is exactly that, else none.
   - `name!(...)`, `name![...]`, `name!{...}`: `is_macro`. Calls inside macro arguments are
     also extracted when the macro arguments are code-like (`format!`, `vec!`, `println!`,
     ...): extract calls from every macro's token tree.
   - Closure bodies belong to the enclosing function.
7. `route` facts by route patterns with `lang: rust` (section 11.2).
8. `env_var` facts: `env::var("X")`, `std::env::var("X")`, `env::var_os("X")`,
   `std::env::var_os("X")`, `env!("X")`, `option_env!("X")`; key = `X`. Only string literal
   arguments.
9. SQL in strings: every string literal inside a function body whose value
   `extract::sql::looks_like_sql` gives one `sql_in_code` fact (confidence `heuristic`, preview
   from `extract::sql::sql_preview`: `'...'` literals replaced by `'?'`, whitespace collapsed,
   at most 120 characters) and one
   `table_access` fact (confidence `heuristic`) per `(table, op)` from
   `extract::sql::sql_text_accesses`, owner = enclosing function key.

### 9.2 SQL

1. Statements are split at `;` tokens. Statement keywords compare case-insensitively.
2. `CREATE [UNLOGGED|TEMP|TEMPORARY] TABLE [IF NOT EXISTS] name (...)`: `table` (create,
   object `table`, `end_line` = closing parenthesis) and one `column` per column definition
   (`sql_type` = the type tokens up to the first constraint keyword, joined by spaces;
   `sequence` = `<table>_<column>_seq` for a `serial` type or `GENERATED ... AS IDENTITY`,
   also in `ALTER TABLE ... ADD COLUMN`); table constraints (`PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK`, `CONSTRAINT`,
   `EXCLUDE`, `LIKE`) are not columns.
3. `CREATE [OR REPLACE] [MATERIALIZED] VIEW name`: `table` with object `view` or
   `materialized_view`; the query's tables as `table_access` select facts whose owner is the
   table key of the view.
4. `ALTER TABLE [IF EXISTS] [ONLY] name` with actions separated by commas: `ADD [COLUMN] [IF
   NOT EXISTS] c type` (column create), `DROP [COLUMN] [IF EXISTS] c` (column drop), `ALTER
   [COLUMN] c ...` (column alter), `RENAME [COLUMN] c TO d` (column rename on `c`, plus
   column create `d`), `RENAME TO new` (table rename, plus table create `new`),
   `RENAME CONSTRAINT a TO b` (no fact). Every `ALTER TABLE` also gives a `table` fact with
   action `alter`.
5. `DROP TABLE|VIEW [IF EXISTS] a, b [CASCADE]`: `table` drop per name.
6. `GRANT privileges ON [TABLE|SEQUENCE|FUNCTION|SCHEMA] objects TO roles` and `GRANT ...
   ON ALL {TABLES|SEQUENCES|FUNCTIONS|PROCEDURES|ROUTINES} IN SCHEMA s TO ...` (object kind
   `ALL <kind> IN SCHEMA`, object `s`): one `grant` fact per privilege, object and role.
   A privilege with a column list (`SELECT (a, b)`) gives one fact per column, with the
   column in `column` and in the key (`SELECT(a) orders reader`).
   In a function or `DO` body, `EXECUTE format('GRANT ...', args)` gives grant facts when
   every argument is a string literal or the variable of an enclosing
   `FOREACH v IN ARRAY ARRAY['a', ...] LOOP` (all elements string literals): one statement per
   value, with `%I` and `%s` replaced by the argument, `%L` by the quoted argument and `%%` by
   `%`. An open `IF` (or `ELSIF`) branch whose condition is `v IN ('a', ...)` or `v = 'a'`
   keeps only those values of `v`; any other condition and `ELSE` keep all. An `IF` counts
   only where a statement or branch starts (not `DROP TABLE IF EXISTS`). Any other argument
   (query loop variable, expression) gives no fact.
   `REVOKE ... FROM roles` likewise with `revoke: true`. Object names follow table key rules;
   functions keep their name without the argument list. A role membership grant without
   `ON` (`GRANT role TO role`) gives no fact.
7. `CREATE [OR REPLACE] FUNCTION|PROCEDURE name(args) ... AS <body> LANGUAGE x` (clauses in
   any order): `sql_function` with `language` lowercased; when the body is a `DollarStr` or a
   string, the body is lexed again and its statements give `table_access` facts (owner = the
   function key) through `sql_text_accesses`, and `grant` facts as in 6 with `owner`. A
   body statement is a grant when it starts with `GRANT` or `REVOKE`, or has one right after
   `BEGIN`, `THEN`, `ELSE` or `LOOP` (`IF ... THEN GRANT ...;`); the fact's line is the
   line of that keyword.
8. `DO [LANGUAGE x] <body>`: body handled like a function body with owner
   `<file>#do@<line>`.
9. `CREATE [OR REPLACE] [CONSTRAINT] TRIGGER name {BEFORE|AFTER|INSTEAD OF} event [OR event]
   ON table ... EXECUTE {FUNCTION|PROCEDURE} fname(...)`: `trigger`. `UPDATE OF cols` counts as
   `UPDATE`. `DROP TRIGGER name ON table`: trigger drop.
10. Top-level `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `SELECT`, `WITH`, `MERGE` statements:
    `table_access` facts with the file as owner.
11. `sql_text_accesses(sql)`: lex `sql`; for each statement, `INSERT INTO t` → insert,
    `UPDATE [ONLY] t` → update, `DELETE FROM [ONLY] t` → delete, `TRUNCATE [TABLE] t, u` →
    truncate, `MERGE INTO t` → insert and update (its `THEN INSERT` and `THEN DELETE`
    actions add nothing; outside a `MERGE`, as in PL/pgSQL `IF ... THEN INSERT INTO t`, they
    are ordinary statements), every table name after `FROM` or `JOIN` (including
    comma-separated lists, skipping subqueries and function calls such as
    `generate_series(...)`; not `IS DISTINCT FROM` and not `FROM` inside the arguments of
    `EXTRACT`, `SUBSTRING`, `TRIM` or `OVERLAY`) → select, and `select` becomes
    `select_for_update` for tables of a
    `SELECT` that has `FOR UPDATE` or `FOR NO KEY UPDATE`. CTE names defined by `WITH x AS
    (...)` are not tables. Aliases are ignored. Each `(table, op)` once, in order of first
    appearance.
12. Anything else (`CREATE INDEX`, `COMMENT ON`, `CREATE TYPE`, `CREATE EXTENSION`, `SET`,
    ...) is ignored without an `unparsed` fact. A statement that starts with a recognized
    keyword sequence but cannot be read gives `unparsed`.

### 9.3 TypeScript and JavaScript

1. One `module` fact per file (key: the path, name: file name without extension).
2. `fn_def` for named functions (7.2); `is_pub` when exported; `is_async` for `async`;
   `owner_type` for class methods.
3. `type_def` for `class` (kind `class`), `interface` (`interface`), `type X =` (`type_alias`),
   `enum` (`enum`).
4. `import` facts: `import x from 'm'` (name `default`, alias `x`), `import {a, b as c} from
   'm'`, `import * as ns from 'm'` (name `*`, alias `ns`), `import 'm'` (name `*`),
   `export ... from 'm'`, `require('m')` assigned to a binding (name `*`, alias the binding).
   `ImportData::path` is the specifier as written; key `<specifier>#<name>`.
5. `call` facts for calls inside function bodies and at module level (owner: module key):
   `name(...)` (no qualifier), `a.b.name(...)` (qualifier `["a","b"]`, `is_method`,
   `receiver` = `this` or the single identifier `a` when the callee is `a.name`), optional
   calls `a?.name(...)`, `new Name(...)` (callee `new Name`). Generic arguments
   `name<T>(...)` are dropped.
6. `api_call` facts by API client patterns with `lang: ts` (11.2). TypeScript and
   JavaScript give no `route` facts.
7. `env_var` facts: `process.env.X`, `process.env["X"]`, `import.meta.env.X`.
8. SQL in strings and templates as in 9.1.9 (template literals use their
   `template_pattern` text).
9. Statements end at `;` or, approximating automatic semicolon insertion, at a line break
   when neither the token before it (a punctuation other than `)` `]` `}` `++` `--`, or one
   of `new typeof await void delete instanceof in of as satisfies keyof extends yield
   async`) nor the token after it (any punctuation, or one of `instanceof in of as satisfies
   extends`) continues the expression. This applies to variable declarators, class fields
   and type aliases.
10. Decorators before class members (`@name`, `@a.b`, `@name(...)`) are skipped whole.
    `case X:` and `default:` followed by `{` open a block, not an object literal; `class`
    followed by `=` or after `.` is not a class declaration. `async (x) => ...` is not a call.

## 10. Paths

`model::norm_path` normalizes HTTP paths: query and fragment dropped first, then scheme and
host dropped (only when `://` comes before any `/`), empty segments removed, and every parameter segment (`:id`, `{id}`, `{*rest}`, `*...`, or any
segment containing `${`) replaced by `{}`. The result starts with `/`. Route and API call keys
use the normalized path, so `GET /orders/:id`, `GET /orders/{id}` and a call to
`` `/orders/${id}` `` all meet at `GET /orders/{}`. A route with method `ANY` matches calls of
every method.

## 11. Configuration

### 11.1 File

`arlineo.json` at the repository root (the top level of the worktree the command runs in).
It is optional; without it Arlineo uses `Config::defaults`. Unknown fields are errors. Every
object, rule selectors included, may have a `note` string for comments. Example:

```json
{
  "note": "Example configuration",
  "base": "main",
  "include": ["backend/**", "frontend/src/**"],
  "exclude": ["**/generated/**"],
  "routes": [
    {
      "id": "router",
      "lang": "rust",
      "call": "route",
      "methods": { "get": "GET", "post": "POST" },
      "base_path": "/api/v1"
    }
  ],
  "api_clients": [
    {
      "id": "client",
      "lang": "ts",
      "call": "request",
      "receivers": ["*"],
      "method_arg": 0,
      "path_arg": 1,
      "base_path": "/api/v1"
    }
  ],
  "rules": [
    {
      "id": "payments-need-permission",
      "kind": "requires-before",
      "note": "Code that writes payments must check the permission first",
      "when": { "kind": "table_access", "key": "payments", "data": { "op": "insert" } },
      "require": { "kind": "call", "data": { "name": "check_permission" } },
      "depth": 2
    }
  ]
}
```

`Config::from_bytes` validates: unique pattern ids, unique rule ids, compilable globs, HTTP
methods uppercase or `ANY`, `call` present in every route pattern, `lang` `rust` for route
patterns (only the Rust extractor reads routes), `include_ancestor_callees` not together with `all_paths`, each `ignore`
entry of a rule with a non-empty `key` and `reason` and no key twice, `lang` `ts` for API client
patterns (only the TypeScript extractor reads them), at most one of `method`, `method_arg`,
`method_option` per API client pattern, `prefix`, `strip_prefix` and `base_path` starting with
`/`, and per rule kind the required fields (11.4). Every problem is an `Error::Config` that
names the object (`rules[2] "id"`) and the field.

`Config::load` reads the file named by `$ARLINEO_CONFIG` when the variable is set and not
empty (an absolute path to an existing file, else an error), else `<repo top>/arlineo.json`.

`Config::hash` covers only what changes extraction: `include`, `exclude`, `routes` and
`api_clients`, without `note` fields. Editing `base`, `rules` or a note does not
rebuild indexes.

### 11.2 Patterns

Route patterns are Rust only; TypeScript and JavaScript code gives API calls, never routes.
There are no built-in patterns: without configured patterns no route or API call is recorded.

1. Routes: a call whose callee name is `call` (bare or method call), with a string
   literal at `path_arg`. Every other argument is scanned for wrapper calls `w(handler)` where
   `w` is a key of `methods`, including chains `get(a).post(b)` and qualified
   `routing::get(a)`. Each wrapper gives one route: method `methods[w]`, handler = the
   wrapper's first argument as written.
2. API clients: a call whose name is `call`, bare when `receivers` is empty, on any receiver
   for `["*"]`, else on one of the listed receivers; the argument at `path_arg` must be a string
   or template literal. Method: `method`, else the string argument at `method_arg`, else the
   string value of key `method_option` in an object literal argument, else `default_method`,
   else `GET`.

### 11.3 Globs

`*` matches any run of characters except `/`; `**` as a whole segment matches zero or more
segments; `?` matches one character except `/`; everything else matches itself. Globs match
the whole text. `include` empty means every file. Matching is case-sensitive.

### 11.4 Rules

A selector `{ "kind", "key"?, "file"?, "data"? }` matches facts of that kind whose key, file
and listed data fields (as `Fact::data_text`) match the globs. Owner of a fact: its
`data.owner` field (calls, routes, API calls, environment variables, SQL in code, table
accesses, grants inside functions).

1. `requires-before` (`when`, `require`, optional `depth` default 0,
   `include_ancestor_callees` default false, `all_paths` default false): for every fact
   matching `when`, take its owner
   function. The rule holds when a fact matching `require` has as owner: the owner itself, or
   a transitive caller up to `depth` levels (incoming `calls` edges of any confidence,
   `guessed` included), or, with `include_ancestor_callees`, a function called directly by one
   of those callers. One such function is enough, even if other callers lack the
   requirement, and the order of the two facts is not checked. Otherwise one hit per owner and
   `when` key. With `all_paths` (not with `include_ancestor_callees`): a function takes the
   requirement before line `L` when it has a `require` fact before `L` or calls, on an earlier
   line, a function that has one. The owner must take it before the `when` fact, else every
   caller (incoming `calls` and `handles` edges, not `guessed`) must take it before its call,
   or have all its callers do so, up to `depth` levels. A chain that reaches a function
   without callers or `depth` levels without it is a hit (one per owner and `when` key, the
   message names the chain). A caller already on the chain is skipped (call cycle); a
   self-call does not count as a caller. The hit is uncertain (`Hit.certain` false) when its
   chain is cut by `depth`, ends at a function whose only callers are `guessed`, or ends at a
   function whose callers are all on the chain; a certain chain is preferred. A "locked"
   result is remembered per (function, line, depth) only when no caller was skipped for being
   on the chain.
2. `order-in-function` (`first`, `then`, optional `only_with_first` default false): for every
   owner that has a fact matching `then`, the rule holds when the same owner has a fact
   matching `first` on an earlier line than the first `then` fact, or, with
   `only_with_first`, when the owner has no `first` fact at all. Otherwise one hit per owner.
3. `sources-agree` (`left`, `right`, `fields`, default `["key"]`, `direction` default
   `both`, optional `minus` and `dropped`): the set of value tuples of `fields` (`key` means
   the fact key) over facts matching `left` equals the set over facts matching `right`. One
   hit per tuple present on only one side; with `left-in-right`, only left tuples missing on
   the right. A left fact is dropped when a `minus` fact has the same tuple at a later position
   (file name, then line), or when a `dropped` fact at a later position has a key equal to the
   left fact's `object` field (its key without that field); a dropped key that is a table
   also ends the `sequence` of each of its `column` facts. For `grant` facts compared
   through `privilege` and `column`, the same grant on the whole object and `ALL` on the
   column or object also cover a tuple, also for `minus`.
4. `has-target` (`source`, `target`, `source_field`, `target_field`, defaults `key`): every
   fact matching `source` has a fact matching `target` whose `target_field` equals its
   `source_field`. For `api_call` → `route` keys, paths match segment by segment: equal
   method or route `ANY`, equal segment count, a route `{}` takes any segment, and a call `{}`
   against a constant route segment holds only when a constant segment after the API client
   `base_path` agrees too. A call with `{}` right after a base path has an unknown prefix and
   is skipped. Routes count only with a constant segment after the base path. One hit per
   source fact without a target.
5. `forbidden` (`selector`): one hit per matching fact.
6. `review-on-change` (`selector`): one hit per matching fact, keyed by its `hash` data field,
   else a hash of all its data. The line has every such hit, so `check` reports exactly the
   matching facts a worktree adds or changes.

Hit keys (stable across line shifts): `<rule id>|<owner or fact key>|<extra>` where extra is
the `when` key (1), empty (2), the tuple joined by `,` with the side (3), the source key (4),
the fact file and key (5), or the content hash with the fact key in the middle part (6).
`check` reports hits whose `(rule, key)` is not present in the line view (18.10).

## 12. Graph

### 12.1 Nodes

One node per `fn_def` (kind `fn`), `type_def` (`type`), `module` (`module`), `sql_function`
(`sql_function`), `table` key (`table`, one node per key even with several facts; the file
and line of the first `create`, else of the first fact) and `route` key (`route`). Duplicate
definition keys (for example `#[cfg]` variants) keep the first fact in facts order.

### 12.2 Edges

| Edge | From | To | Confidence |
|---|---|---|---|
| `contains` | module | its functions and types; type → its methods | exact |
| `calls` | owner function | resolved callee function or SQL function | 12.3 |
| `handles` | route | handler function of every route fact with this key | resolution of the handler text as a bare or qualified call from the registering owner; at most `heuristic` |
| `requests` | API call owner | route with the same key, or `ANY` route with the same path | heuristic |
| `reads` / `writes` | owner (function or SQL function) | table | `select`/`select_for_update` read; others write; fact confidence |
| `triggers` | table | trigger function (`sql:<name>`) | exact |
| `imports` | module | module it imports from, when resolvable | exact |

Edges are deduplicated on `(from, to, kind)` keeping the strongest confidence and the lowest
line. Edge order: by `from`, `kind`, `to`.

### 12.3 Call resolution

For a `call` fact with owner `O` in module `M` (Rust or TS):

1. Rust `self.name(...)` or `Self::name(...)` inside `impl T`: `M::T::name` if it exists →
   `exact`.
2. Qualified Rust calls: the qualifier is resolved through, in order: `crate::`, `self::`,
   `super::` (relative to `M`); a `use` import in `M` whose name or alias equals the first
   segment; a crate name; then the result path plus `name` is looked up as a function key,
   and any other path as written and relative to `M` (`Type::name` as `M::Type::name`,
   `a::b::name` as `M::a::b::name`) → `exact` when found.
3. Bare calls: a function nested in `O`, then `M::name`, then a `use` or `import` that brings
   `name` into `M` (resolved to its target key) → `exact`.
4. TS calls through an import: relative specifiers (`./x`, `../x`) are resolved against the
   importing file with extensions `.ts .tsx .js .jsx .mts .cts .mjs .cjs` and `/index.*`, and a
   specifier naming an emitted file (`./x.js`) also as its source (`./x.ts`, `./x.tsx`; `.jsx`
   → `.tsx`, `.mjs` → `.mts`, `.cjs` → `.cts`); the imported name is looked up in that module
   → `exact`. A default import is looked up as `default` and then under its local name.
   Namespace imports `ns.name(...)` likewise.
5. Otherwise, all functions with this last name: exactly one → `unique_name`; 2 to
   `MAX_GUESSES` → one `guessed` edge to each; more → no edge.
6. Method calls on receivers other than `self`/`this` (`x.save()`) use step 5 only, restricted
   to methods (`owner_type` set) when any method has the name.
7. Macros, calls to `new Name` and calls whose name is a common standard library method
   (`clone`, `to_string`, `into`, `from`, `iter`, `map`, `unwrap`, `push`, `len`, `get`,
   `insert`, `collect`, `then`, `catch`, `forEach`, `filter`, `find`, `join`, `split`,
   `toString`, `parse`, `json`, `log`) never get `unique_name` or `guessed` edges; they get
   only `exact` edges from steps 1 to 4.
8. SQL `sql:` functions are callees only through triggers (12.2); calls from code to SQL
   functions are not resolved.

### 12.4 Determinism

Nodes are ordered by kind (`NodeKind::ALL` order), then key. The same facts always give the
same graph. A fact whose data does not decode for its kind fails the build with a JSON error;
no fact is skipped silently.

### 12.5 Graph file

`graph.bin`, all integers little-endian:

| Part | Content |
|---|---|
| magic | 8 bytes `ARLNGRPH` |
| version | u32, `GRAPH_FORMAT_VERSION` |
| facts hash | u32 length + UTF-8 bytes (the 16-hex-digit FNV of `facts.jsonl`) |
| strings | u32 count, then per string u32 length + UTF-8 bytes; each distinct key and path once, in first-use order |
| nodes | u32 count, then per node: u8 kind code, u32 key string index, u32 file string index or `u32::MAX` for none, u32 line |
| edges | u32 count, then per edge: u32 from, u32 to, u8 kind code, u8 confidence code (0 exact, 1 unique_name, 2 heuristic, 3 guessed), u32 line |
| checksum | u64 FNV-1a 64 of every preceding byte |

Readers check every length against the remaining bytes before allocating, reject unknown codes,
out-of-range indexes and trailing bytes, and verify the checksum before building the graph with
`Graph::from_parts`. Any problem means `None`: the caller rebuilds from facts.

## 13. Sources

1. A file is read only when `Config::wants` accepts it.
2. Secret files are never read: names `.env`, `.env.*`, `*.env`, `*.pem`, `*.key`, `*.p12`,
   `*.pfx`, `id_rsa*`, `id_ed25519*`, `*.keystore`, and anything under a directory named
   `secrets` or `.ssh`.
3. Files larger than `MAX_FILE_BYTES`, containing a NUL byte, or not valid UTF-8 are skipped
   with a reason. A UTF-8 byte order mark is removed.
4. From disk: symlinks are never followed (`symlink_metadata`); size and modification time are
   read before and after reading; on a difference the file is read again, at most 3 times.
5. A skipped file gives one `unparsed` fact with line 0 and the reason, and counts in
   `BaseIndex::skipped`.

## 14. Git

1. Run `git` with `-C <dir>` and `-c core.fsmonitor=false` (no hook program from the
   repository configuration runs), arguments as separate process arguments, environment
   `LC_ALL=C`, `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1` (a partial clone does not fetch
   missing objects), with the inherited `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`,
   `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_COMMON_DIR`,
   `GIT_NAMESPACE` and `GIT_PREFIX` removed (a hook that runs Arlineo must not redirect it),
   stdin null except for `cat-file --batch`.
2. Commands: `rev-parse --show-toplevel`, `rev-parse --git-common-dir` (resolved against the
   `-C` directory when relative, then canonicalized), `rev-parse --verify --quiet <rev>^{commit}`,
   `merge-base <a> <b>` (exit 1 without output means unrelated histories), `ls-tree -r -l -z
   <commit>`, `diff --name-status -z --no-renames <a> <b>`, `status --porcelain=v1 -z
   --untracked-files=all --no-renames`, `worktree list --porcelain -z`, `cat-file --batch`.
3. Paths from git that are not UTF-8 or not valid `RelPath`s are skipped and counted.
4. `cat-file --batch` protocol: write `<oid>\n`, read `<oid> <type> <size>\n`, then exactly
   `size` bytes and `\n`; `<oid> missing\n` means not found. Sizes above
   `MAX_FILE_BYTES` are read and discarded in chunks and reported as too large by the caller.

## 15. Store

1. Location: `<state>/arlineo/<repo-id>/` with `<state>` = `$ARLINEO_STATE_DIR`, else
   `$XDG_STATE_HOME`, else `$HOME/.local/state`, each used only when it is an absolute path;
   `<repo-id>` = FNV-1a 64 hex of the git common directory path. No usable variable is an
   error. On Unix, `<state>/arlineo/` and the store directory are created with mode 0700.
2. Layout:
   ```
   meta.json
   lock
   base/<commit>/facts.jsonl
   base/<commit>/graph.bin
   overlay/<worktree-id>/facts.jsonl      facts of changed files on disk
   overlay/<worktree-id>/before.jsonl     facts of the same files at the merge-base
   overlay/<worktree-id>/cache.json       per path: size, mtime (ns), hash; merge-base; config hash
   overlay/<worktree-id>/view-graph.bin   graph of the worktree view (17.3)
   ```
3. All writes go through `store::write_atomic`. Writers hold the lock; readers do not lock and
   rely on atomic renames. The one exception is `view-graph.bin` (17.3): it is written without
   the lock and on a best-effort basis, because it is only a cache checked by its key.
4. Lock: create `lock` with `create_new`, containing the process id and Unix time; on
   `AlreadyExists` retry every 100 ms up to 30 s; a lock file older than 10 minutes whose
   holder process is gone (checked through `/proc` where it exists) is replaced, only if its
   contents did not change meanwhile. On drop the lock file is removed only when it still
   holds this process's contents.
5. `meta.json`: `Meta` (`format_version`, `tool_version` = the crate version, and per indexed
   commit its config hash, facts hash and number of skipped files). The store is a cache: a
   missing, unreadable or foreign-version `meta.json` (format or tool version) is replaced by
   a fresh one under the lock; after every `meta.json` write, base directories of commits it
   does not list are removed (best effort).

## 16. Indexing

1. The line: `--base`, else `base` from the configuration, else `main`, else `master`, else
   the main worktree's `HEAD`.
2. `ensure_index(commit)`: if `meta.json` lists the commit with the current config hash and
   both files exist and read cleanly (the `facts.jsonl` bytes hash to the listed facts hash),
   load them (graph file through `read_graph`; a `None`
   rebuilds the graph and rewrites the file). Otherwise, under the lock:
   - take the newest indexed commit with the same config hash as a starting point, if any;
     `diff_names(start, commit)` gives changed paths; facts of unchanged paths are kept, facts
     of changed and deleted paths are removed, changed and added paths are extracted again;
     when any `Cargo.toml` changed, everything is extracted again;
   - without a starting point, extract every wanted file of `ls_tree(commit)`;
   - sort, write `facts.jsonl`, build the graph, write `graph.bin`, update `meta.json`, prune
     to the 5 newest indexes (the pruned directories are removed).
3. Commands call `ensure_index` for the line's current commit before answering, so the index
   follows the line automatically.

## 17. Overlays and views

1. Active worktrees: every worktree that is not bare, not prunable and has a HEAD.
2. `overlay_for(worktree)`: merge-base of its HEAD and the line commit (unrelated histories:
   `Error::NotFound`). Changed paths: `diff_names(merge_base, HEAD)` combined with the
   worktree's `status()`; per path the last state wins (deleted on disk means deleted). Facts
   of changed paths come from disk (`load_from_disk` at the worktree top); `before` facts come
   from the merge-base blobs. Results are cached in the overlay directory and reused per path
   while size, modification time and merge-base are unchanged and the config hash matches; a
   file modified less than 2 seconds ago is always read again, and a file that changed while
   it was read is not cached. When nothing changed, the cache files are not rewritten.
   Overlay directories of worktrees that no longer exist are removed by `status`.
3. `worktree_view`: the base index of the line commit, minus all facts whose file is a changed
   path, plus the overlay facts; sorted; graph built. The graph is cached in
   `overlay/<worktree-id>/view-graph.bin` (format 12.5) whose facts-hash field holds a key:
   FNV-1a 64 hex over the base facts hash, each changed path followed by a newline, and the
   JSONL bytes of the overlay facts. A missing, unreadable or stale cache means the graph is
   built again; a failed cache write is ignored.
4. Touched definitions: compare `before` and `facts` by `(kind, key)` for kinds `fn_def`,
   `type_def`, `sql_function`, `table`, `column`, `route`, `trigger`, `grant`; present only after
   → added, only before → removed, both with different `hash` (or, when the kind has no hash,
   different data apart from `end_line`, so a definition that only moved is not changed) →
   changed. Added `.sql` files also count as an added `file` definition. `conflicts` and `diff`
   share this comparison.

## 18. Commands

1. `status`: line name and commit, index age, store path, number of facts per kind, skipped
   files, unparsed facts, and one line per active worktree (name, merge-base, changed files).
2. `index [REV]`: `ensure_index` for REV (default: the line) and a summary.
3. `find`: facts matching `--kind` and every `--where` (globs over `key`, `file`, `line` as
   text, or `data_text(field)`), up to `--limit`. Text: `<file>:<line> <kind> <key>` plus the
   data as compact JSON.
4. `table NAME`: table, columns (with the latest action per column), grants, triggers, and
   readers and writers (owners of `table_access` facts and incoming graph edges), each with
   file and line.
5. `route PATH`: routes whose normalized path equals `norm_path(PATH)` or matches it as a
   glob; for each route fact, the `handles` targets whose last key segment equals the last
   segment of that fact's handler; API calls with the same key; and API calls with that path that have
   no route.
6. `fn NAME`: functions whose key equals NAME, or whose key matches NAME as a glob (11.3, so
   `**` is needed to cross the `/` of TypeScript keys) when it contains `*`, or whose last
   segment equals NAME; up to `--limit`, with a `... N more` line and `truncated` in JSON when
   more match; for each: definition, callers and callees with confidence and call line, and
   facts with this owner.
7. `graph FROM`: nodes whose key matches like `fn`; breadth-first up and down with depth
   limits, each node once, printed as an indented tree with edge kind and confidence.
8. `impact TARGET`: nodes found like `fn` (also tables, routes, SQL functions by key);
   transitive incoming `calls`, `handles` (from routes), `requests` (from API callers) and
   writers (`writes` edges, not `reads`) for tables, up to `--depth`; each reached node with
   the weakest confidence on its strongest path within the depth; grouped by confidence,
   strongest first. A stronger but longer path never hides a shorter one that still reaches
   further nodes within the depth.
9. `conflicts`: overlays of all active worktrees except the one whose HEAD equals the line
   commit with no changes; collisions between every pair: same touched `(kind, key)`, same
   migration id of added `.sql` files (under a `migrations` directory the leading digits of
   the file name, so `0004_a.sql` and `0004_b.sql` collide; elsewhere the file name), same
   changed path; each with both worktrees and files.
10. `check [--worktree W]`: `hits` in the worktree view (default: the current worktree) minus
    hits in the line view (the base index of the line's current commit), so hits that the
    line already has are not reported; a certain hit is compared only with the line's certain
    hits, an uncertain one with all. Status `Findings` when any certain hit remains. Text:
    one line per hit, `<file>:<line> <message>`, where the message starts with `Rule <id>:`,
    then uncertain hits as `uncertain (not counted): <file>:<line> <message>`, then
    `warning: rule <id>: ignore entry '<key>' matches no hit` per unused `ignore` entry (in
    the worktree view), then the summary (`, N uncertain`, `, N ignored`); JSON `new`,
    `uncertain`, `already_present`, `ignored`, `ignore_unused`. `hits` drops a rule's hits
    whose key is in its `ignore`.
11. `diff BRANCH...`: for each branch the merge-base with the line, changed files and touched
    definitions (base index of the merge-base versus base index of the branch commit), then
    collisions between the branches and with the line's own changes since each merge-base.

## 19. Output

1. Text stamp, the first line of every command's output:
   `arlineo: line main@3f2a1c9d0 indexed 12s ago` plus
   ` | worktree feature-x (merge-base 1b2c3d4e5, 3 changed files)` when a worktree view is used.
2. Then the report lines. Paths are repository-relative. No colors.
3. JSON: one object `{"stamp": {"line": "...", "commit": "...", "indexed_unix": n, "now_unix":
   n, "worktree": null | {"name": "...", "merge_base": "...", "changed_files": n}}, "command":
   "...", "result": ...}` followed by `\n`. `result` is the report's JSON value.
4. Every query's JSON result lists items with `kind`, `key`, `file`, `line`, `confidence` where
   applicable, so agents can follow up with the same keys.

## 20. Security

1. The configuration is data. No field is ever executed, evaluated or passed to a shell.
2. Arlineo only runs `git` with fixed subcommands; user input appears only as separate
   arguments after `--` where git supports it, and revisions are checked with
   `rev-parse --verify`.
3. Secret files are never read (13.2). Values of environment variables are never read or
   stored; only names written in code.
4. Symlinks are not followed. Paths from git and from configuration are validated as
   `RelPath`.
5. The store contains facts about code (names, paths, lines); treat it like the source code.

## 21. Limitations (initial)

1. No type inference: method calls on values resolve by name only (`unique_name`/`guessed`).
2. Macros: calls inside macro arguments are extracted, items generated by macros are not.
3. Route prefixes from nested routers (`nest`) are not joined;
   use the pattern `prefix` option.
4. SQL: PostgreSQL only; dynamic SQL built at runtime is not seen.
5. TypeScript: path aliases from `tsconfig.json` are not resolved.
6. Template literal expressions inside `${}` are not scanned for calls.

`LIMITATIONS.md` keeps the current list with examples.
