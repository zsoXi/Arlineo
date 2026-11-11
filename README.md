# Arlineo

Arlineo is a code map for a git repository. It reads the repository through `git`, extracts facts from Rust, PostgreSQL and TypeScript/JavaScript sources with hand-written lexers, builds a call graph in which every edge has a confidence level, and answers questions about the code: where something is, who calls it, what a change can reach, whether two worktrees change the same thing, and whether a change breaks the project's own rules (see `RULES.md`).

## Why

Teams of humans and AI coding agents often work in many git worktrees of one repository at the same time. An index built once in one place is then stale in every other place, and a tool that does not say what its answer is based on cannot be trusted after branches move. Arlineo has no background service, checks the current state of git on every command, and stamps every answer with the commit and worktree state it is based on, so a reader can tell at once whether an answer still describes the code in front of them.

## What it answers

One short example per command (`SPEC.md` section 5):

| Command | Example | What it reports |
|---|---|---|
| `status` | `arlineo status` | Line name and commit, index age, store path, fact counts per kind, skipped and unparsed files, and one line per active worktree. |
| `index` | `arlineo index` | Builds or updates the index of a revision (default: the line) and prints a summary. |
| `find` | `arlineo find --kind fn_def --where key=**/*orders*` | Facts matching `--kind` and every `--where F=GLOB` condition, up to `--limit`. In globs `*` stops at `/` and `**` crosses it, so `**/*orders*` matches both Rust keys (`app::orders::create_order`) and TypeScript keys (`frontend/src/api/orders.ts::loadOrder`). |
| `table` | `arlineo table orders` | One table: definitions, columns, grants, triggers, and who reads and writes it. |
| `route` | `arlineo route /orders/:id` | Routes whose normalized path matches, their handlers, the API calls that reach them, and API calls with that path that have no route. |
| `fn` | `arlineo fn create_order` | One function: its definition, callers and callees with confidence, and the facts inside it. |
| `graph` | `arlineo graph create_order --up 1 --down 1` | A breadth-first walk of the graph from a node, as an indented tree with edge kind and confidence. |
| `impact` | `arlineo impact orders --depth 5` | What a change to a target can reach, grouped by the weakest confidence on the strongest path. |
| `conflicts` | `arlineo conflicts` | Collisions between active worktrees: the same touched definition, added migrations with the same version number, or the same changed file. |
| `check` | `arlineo check` | Rule violations in a worktree view that the line does not already have. Exits with code 3 when such violations remain. |
| `diff` | `arlineo diff feature-orders feature-payments` | What each branch changes against its merge-base with the line, and where the branches collide with each other and with the line. |
| `help` | `arlineo help fn` | Usage for one command, or for all commands without an argument. |

Common options: `--worktree W` (answer from the view of worktree `W`, given as a path, directory name or branch), `--base REF` (use `REF` as the line instead of the configured one), `--json` (JSON output), and `--limit N` (at most `N` results, default 200). Options may appear anywhere after the command. Exit codes: 0 success, 1 error, 2 usage error, 3 `check` found new violations.

Example text output. The first line is the stamp (see below); the lines after it are the report:

```text
arlineo: line main@3f2a1c9d0 indexed 12s ago | worktree feature-x (merge-base 1b2c3d4e5, 3 changed files)
backend/src/orders.rs:41 fn_def app::orders::create_order {"end_line":58,"hash":"5f1c0e2a9b3d4c71","is_async":true,"is_pub":true,"name":"create_order"}
```

With `--json`, output is one object `{"stamp": {...}, "command": "...", "result": ...}` followed by a newline. Query results list items with `kind`, `key`, `file`, `line` and `confidence` where applicable, so the keys in a result can be used as arguments to follow-up commands.

## How freshness works

There is no background service and no daemon to start or stop. Every command except `help` runs in the current directory's repository, resolves the base line (the `--base` option, else `base` from the configuration, else `main`, else `master`, else the main worktree's `HEAD`), and calls `ensure_index` for the line's current commit before answering, so the index follows the line automatically. For a worktree view, the command also builds that worktree's overlay: the facts of the files it changed since its merge-base with the line, read from disk at question time.

Every answer starts with a stamp that says what it is based on. In text it is the first line, for example `arlineo: line main@3f2a1c9d0 indexed 12s ago`, plus ` | worktree feature-x (merge-base 1b2c3d4e5, 3 changed files)` when a worktree view is used. In JSON the same data is in the `"stamp"` object: line name, commit, when the base index was written, the current time, and the worktree name, merge-base and changed-file count when applicable. Read the stamp before trusting the lines below it: if the commit or merge-base is not the state you are working on, the answer describes other code.

## Confidence levels

Every graph edge, and every fact that is not read directly from syntax, carries one of four confidence levels, ordered from strongest to weakest:

