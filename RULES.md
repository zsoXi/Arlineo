# Arlineo configuration and rules: `arlineo.json`

`arlineo.json` sits at the repository root (the top level of the worktree the command runs in). It is optional; without it Arlineo uses defaults: no route or API client patterns, no rules, and every supported file. The file is data only; nothing in it is ever executed, evaluated or passed to a shell.

When the environment variable `ARLINEO_CONFIG` is set and not empty, Arlineo reads the file it names instead of the repository's `arlineo.json`, so the rules for a repository can live outside it. The path must be absolute and the file must exist; otherwise the command fails.

Unknown fields are errors: every object lists exactly the fields it may have. Every object, rule selectors included, may also have a `note` string, which is a comment for humans and changes nothing. When the file is invalid, the error is an `Error::Config` that names the object and the field, for example `rules[2] "id"`. Validation checks: unique pattern ids, unique rule ids, compilable globs, HTTP methods uppercase or `ANY`, `call` present in every route pattern, `lang` `"rust"` for route patterns, `include_ancestor_callees` not together with `all_paths`, `lang` `"ts"` for API client patterns, at most one of `method`, `method_arg` and `method_option` per API client pattern, `prefix`, `strip_prefix` and `base_path` starting with `/`, and the required fields per rule kind (below).

Only `include`, `exclude`, `routes` and `api_clients` change what is extracted; editing them rebuilds the indexes. Editing `base`, `rules` or a `note` does not.

Types below are written as in JSON: `string`, `number` (an unsigned integer index or count), `boolean`, `array of string`, `object`. "Default" is the value used when the field is omitted from an otherwise valid file.

## Top-level fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `note` | string | none | Comment for humans. |
| `base` | string | none | The base line ref (for example `"main"`). The `--base` command option overrides it; without either, Arlineo tries `main`, then `master`, then the main worktree's `HEAD`. |
| `include` | array of string (globs) | `[]` | Only files matching one of these globs are indexed. Empty means every supported file. |
| `exclude` | array of string (globs) | `[]` | Files matching one of these globs are not indexed, even when `include` matches them. |
| `routes` | array of route pattern objects | `[]` | Patterns that recognize route registrations. Without them no route is recorded. |
| `api_clients` | array of API client pattern objects | `[]` | Patterns that recognize HTTP calls from client code. Without them no API call is recorded. |
| `rules` | array of rule objects | `[]` | Project rules checked by `arlineo check`. |

