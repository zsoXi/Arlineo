---
name: arlineo
description: Use Arlineo before and after changing code in a repository it indexes, to check freshness, callers, tables, routes and worktree conflicts.
---

# Arlineo

Arlineo is a code map over the current git state. Run it from inside the repository.
Every answer starts with a stamp line. Read it first.

## Always

- Check the stamp before trusting an answer. Text form, first line:
  `arlineo: line main@3f2a1c9d0 indexed 12s ago | worktree feature-x (merge-base 1b2c3d4e5, 3 changed files)`
  (the worktree part appears only for a worktree view). If the line commit or
  merge-base is not the state you are editing, the answer describes other code.
- Use `--json` when you need exact keys. JSON output is
  `{"stamp": {...}, "command": "...", "result": ...}` and query items carry
  `kind`, `key`, `file`, `line` and `confidence` where applicable. Feed those
  keys back into follow-up commands instead of guessing names.
- Read confidence literally: `exact` is read from syntax or resolved through an
  explicit path, `use`, `self` or import; `unique_name` means exactly one
  definition has that name; `heuristic` means a pattern matched (routes, API
  calls, SQL in strings); `guessed` means one of several possible targets.
  Do not present `guessed` or `unique_name` callers as proven.

## Before editing

- Before editing a function, run both:
  - `arlineo fn <name>` - definition, callers and callees with confidence and
    call line, and the facts inside it (calls, table accesses, SQL, env vars).
  - `arlineo impact <name>` - what a change can reach, up to `--depth N`
    (default 5), grouped by the weakest confidence on the strongest path.
  Use a full key from `--json` output when a plain name matches several
  functions (`fn` also matches a `*` glob over keys, or the last segment).
- Before touching SQL or a table, run `arlineo table <name>` first: columns,
  grants, triggers, and every reader and writer with file and line. Also search
  for SQL held in code, for example
  `arlineo find --kind table_access --where key=<name>`.
- For an HTTP endpoint, run `arlineo route <path>`, for example
  `arlineo route /orders/:id`. Paths are normalized (`:id`, `{id}` and `${id}`
  all become `{}`), and the result shows handlers, API calls that reach the
  route, and API calls with that path that have no route.
- Before starting work in a worktree, run `arlineo conflicts`. It compares all
  active worktrees pairwise: same touched definition, added migrations with
  the same version number, same changed file. Coordinate before editing a colliding file.
- Use `arlineo status` to see the line, store, fact counts and every active
  worktree, and `arlineo find --kind unparsed` when facts look missing.

## While editing

- Answer from your own worktree's view with `--worktree <path|directory|branch>`
  when the question is about uncommitted or branch changes; without it, queries
  answer from the line alone and will not show your changed files.
- To walk neighbours one step at a time, use
  `arlineo graph <name> --up 1 --down 1` and widen only as needed.
- Treat `unparsed` facts as blind spots: that file or construct was not
  understood, so absence of an edge there proves nothing.

## Before finishing

- Run `arlineo check` (or `arlineo check --worktree <w>`). It reports only rule
  violations that the line does not already have, so hits that exist on the
  line are left out. A remaining hit comes from your changed files, but read
  it before treating it as your own: a change on the line can also make it
  appear. Exit code 3 means such violations remain; fix them or report them.
  Do not silence them by editing rules unless the task asks for a rule change.
- Re-run `arlineo impact <changed-function> --worktree <w>` and, when other
  worktrees are active, `arlineo conflicts`, after your last edit. The index
  follows git on every command, and with `--worktree` the answer includes your
  changed files as they are on disk; without it, `impact` answers from the
  line and does not see your edits.
- In your summary, name the stamp commit you verified against and call out any
  `guessed`, `unique_name` or `heuristic` edges you relied on.