| Level | Meaning |
|---|---|
| `exact` | Read directly from syntax, or resolved through an explicit path, `use`, `self` or import. |
| `unique_name` | Resolved by name because exactly one definition has that name. |
| `heuristic` | Recognized by a configured pattern (routes, API calls) or as SQL in strings. |
| `guessed` | One of several possible targets (2 to 8 candidates; with more, the call gets no edge). |

Facts extracted from syntax are `exact`. Facts recognized by patterns, and SQL found in string literals, are `heuristic`. A statement or declaration a lexer or extractor cannot read becomes an `unparsed` fact with a reason, and skipped files (too large, binary, not UTF-8) are counted by `status`. Some things are left out without an `unparsed` fact: SQL statements Arlineo does not model (for example `CREATE INDEX`, `SET`), constructs the extractors read only approximately (for example TypeScript without semicolons), and calls whose target cannot be resolved get no graph edge. See `LIMITATIONS.md` for what is guessed or missed.

## Supported languages and patterns

- Rust: modules, functions (including methods, trait methods and nested functions), types, constants and statics, `impl` blocks, `use` imports, calls, environment variable access (`env::var`, `env!` and related forms), routes by pattern, and SQL found in string literals.
- PostgreSQL (`.sql` files): tables, views and materialized views, columns, grants and revokes, triggers, SQL functions and procedures, `DO` blocks, and table accesses from statements and from function bodies. Other statements (for example `CREATE INDEX`, `SET`) are ignored without an `unparsed` fact.
- TypeScript and JavaScript (including JSX in `.tsx` and `.jsx` files): modules, named functions, class methods, types, imports and `require`, calls, `process.env` and `import.meta.env` access, API calls by pattern, and SQL in strings and templates.

Routes (in Rust) and API calls (in TypeScript/JavaScript) are recognized only by the patterns configured in `arlineo.json`; there are no built-in patterns. HTTP paths are normalized before matching: parameter segments such as `:id`, `{id}` and `${id}` all become `{}`, so `GET /orders/:id` and a call to `/orders/${id}` meet at `GET /orders/{}`.

## Configuration

Configuration is optional. `arlineo.json` at the repository root selects which files are indexed, adds route and API client patterns, and defines project rules. `ARLINEO_CONFIG=/absolute/path.json` uses a file outside the repository instead. Without either, Arlineo uses defaults: no route or API client patterns, no rules, and every supported file. Unknown fields in the file are errors. Example:

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
      "methods": { "get": "GET", "post": "POST", "patch": "PATCH", "delete": "DELETE" },
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

`RULES.md` is the full configuration reference: every field, the glob and selector syntax, and the five rule kinds with examples.

## Build from source

Arlineo is written in Rust, edition 2024, which needs Rust 1.85 or newer. Its only dependencies are `serde` and `serde_json`. At run time it needs `git` 2.36 or newer (`worktree list --porcelain -z`).

```sh
cargo build --release
```

This produces the `arlineo` binary under `target/release/`. There is no other build step and no code generation.

## Where the index lives

Arlineo never writes into a repository or worktree it reads. Its index lives in a state directory outside the repository: `<state>/arlineo/<repo-id>/`, where `<state>` is `$ARLINEO_STATE_DIR` if set, else `$XDG_STATE_HOME`, else `$HOME/.local/state` (only absolute paths count; on Unix the directories are created readable by the user only), and `<repo-id>` is the FNV-1a 64 hex of the repository's git common directory path, so all worktrees of one repository share one store. The store holds `meta.json`, a `lock` file, one `base/<commit>/` directory per indexed commit (at most 5 are kept) with `facts.jsonl` and `graph.bin`, and one `overlay/<worktree-id>/` directory per worktree. See `ARCHITECTURE.md` for the layout and formats.

## Security notes

- The configuration file is data. No field is ever executed, evaluated or passed to a shell.
- Arlineo only runs `git`, with fixed subcommands; user input appears only as separate process arguments after `--` where git supports it, and revisions are checked with `rev-parse --verify`.
- Secret files are never read: files named `.env`, `.env.*`, `*.env`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`, `id_ed25519*` or `*.keystore`, and anything under a directory named `secrets` or `.ssh`. Values of environment variables are never read or stored; only variable names written in code are recorded.
- Symlinks are not followed. Paths from git and from the configuration are validated as repository-relative paths.
- The store contains facts about code (names, paths, lines, and a 120-character preview of SQL found in string literals, with `'...'` literals masked); treat it like the source code.
- Arlineo itself makes no network access. It runs `git` with `GIT_NO_LAZY_FETCH=1`, which stops git 2.44 and newer from fetching missing objects of a partial clone; older git versions ignore the variable.

## License

Apache-2.0. See the `LICENSE` file.