Supported files are Rust (`.rs`), PostgreSQL (`.sql`), TypeScript (`.ts`, `.tsx`, `.mts`, `.cts`) and JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`) files. A file is indexed only when its language is known, it matches `include`, it does not match `exclude`, and it is not a secret file (secret files are never read; see the security notes in `README.md`).

## Route patterns (`routes`)

A route pattern recognizes route registrations in Rust code (`"lang": "rust"`) and produces `route` facts with `heuristic` confidence. TypeScript and JavaScript code is read for API calls only, never for routes. Fields:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | required | Unique pattern id. Recorded as the fact's `pattern`. |
| `note` | string | none | Comment for humans. |
| `lang` | `"rust"` | required | Language the pattern applies to; any other value is rejected. |
| `call` | string | required | Name of the registering call, for example `"route"`. |
| `path_arg` | number | `0` | Index of the argument that holds the path string. |
| `methods` | object (string to string) | required | Wrapper name mapped to an HTTP method. Each value must be an uppercase HTTP method or `ANY`. |
| `prefix` | string | none | Prefix put in front of every path found by this pattern, before the path is normalized. |
| `base_path` | string | none | Path the found paths are relative to. It is put in front of a path (after `prefix`) unless the normalized path already starts with it as whole segments, so `"/orders"` and `"/api/v1/orders"` both give `/api/v1/orders` for base path `/api/v1`. |

A route is a call whose callee name is `call` (bare or a method call), with a string literal at `path_arg`. Every other argument is scanned for wrapper calls `w(handler)` where `w` is a key of `methods`, including chains such as `get(a).post(b)` and qualified calls such as `routing::get(a)`. Each wrapper gives one route: method `methods[w]`, handler the wrapper's first argument as written.
Example:

```json
{
  "id": "router",
  "lang": "rust",
  "call": "route",
  "methods": { "get": "GET", "post": "POST" },
  "base_path": "/api/v1"
}
```

This recognizes `.route("/orders/{id}", get(get_order))` and records the route `GET /api/v1/orders/{}` with handler `get_order` (after normalization; see "Paths" below).

## API client patterns (`api_clients`)

An API client pattern recognizes HTTP calls from client code and produces `api_call` facts with `heuristic` confidence. Fields:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | required | Unique pattern id. Recorded as the fact's `pattern`. |
| `note` | string | none | Comment for humans. |
| `lang` | `"ts"` | required | Language the pattern applies to. Only TypeScript and JavaScript client code is read; `"rust"` is rejected. |
| `call` | string | required | Called function or method name, for example `"request"`. |
| `receivers` | array of string | `[]` | Empty: only a bare call `call(...)`. `["*"]`: a method call on any receiver. Otherwise: a method call on one of these receiver identifiers. |
| `path_arg` | number | `0` | Index of the argument that holds the path (a string or template literal). |
| `method` | string | none | Fixed HTTP method (uppercase or `ANY`). |
| `method_arg` | number | none | Index of a string argument that holds the method, for example `request("POST", path)`. |
| `method_option` | string | none | Key of an object literal argument that holds the method, for example `{ method: "POST" }`. |
| `default_method` | string | none | Method used when none of `method`, `method_arg` and `method_option` gives one. |
| `strip_prefix` | string | none | Prefix removed from the path before normalization, for example `"/v1"`. |
| `base_path` | string | none | Path the called paths are relative to, for example the client's base URL path `"/api/v1"`. It is put in front of a path (after `strip_prefix`) unless the normalized path already starts with it as whole segments. |

At most one of `method`, `method_arg` and `method_option` may be present. The method of a matched call is decided in this order: `method`, else the string argument at `method_arg`, else the string value of key `method_option` in an object literal argument, else `default_method`, else `GET`.

Example, a client object with a `request(method, path)` method:

```json
{
  "id": "client",
  "lang": "ts",
  "call": "request",
  "receivers": ["*"],
  "method_arg": 0,
  "path_arg": 1,
  "base_path": "/api/v1"
}
```

This recognizes `api.request("POST", "/orders")` and records `POST /api/v1/orders` after the base path is put in front and the path is normalized.

## Paths

Route and API call keys are `<METHOD> <norm_path>`, where `METHOD` is uppercase or `ANY`. Normalization drops scheme and host, drops query and fragment, removes empty segments, and replaces every parameter segment (`:id`, `{id}`, `{*rest}`, `*...`, or any segment containing `${`) with `{}`. The result starts with `/`. So `GET /orders/:id`, `GET /orders/{id}` and a call to `` `/orders/${id}` `` all meet at `GET /orders/{}`. A pattern's `prefix` is added, and an API client pattern's `strip_prefix` is removed, before normalization. A route with method `ANY` matches calls of every method.

## Globs

Globs are used in `include`, `exclude`, selectors, `--where` conditions and some command arguments. A glob matches the whole text, case-sensitively:

| Syntax | Matches |
|---|---|
| `*` | Any run of characters except `/`. |
| `**` | As a whole segment: zero or more path segments. |
| `?` | One character except `/`. |
| anything else | Itself. There are no character classes and no escapes. |

A glob is invalid (an `Error::Config`) when it is empty, contains a backslash, or uses `**` inside a segment such as `a**b`. Examples: `backend/**` matches everything under `backend/`; `**/generated/**` matches any `generated` directory at any depth; `web/src/*.ts` matches one level of TypeScript files.

## Selectors

A selector is an object `{ "kind", "key"?, "file"?, "data"? }` that matches facts:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `kind` | string | required | The fact kind: one of `file`, `unparsed`, `module`, `fn_def`, `type_def`, `impl`, `import`, `call`, `route`, `api_call`, `env_var`, `sql_in_code`, `table`, `column`, `grant`, `trigger`, `sql_function`, `table_access`, `const_def`. A `const_def` fact is a Rust `const` or `static` item, keyed like a function (`<module key>::<NAME>`), with data `name`, `is_static`, `end_line` and `hash`. |
| `key` | string (glob) | none | Glob over the fact key. Omitted means any key. |
| `file` | string (glob) | none | Glob over the fact's repository-relative file. Omitted means any file. |
| `data` | object (string to glob) | `{}` | One glob per data field name. A selector matches when every listed field's text matches its glob. A field that is missing or null never matches. |

Data field values are matched as text: strings as they are, numbers and booleans in JSON form, arrays as their elements joined with `,`. The data fields of each kind are the fields of its fact data (for example a `call` fact has `owner`, `callee`, `name`, `qualifier`, `receiver`, `is_method`, `is_macro`; a `table_access` fact has `owner`, `table`, `op`).

The owner of a fact is its `data.owner` field. Owner exists on calls, routes, API calls, environment variables, SQL in code, table accesses, and grants inside functions. Several rule kinds reason about the owner function of a fact.

Example selector, "inserts into the payments table":

```json
{ "kind": "table_access", "key": "payments", "data": { "op": "insert" } }
```

## Rules

A rule is an object with `id` (string, required, unique), `note` (string, optional), `kind` (required) and `ignore` (optional), plus the fields its kind requires.

`ignore` is an array of objects `{ "key", "reason" }`, both non-empty strings: the rule does not report the hit with that hit key (see "Hit keys"). The reason says why the hit is not a violation and stays in the file for review. A key listed twice is an error. A key that matches no hit is not an error, but `check` prints a warning for it and lists it in the JSON under `ignore_unused`; the number of ignored hits is in the summary (`N ignored`) and in the JSON under `ignored`. A `requires-before` hit key names the function and the `when` key, so one entry also silences later chains to the same function: give the reason with `file:line`. Validation checks the required fields per rule kind and reports problems as an `Error::Config` naming the object and the field.

| Field | Type | Used by | Meaning |
|---|---|---|---|
| `when` | selector | `requires-before` | Facts that trigger the requirement. Required. |
| `require` | selector | `requires-before` | Facts that must be found. Required. |
| `depth` | number | `requires-before` | How many caller levels above the owner may also satisfy the requirement. Default `0`. |
| `include_ancestor_callees` | boolean | `requires-before` | Whether a function called directly by one of those callers may also satisfy the requirement. Default `false`. Not with `all_paths`. |
| `all_paths` | boolean | `requires-before` | Whether every caller chain must meet the requirement before the call that leads to the fact (see below). Default `false`. |
| `first` | selector | `order-in-function` | Fact that must come first. Required. |
| `then` | selector | `order-in-function` | Fact that must come after `first`. Required. |
| `only_with_first` | boolean | `order-in-function` | Whether only owners that also have a `first` fact are checked, so the rule reports a wrong order and not a missing `first`. Default `false`. |
| `left` | selector | `sources-agree` | One side of the comparison. Required. |
| `right` | selector | `sources-agree` | Other side of the comparison. Required. |
| `fields` | array of string | `sources-agree` | Fact fields compared as tuples; `key` means the fact key. Default `["key"]`. |
| `direction` | `"both"` or `"left-in-right"` | `sources-agree` | `both`: the sets must be equal. `left-in-right`: only left tuples missing on the right are hits. Default `"both"`. |
| `minus` | selector | `sources-agree` | Facts that cancel an earlier left fact with the same tuple, for example a `REVOKE` in a later migration. Optional. |
| `dropped` | selector | `sources-agree` | Facts whose key names an object that ends there, for example a `DROP TABLE` in a later migration; they cancel every earlier left fact on that object. Optional. |
| `source` | selector | `has-target` | Facts that must each have a target. Required. |
| `target` | selector | `has-target` | Facts that can serve as targets. Required. |
| `source_field` | string | `has-target` | Field of the source fact to compare. Default `"key"`. |
| `target_field` | string | `has-target` | Field of the target fact that must be equal. Default `"key"`. |
| `selector` | selector | `forbidden`, `review-on-change` | Facts that must not exist (`forbidden`) or that are watched (`review-on-change`). Required. |

### `requires-before`

For every fact matching `when`, take its owner function. The rule holds when a fact matching `require` has as owner: the owner itself, or a transitive caller up to `depth` levels (following incoming `calls` edges of any confidence), or, with `include_ancestor_callees`, a function called directly by one of those callers. Otherwise there is one hit per owner and `when` key.

Read this rule as "some path has the requirement", not "every path has it". For a security rule this matters:

- One caller with the requirement is enough. Other callers of the same owner that never meet the requirement do not produce a hit.
- Order is not checked. A requirement that runs after the guarded fact, or in a branch that does not always run, still satisfies the rule. Use `order-in-function` when order within one function matters.
- Every `calls` edge counts, `guessed` ones included, so a call that only might reach the owner can satisfy the rule.

With `all_paths`, the rule reads "every path has it, in order". A function takes the requirement before line `L` when it has a `require` fact before `L`, or calls (on an earlier line) a function that has one. The owner must take it before the `when` fact; if it does not, every caller must take it before its call to the owner, or have all its own callers do so, up to `depth` levels. `calls` and `handles` edges count except `guessed` ones. A chain that reaches a function without callers, or `depth` levels, without the requirement is a hit, one per owner and `when` key; the message names the chain. A call cycle ends a chain as satisfied.

Such a hit is uncertain when its chain is cut by `depth`, ends at a function whose only callers are `guessed` edges, or ends in a call cycle (every caller of the last function is already on the chain): the requirement may be taken higher up, by the guessed caller, or where the cycle is entered. A function that only calls itself counts as one without callers. `check` lists uncertain hits apart and does not count them (see "How `check` compares against the line"). A certain chain, ending at a function with no callers at all, is reported in preference to an uncertain one. Read uncertain hits by hand; for one cut by `depth`, raise `depth` first.

Example: code that writes payments must check the permission first.

```json
{
  "id": "payments-need-permission",
  "kind": "requires-before",
  "note": "Code that writes payments must check the permission first",
  "when": { "kind": "table_access", "key": "payments", "data": { "op": "insert" } },
  "require": { "kind": "call", "data": { "name": "check_permission" } },
  "depth": 2
}
```

A hit means: some owner inserts into `payments`, and neither that owner nor any caller up to 2 levels above it calls `check_permission`.

### `order-in-function`

For every owner that has a fact matching `then`, the rule holds when the same owner has a fact matching `first` on an earlier line than the first `then` fact. Otherwise there is one hit per owner. With `only_with_first`, an owner without any `first` fact holds, so only a `first` that comes too late is a hit.

Example: stock is reserved before an order row is created.

```json
{
  "id": "reserve-before-order-insert",
  "kind": "order-in-function",
  "first": { "kind": "call", "data": { "name": "reserve_stock" } },
  "then": { "kind": "table_access", "key": "orders", "data": { "op": "insert" } }
}
```

A hit means: a function inserts into `orders` but has no `reserve_stock` call on an earlier line of the same function.

### `sources-agree`

The set of value tuples of `fields` over facts matching `left` must equal the set over facts matching `right` (`key` in `fields` means the fact key). There is one hit per tuple present on only one side. With `"direction": "left-in-right"` only left tuples missing on the right are hits. A left fact is dropped when a fact matching `minus` has the same tuple and a later position (file name, then line; migrations sort by their timestamp prefix). It is also dropped when a fact matching `dropped` has a later position and a key equal to the left fact's `object` field (its key when it has no `object` field); a left fact after that position is on the object created again and stays. A dropped table also ends the sequences of its identity and `serial` columns (`<table>_<column>_seq`), which PostgreSQL drops with it. Only the name is compared: a `DROP TABLE` also ends grants on a function of the same name.

For `grant` facts compared through the fields `privilege` and `column` (not `key`), a tuple is also covered by the same grant on the whole object (empty `column`) and by `ALL` on the column or the whole object; the same coverage applies to `minus`. A column grant `GRANT SELECT (a, b)` gives one fact per column. A grant run by `EXECUTE format('GRANT ..', ..)` in a function or `DO` body is read when every argument of `format` is a string literal or the variable of an enclosing `FOREACH v IN ARRAY ARRAY['a', 'b'] LOOP`: one grant per value. An enclosing `IF v IN ('a', 'b') THEN` or `IF v = 'a' THEN` (also as `ELSIF`) on that variable keeps only those values; any other condition, and an `ELSE` branch, keeps all. A grant whose object or role comes from a query loop (`FOR t IN SELECT ..`) or a function call gives no fact; list such known gaps in `ignore`.

Example: the routes the server offers are exactly the routes the frontend calls.

```json
{
  "id": "routes-match-client-calls",
  "kind": "sources-agree",
  "left": { "kind": "route" },
  "right": { "kind": "api_call" },
  "fields": ["key"]
}
```

A hit means: a route key such as `GET /orders/{}` exists on only one side - a route no client calls, or a client call with no route. (The `ANY` route special case is specified for `has-target`, below, not for this comparison.)

### `has-target`

Every fact matching `source` must have a fact matching `target` whose `target_field` equals its `source_field`. There is one hit per source fact without a target.

For `api_call` to `route` keys, the paths are compared segment by segment: the method must be equal or the route's `ANY`, the segment count equal, and a route `{}` matches any segment. A call `{}` against a constant route segment is a weak match; it holds only when a constant segment after the API client base path (`base_path` of the API client patterns) agrees too. A call whose first segment after a base path is `{}` (`` `${base}/x` ``) has an unknown prefix and is skipped, not reported. A route `{*rest}` matches one segment only.

A route is used only when it has a constant segment after the API base path; a route made only of parameters there cannot tell which calls it serves.

Example: every API call the frontend makes has a route.

```json
{
  "id": "client-calls-have-routes",
  "kind": "has-target",
  "source": { "kind": "api_call" },
  "target": { "kind": "route" },
  "source_field": "key",
  "target_field": "key"
}
```

A hit means: a call such as `POST /orders` (by key) has no route with the same key, and no `ANY /orders` route either.

### `forbidden`

One hit per fact matching `selector`.

Example: application code never deletes payments directly.

```json
{
  "id": "no-direct-payment-deletes",
  "kind": "forbidden",
  "selector": { "kind": "table_access", "key": "payments", "data": { "op": "delete" } }
}
```

A hit means: a `table_access` fact records a delete on `payments`, wherever it occurs.

### `review-on-change`

One hit per fact matching `selector`, keyed by the fact's content: its `hash` data field (definitions such as `fn_def`, `type_def` and `const_def`), else a hash of all its data. On the line every such hit is already present, so `check` reports exactly the matching facts that a worktree adds or changes, and nothing for unchanged ones. Use it for code whose every change needs a person's review.

Example: any change to how a refund is approved is reviewed.

```json
{
  "id": "review-refund-approval",
  "kind": "review-on-change",
  "selector": { "kind": "fn_def", "key": "*::approve_refund" }
}
```

A hit means: the worktree adds a function matching `*::approve_refund` or changes one; formatting alone does not change the hash.

## Hit keys

Every hit has a stable key that does not contain a line number, so a hit keeps its identity when lines shift:

```text
<rule id>|<owner or fact key>|<extra>
```

The `extra` part depends on the rule kind:

| Rule kind | `extra` |
|---|---|
| `requires-before` | The `when` key. |
| `order-in-function` | Empty. |
| `sources-agree` | The tuple joined with `,`, with the side it was found on. |
| `has-target` | The source key. |
| `forbidden` | The fact file and key. |
| `review-on-change` | The fact's content hash; the middle part is the fact key. |

For example, a hit of `payments-need-permission` for an owner `app::orders::create_order` inserting into `payments` has the key `payments-need-permission|app::orders::create_order|payments`.

## How `check` compares against the line

`arlineo check [--worktree W]` (default: the current worktree) computes all rule hits in the worktree view - the line's base index with that worktree's changed files, as they are on disk, swapped in. It then computes the hits of the line view, which is the base index of the line's current commit. A hit is reported only when its `(rule, key)` pair is not present among the line's hits; a certain hit is compared only with the line's certain hits, so an uncertain hit on the line does not hide a certain one with the same key. Violations the line already has are therefore not reported, whoever added them, and a violation that only moved to another line is not new, because hit keys contain no line number. What remains comes from the worktree's changed files, but it can still be a hit that a change on the line made possible (for example a new rule or a removed check), so read each hit before treating it as your own. Each hit is printed as `<file>:<line> Rule <id>: <message>`. Uncertain new hits (`requires-before` with `all_paths`) are printed after them as `uncertain (not counted): <file>:<line> Rule <id>: <message>`, listed in the JSON under `uncertain`, and do not change the exit code. When at least one certain new hit remains, `check` reports them and the command exits with code 3; otherwise it exits with code 0.
