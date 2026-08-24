# trace

**trace** is a static analysis tool for C codebases. It runs a custom preprocessor, parses translation units with [tree-sitter](https://tree-sitter.github.io/), performs Andersen-style field-sensitive pointer analysis, and exports call graphs and interprocedural argument-flow facts to SQLite.

Typical uses:

- Find **direct and indirect** call targets (function pointers, vtables, struct op tables).
- Trace **argument flow** from call-site actuals to callee formals.
- Query results with **`trace inspect`** or ad-hoc SQL.

## Build

```bash
cargo build --release
# binary: target/release/trace
```

Run the workspace test suite:

```bash
cargo test --workspace
```

## Quick start

```bash
trace analyze ./tests/fixtures/direct_call -o /tmp/trace.db
trace inspect /tmp/trace.db calls
trace inspect /tmp/trace.db calls --from main
trace inspect /tmp/trace.db calls --from caller --to helper
```

Analyze a large tree (parallel indexing, minimal SQLite export):

```bash
trace analyze /path/to/project -o /tmp/project.db --jobs 8
```

## CLI reference

### `trace analyze`

Analyze every C/C++ file (`.c`, `.cpp`, `.cc`, `.cxx`) under `TARGET` and write results to SQLite.

```text
trace analyze [OPTIONS] <TARGET>
```

| Option | Description |
|--------|-------------|
| `<TARGET>` | Root directory to scan recursively for `*.c` / `*.cpp` / `*.cc` / `*.cxx` files. |
| `-o`, `--output <PATH>` | Output database path. Default: `trace.db`. |
| `--include <PATH>` | Add a preprocessor `#include` search path. Repeatable. |
| `-D <NAME>` | Define preprocessor macro `NAME=1`. Repeatable. |
| `-D <NAME=VALUE>` | Define macro with explicit value. Repeatable. |
| `--jobs <N>` | Parallel jobs for indexing (parse + lower). Default: logical CPU count. |
| `--full-export` | Export full IR detail: all types, all variables, PAG `locations`. Slower and produces a larger database. |
| `--debug-points-to` | Retain points-to sets during analysis and export the `points_to` debug table (requires PAG in memory). Implies keeping location data needed for export. |

**Progress output** (stderr):

```text
index: 24.2s (618 files, 11442 functions, 48406 flow)
analyze: 0.3s (25478 edges, 3468 indirect)
export: 0.1s
analysis complete: 11442 functions, 25478 call edges, 25803 arg-flow edges -> trace.db
```

**Examples**

```bash
# HDF-style tree with extra include roots
trace analyze ~/drivers_hdf_core -o /tmp/hdf.db \
  --include ~/drivers_hdf_core/framework/core/common/include \
  -D __LITEOS__ -D CONFIG_XXX=1

# Debug pointer analysis
trace analyze ./my_app -o /tmp/debug.db --debug-points-to --full-export
```

**Notes**

- **`.c` and `.cpp`-family files** are indexed as translation units. Headers are pulled in via `#include` during preprocessing, not analyzed as standalone TUs. C++ support is a pragmatic first step — see [docs/ANALYSIS.md](docs/ANALYSIS.md) for scope and imprecision.
- Line numbers in the database refer to **original** files on disk (resolved through the preprocessor's `LineMap`); call sites inside macro expansions attribute to the expansion site.
- Pass include paths that match your build; there is no `compile_commands.json` integration yet.
- **`static` functions** (internal linkage) and **file-scope `static` variables** are resolved within the defining translation unit. **`static` locals** inside functions are tracked as `fn_static` storage.

## `static` storage support

| Context | IR / export | Call / flow resolution |
|---------|-------------|------------------------|
| File-scope `static` function | `linkage = internal` | Direct calls and `CallReturn` via scope-aware name lookup (`file` + name) |
| File-scope `static` variable | `kind = file_static` | Persistent PAG location; name lookup scoped to file |
| Function-local `static` variable | `kind = fn_static` | Persistent PAG location within enclosing function |
| External (non-`static`) symbols | `linkage = external` | Global `fn_by_name` / `global_by_name` tables |

Same identifier in different `.c` files (each `static`) gets distinct IR ids; resolution uses the call site's file.

### `trace inspect`

Query an existing analysis database.

```text
trace inspect <DB> calls [--from FN] [--to FN] [--file SUBSTR]

Edges print as `caller (file:line) -> callee [deffile] (resolution)` — the
`[deffile]` bracket distinguishes same-name (e.g. `static`) functions defined
in different files; `--file` filters edges whose caller or callee file path
contains the substring.
```

| Option | Description |
|--------|-------------|
| `<DB>` | Path to SQLite file produced by `trace analyze`. |
| `--from <FN>` | Filter edges where the **caller** function name equals `FN`. |
| `--to <FN>` | Filter edges where the **callee** function name equals `FN`. |

Both filters may be combined. Output format:

```text
CallerFn -> CalleeFn (direct|indirect|ambiguous) at line N
```

Only **`call_edges`** are listed. Unresolved indirect call sites appear in `call_sites` but produce no line here unless an edge exists.

**Examples**

```bash
trace inspect /tmp/hdf.db calls --from NetIfSetAddr
trace inspect /tmp/hdf.db calls --from HdfSbufReadBuffer
trace inspect /tmp/hdf.db calls --to LiteNetSetIpAddr
```

For unresolved indirect calls, query SQL directly (see below).

## Analysis pipeline

```
discover .c/.cpp → preprocess → parse → lower IR → build PAG → solve → export SQLite
```

| Stage | What happens |
|-------|----------------|
| **Index** | Discover `.c` / `.cpp` files, preprocess TUs (custom preprocessor), parse with tree-sitter (C or C++ grammar per TU), lower to IR (functions, variables, flow constraints, call sites). |
| **Analyze** | Build pointer assignment graph (PAG), run Andersen-style solver, resolve direct calls by name (including file-local `static` functions), indirect calls via points-to to function locations. |
| **Export** | Write SQLite (minimal by default). |

Analysis is **may-analysis** (sound over-approximation): if a call target is possible, it may appear as an edge.

## Export modes

| Mode | Flags | Database contents |
|------|-------|-------------------|
| **Minimal** (default) | *(none)* | `analysis_run`, `files`, `functions`, filtered `call_sites`, `call_edges`, `arg_flow_edges`, variables referenced by arg-flow only, `diagnostics`. |
| **Full IR** | `--full-export` | Minimal plus all `types`, all `variables`, PAG `locations`. |
| **Points-to debug** | `--debug-points-to` | Adds `points_to` table (and retains PAG during analysis). Use with `--full-export` for complete debug dumps. |

### Call site export filter

`call_sites` rows are written when any of the following holds:

- The site has at least one **`call_edge`**.
- The site has at least one **`arg_flow_edge`**.
- The site is an **indirect** call (`is_direct = 0`), including unresolved function-pointer calls.

So unresolved indirect sites (e.g. `sbuf->impl->readBuffer` before a fix) still appear in `call_sites` even with zero `call_edges`.

## SQLite database schema

Schema version: **v1**. Foreign keys are declared in DDL; exports temporarily disable FK enforcement for bulk load speed.

### Entity relationship (overview)

```text
analysis_run
files ─┬─ functions ─┬─ call_sites ─┬─ call_edges → functions (callee)
       │             │              └─ arg_flow_edges → variables
       └─ variables (fn_id → functions, type_id → types)
types
locations (full export / debug)
points_to (debug only)
diagnostics
```

### `analysis_run`

Metadata for one `trace analyze` invocation.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Run id (always `1` per file). |
| `trace_version` | TEXT | trace crate version string. |
| `target_root` | TEXT | Absolute or normalized `<TARGET>` path. |
| `created_at` | TEXT | Unix timestamp (seconds). |
| `options_json` | TEXT | JSON: `include_paths`, `defines`, `include_points_to`, `full_detail`. |

### `files`

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Internal file id. |
| `path` | TEXT UNIQUE | Source file path. |
| `sha256` | TEXT | Content hash (may be empty in current export). |

### `functions`

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Internal function id. |
| `name` | TEXT | Linkage-visible name (may duplicate across TUs before merge; ids differ). |
| `file_id` | INTEGER FK → `files` | Defining or primary declaration file. |
| `line_start` | INTEGER | Start line (original file). |
| `line_end` | INTEGER | End line (currently same as start in export). |
| `linkage` | TEXT | `external`, `internal`, or `none`. |
| `signature` | TEXT | Placeholder signature string (`fn_<name>`). |
| `is_defined` | INTEGER | 1 if a body exists under the analyzed root; 0 covers prototypes and synthesized externals (libc, macro-referenced logging backends). |

**Index:** `functions(name)`.

### `call_sites`

One row per collected call (direct name call or indirect/function-pointer syntax).

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Call site id (matches IR `CallSiteId`). |
| `caller_fn_id` | INTEGER FK → `functions` | Containing function. |
| `file_id` | INTEGER FK → `files` | File containing the call. |
| `line` | INTEGER | Line (original file). |
| `col` | INTEGER | Column. |
| `callee_text` | TEXT | Surface syntax, e.g. `foo`, `p->handler`, `ndImpl->interFace->setIpAddr`. |
| `is_direct` | INTEGER | `1` = direct call by name; `0` = indirect / fn-ptr / unresolved name. |

### `call_edges`

Resolved caller → callee edges (one row per target; indirect sites may have multiple rows).

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Edge id. |
| `call_site_id` | INTEGER FK → `call_sites` | Call site this edge resolves. |
| `callee_fn_id` | INTEGER FK → `functions` | Resolved target function. |
| `resolution` | TEXT | `direct`, `indirect`, `ambiguous`, or `external` (statically resolved but bodyless under the analyzed root). |

**Indexes:** `call_edges(callee_fn_id)`, `call_edges(call_site_id)`.

### `arg_flow_edges`

Maps actual arguments at a call site to callee formal parameters (when wired by analysis). Each row has **either** a variable actual **or** a function-pointer actual.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Edge id. |
| `call_site_id` | INTEGER FK → `call_sites` | Call site. |
| `arg_index` | INTEGER | Zero-based argument index. |
| `actual_var_id` | INTEGER FK → `variables` | Variable passed at call site (`NULL` when actual is a function). |
| `actual_fn_id` | INTEGER FK → `functions` | Function passed as fn-ptr actual (`NULL` when actual is a variable). |
| `formal_var_id` | INTEGER FK → `variables` | Callee parameter variable. |

**Index:** `arg_flow_edges(call_site_id)`.

### `variables`

Present in full export, or minimal export for variables referenced by `arg_flow_edges` only.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Variable id. |
| `name` | TEXT | Source name or synthetic temp (`_gepN`, `_loadN`, …). |
| `kind` | TEXT | `global`, `file_static`, `fn_static`, `param`, `local`. |
| `fn_id` | INTEGER FK → `functions` | Enclosing function (`NULL` for globals). |
| `type_id` | INTEGER FK → `types` | Type id. |
| `file_id` | INTEGER FK → `files` | Declaration file. |
| `line` | INTEGER | Declaration line. |

### `types`

Exported only with `--full-export`.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Type id. |
| `kind` | TEXT | `void`, `int`, `struct`, `ptr`, `fn_ptr`, … |
| `name` | TEXT | Display name. |
| `size` | INTEGER | Layout size in bytes. |
| `layout_json` | TEXT | JSON field layout for structs/unions. |

### `locations`

PAG abstract memory locations. Exported with `--full-export`.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Location id. |
| `kind` | TEXT | e.g. `Global`, `Local`, `FieldSummary`, `Function`. |
| `desc` | TEXT | Human-readable description. |
| `type_id` | INTEGER FK → `types` | Optional type. |

### `points_to`

Maps PAG variable nodes to abstract locations. Exported with `--debug-points-to`.

| Column | Type | Description |
|--------|------|-------------|
| `var_node_id` | INTEGER | PAG node id. |
| `loc_id` | INTEGER FK → `locations` | Points-to target. |

Primary key: `(var_node_id, loc_id)`.

### `diagnostics`

Preprocessor, parse, and analysis messages.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Diagnostic id. |
| `severity` | TEXT | `error`, `warning`, `info`. |
| `file_id` | INTEGER FK → `files` | Optional file. |
| `line` | INTEGER | Line number. |
| `message` | TEXT | Message text. |
| `stage` | TEXT | `preprocess`, `parse`, or `analysis`. |

## Example SQL queries

### Callees of a function

```sql
SELECT callee.name, ce.resolution, cs.line, cs.callee_text
FROM call_edges ce
JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = cs.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE caller.name = 'HdfSbufReadBuffer';
```

### Unresolved indirect call sites

```sql
SELECT caller.name, cs.line, cs.callee_text
FROM call_sites cs
JOIN functions caller ON caller.id = cs.caller_fn_id
LEFT JOIN call_edges ce ON ce.call_site_id = cs.id
WHERE cs.is_direct = 0 AND ce.id IS NULL
ORDER BY caller.name, cs.line;
```

### All indirect resolutions for a call expression pattern

```sql
SELECT caller.name, callee.name, cs.line
FROM call_edges ce
JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = cs.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE ce.resolution = 'indirect'
  AND cs.callee_text LIKE '%readBuffer%';
```

### Callers of a function

```sql
SELECT caller.name, ce.resolution, cs.line
FROM call_edges ce
JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = cs.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE callee.name = 'LiteNetSetIpAddr';
```

### Argument flow at a call site (variable actuals)

```sql
SELECT cs.line, af.arg_index, av.name AS actual, fv.name AS formal
FROM arg_flow_edges af
JOIN call_sites cs ON cs.id = af.call_site_id
JOIN variables av ON av.id = af.actual_var_id
JOIN variables fv ON fv.id = af.formal_var_id
WHERE af.actual_var_id IS NOT NULL;
```

### Argument flow (function-pointer actuals)

```sql
SELECT cs.line, af.arg_index, f.name AS actual_fn, fv.name AS formal
FROM arg_flow_edges af
JOIN call_sites cs ON cs.id = af.call_site_id
JOIN functions f ON f.id = af.actual_fn_id
JOIN variables fv ON fv.id = af.formal_var_id
WHERE af.actual_fn_id IS NOT NULL;
```

## Project layout

```
crates/
  trace-preproc/   Custom C preprocessor (#include, #define, conditionals)
  trace-parse/     tree-sitter parsing, IR lowering, TU merge
  trace-ir/        Shared IR (types, symbols, flow constraints)
  trace-analysis/  PAG construction, Andersen solver, call graph
  trace-db/        SQLite schema and export
  trace-cli/       `trace` binary (`analyze`, `inspect`)
docs/              Design docs (architecture, analysis, preprocessor, schema)
tests/fixtures/    Integration test C corpora
```

## Limitations

- **C++ first step** — namespaces, overloads (arity), classes/virtual dispatch, ctors/dtors are modeled; implicit `this->member` accesses, type-based overload ranking, and templates beyond name-stripping are not (see docs/ANALYSIS.md).
- **May-analysis** — indirect calls can list multiple targets; absence of an edge does not prove unreachability.
- **No path sensitivity** — all branches and paths are merged.
- **Preprocessor subset** — not gcc/clang compatible for all extensions; see [docs/PREPROCESSOR.md](docs/PREPROCESSOR.md).
- **Include paths** — must be supplied manually via `--include` / `-D`; no `compile_commands.json` yet.
- **Line numbers** — refer to preprocessed TUs; map back to original sources manually when needed.

## Further reading

- [Architecture](docs/ARCHITECTURE.md)
- [Analysis algorithm](docs/ANALYSIS.md)
- [Preprocessor spec](docs/PREPROCESSOR.md)
- [SQLite schema (detailed)](docs/SQLITE_SCHEMA.md)
- [Roadmap](docs/ROADMAP.md)
- [Contributor / agent guide](AGENTS.md)

## License

MIT
