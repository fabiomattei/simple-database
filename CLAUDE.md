# CLAUDE.md

`sdb` is a schema-first database tool in Rust: a **desktop app** (`sdb-gui`, egui/eframe) and a
CLI (`sdb`). Both sit on one library crate (`simple_database`, `src/lib.rs`). You write schemas
in a small DSL (`.dbs`). The tool validates them, generates DDL for SQLite/Postgres/MySQL, draws
ER diagrams, diffs schemas into migrations, pushes to and pulls from live SQLite, PostgreSQL and
MySQL databases, seeds fake data and runs SQL. The README documents the DSL, every CLI command and every GUI tab.

## Commands

```sh
cargo build                     # target/debug/sdb (CLI) and target/debug/sdb-gui (desktop app)
cargo test                      # unit tests live in each core module's `mod tests`
cargo clippy --all-targets      # keep at zero warnings
cargo run --bin sdb-gui -- schema.dbs app.db --tab diagram
```

- `cargo test` and `cargo clippy` do **not** rebuild the binaries. Run `cargo build` before
  manual end-to-end checks, or you are testing a stale binary.
- The first build (and any release build) is slow, 1–2 minutes, because of eframe/wgpu. Every
  binary pulls in all package dependencies.
- Do manual end-to-end runs in a scratch directory, e.g.
  `sdb init && sdb push schema.dbs --db app.db -y && sdb seed --db app.db`.
- Stop a running app with `pkill -x sdb-gui`. `pkill -f sdb-gui` also matches, and kills, the
  shell whose command line contains it.

## Architecture

Everything flows through the dialect-independent model in `src/schema.rs`
(`Schema` → `Table` → `Column` / `Index`, `DataType`, `DefaultValue`, `ForeignKey`).
Structural edits go through `Schema::rename_table` / `drop_table` / `rename_column` /
`drop_column`, which keep composite keys, indexes and foreign keys pointing at the right names.

Core library (`src/`, exported from `lib.rs`):

| Module | Role |
|---|---|
| `lib.rs` | Module list plus `STARTER_SCHEMA` (`include_str!` of `examples/blog.dbs`) |
| `parser.rs` | `.dbs` DSL → `Schema`. Hand-written tokenizer: `(...)` groups become one raw `Paren` token, and newline, `,` and `;` all end a statement. `outline` / `locate`: lenient table/column → line scan for the editor, works on text that doesn't parse |
| `format.rs` | `Schema` → `.dbs` text (`pull`, `fmt`, shell `.schema`, GUI Pull). `align` re-aligns columns in place keeping comments (GUI Format); it returns `None` unless the result parses to the same schema. `patch` rewrites only the changed tables of a text (SQL tab and Diagram edits → schema text); `patch_renamed` also reprints renamed tables in place |
| `validate.rs` | Errors (unusable schema) and warnings (design lint); `print_issues` is the CLI's ANSI printer |
| `ddl.rs` | `Dialect` enum and all CREATE/DROP SQL; `quote()` quotes only when needed; `canonical()` = the schema as a dialect reports it back |
| `diff.rs` | `diff(old, new, dialect) -> Migration` (changes, warnings, statements). SQLite falls back to the create-copy-drop-rename table rebuild when `ALTER TABLE` can't make the change |
| `diagram.rs` | Text diagrams: Mermaid / Graphviz DOT (the GUI canvas is separate, in `gui/er.rs`) |
| `data.rs` | Data tab logic: `View` (filters, raw WHERE, sort → `page_sql` / `count_sql` / `all_sql`), `literal` (a `Cell` as SQL for a column type; checks text against the type), `RowChange` (staged update/insert/delete by primary key) |
| `safety.rs` | `SafeMode` (off / warn / confirm / read-only) and `classify` (read, write, dangerous) over comment- and string-free words |
| `history.rs` | `QueryLog`: query history (`history.json`, 500 entries, 0600) and saved queries (`queries.json`) in `config_dir()` |
| `import.rs` | CSV/TSV/JSON/JSONL → `Rows`, `plan` (match columns by name), batched INSERTs, `import` in one transaction |
| `discover.rs` | Find databases in a project folder: `.env` URLs and Laravel `DB_*`, compose services (lenient line-based YAML scan), Prisma datasource |
| `connections.rs` | Saved connections (`Store`: `connections.json` + 0600 `secrets.json` in `config_dir()`). `Connection::from_target` / `target` convert to and from URLs. `resolve("@name")` is called by `Database::open` |
| `db.rs` | `Database`: one live connection (SQLite path or `postgres://` / `mysql://` URL). `introspect`, `run` / `run_limited`, `row_counts`, `migration`, `apply_migration`, `normalize_checks`. Also `Cell` / `QueryResult`, `split_statements` and `schema_from_sqlite_ddl` (DDL → `Schema` via an in-memory SQLite; the GUI's editable SQL tab) |
| `sqlite.rs` | SQLite introspection, `run_limited` (multi-statement via `rusqlite::Batch`), `apply_migration` |
| `pg.rs` | Postgres via the `postgres` crate (NoTls): catalog introspection; queries are prepared for column types, then run with the simple protocol so every value arrives as text |
| `mysqldb.rs` | MySQL via the `mysql` crate (pure Rust, no TLS): `information_schema` introspection; checks are parsed from `SHOW CREATE TABLE` |
| `seed.rs` | Fake data. Seedable SplitMix64 RNG (no `rand` crate), heuristics based on column names, retries on constraint errors |
| `shell.rs` | CLI REPL (rustyline) and its `.meta` commands |
| `output.rs` | CLI table / csv / json / markdown rendering (comfy-table 8: `load_style`, not `load_preset`); `export` to files (adds TSV, JSONL, HTML, SQL INSERTs) |

Binaries:

| Path | Role |
|---|---|
| `src/main.rs` | `sdb` clap CLI; `load_schema` accepts a `.dbs` file **or** a SQLite file (detected by magic header) |
| `src/gui/main.rs` | `sdb-gui`: `App` state, menus, sidebar, one `tab_*` method per tab, modal `Dialog`s, `Settings` persisted via eframe storage |
| `src/gui/er.rs` | ER diagram canvas painted by hand (layout by FK depth, drag, pan, zoom). When editable, `ui` returns `ErAction`s (context menu, double-click, drag a column handle to link) that `App::diagram_action` applies via `format::patch_renamed` |
| `src/gui/table_edit.rs` | Diagram's table editor dialog: column grid → DSL lines → `parser::parse`, renames via `Schema::rename_table` / `rename_column`; refuses edits that add validation errors |
| `src/gui/conns.rs` | Connections dialog (`Manager`): list with search, favorites, recent, groups; form, test, save, duplicate, delete. Re-reads the store from disk before each change |
| `src/gui/grid.rs` | Read-only virtualized result grid (egui_extras `TableBuilder`): Query results, import preview |
| `src/gui/data_view.rs` | Data tab: `DataView` (page, filters, sort, `Staged` edits/deletes/new rows), editable grid; returns `DataAction`s that `App` carries out (load, save, import, export) |
| `src/gui/editor.rs` | `code_editor`: `TextEdit` plus gutter and problem `Mark`s, line commands (`command`, pure and unit-tested), find/replace bar, column selection (multi-caret, consumed from input before `TextEdit` sees it) and autocomplete (`Vocabulary` from a `Schema`; context-aware for the DSL). `goto_line` / `caret_line` talk to it by id. Use `take_key` for exact-modifier shortcuts: `consume_key` also matches with Shift held |
| `src/gui/highlight.rs` | DSL/SQL highlighter, keyword lists (also used by autocomplete), `code_view` |

`examples/blog.dbs` is not just an example. It is the output of `sdb init`, the GUI's "New
schema" and its first-run content. The GUI's Help → Schema syntax dialog is built from its
leading `#` comment lines. Keep that header accurate when the DSL changes.

## Invariants: keep these true

- **Round-trip with Postgres/MySQL**: introspecting what `push` created must equal
  `canonical(normalize_checks(s))`. `ddl::canonical` holds what the server changes for sure
  (MySQL: indexed `text` → `varchar(255)`, padded decimal defaults; both: `DEFAULT NULL`
  dropped, composite-pk columns NOT NULL). `diff` applies it to both sides. Check expressions
  are rewritten by the server itself in a scratch temp table (`Database::normalize_checks`,
  cached). Tests: `db::tests::{postgres,mysql}_round_trip`, which run only with
  `SDB_TEST_POSTGRES` / `SDB_TEST_MYSQL` set to a scratch database URL (they drop all tables).
- **Round-trip with SQLite**: `introspect(execute(schema_sql(s, Sqlite)))` must equal `s`, so
  push on an up-to-date database reports no changes. This is why SQLite DDL emits specific
  declared types (`BOOLEAN`, `VARCHAR(n)`, `DECIMAL(p,s)`, `TIMESTAMP`) instead of only the
  affinity types. It is also why bare-word default expressions are lowercased in both the parser
  and introspection. Test: `sqlite::tests::introspection_round_trips_generated_ddl`.
- **Round-trip with the DSL**: `parse(format_schema(s)) == s`. Test: `format::tests::round_trips`.
- **Any DSL change** touches `parser.rs` (parse), `format.rs` (print), the round-trip tests and
  the `examples/blog.dbs` header. It usually also touches `ddl.rs`, `diff.rs`, `sqlite.rs`
  introspection, the README syntax table and the keyword lists in `gui/highlight.rs` (which also feed autocomplete).
  A new column modifier also needs a field in `gui/table_edit.rs`, which writes DSL lines and parses them.
- Foreign key constraints are named `fk_<table>_<column>`. Postgres/MySQL migrations rely on
  that name to drop them.
- Check constraints are named `<table>_<column>_check` (Postgres picks this name; MySQL DDL
  sets it as a table constraint), and diffs rely on it.
- `Database::apply_migration` is the only way migrations are executed (CLI `push` and the GUI's
  Push). It runs statements one at a time. On SQLite, `PRAGMA foreign_key_check` is a gate: any
  violation aborts. On SQLite and Postgres a failure issues an explicit `ROLLBACK`, because the
  GUI keeps its connection open. Don't replace this with a single `execute_batch`.
- The parser forces `nullable = false` on inline `pk` columns; composite-pk columns keep their
  declared nullability.
- **Safe mode lives in `Database`.** `run_limited`, `execute`, `apply_migration` and
  `write_statements` refuse writes when read-only (`guard` / `ensure_writable`), and
  `set_safe_mode` also makes the server session read-only. Callers only ask the user:
  `needs_confirmation` (statements) and `confirms_writes` (sdb's own writes: edits, import,
  seed). `Database::open` takes the mode from the saved connection.
- Row edits and imports go through `Database::write_statements`: one transaction, rolled back on
  any failure; edits must hit exactly one row (MySQL reports 0 for an UPDATE that changes
  nothing, so that is allowed there).

## GUI notes

- egui **0.36** API: `App::ui(&mut self, ui, frame)` (not `update`); panels are
  `egui::Panel::top/bottom/left(..).show(ui, ..)` and `CentralPanel::default().show(ui, ..)`;
  `MenuBar::new().ui(..)`; `ui.fonts_mut(..)` for layout. Check the crate source under
  `~/.cargo/registry/src/` before assuming older egui APIs.
- egui's bundled fonts lack many symbols (▦ ▤ ◈ → showed as boxes). Check new icons on screen.
- Code editors don't wrap lines (column selection maps the pointer to line/column by x
  position), so put them in a `ScrollArea::both`.
- Read-only code goes through `highlight::code_view`, which forces a non-justified, non-wrapping
  layout. Plain labels inside `ui.columns` get justified and mangle SQL.
- All database work is synchronous on the UI thread (fine for local SQLite). Query results are
  capped with `sqlite::run_limited` (10,000 rows); Data pages are 200 rows.
- Started without file arguments, the app reopens the previous session's schema and database
  (eframe storage key `sdb-gui`). Pass files explicitly when testing for a predictable state.
- Diagram editing works only on the schema file, and only while the text parses: `patch` falls
  back to reprinting the whole file when the text doesn't parse, which would drop the user's
  comments. Diagram Undo keeps snapshots of the text and is cleared once the text changes some
  other way. Debug a dialog without clicking through a temporary startup hook that sets
  `self.dialog`, and remove the hook afterwards.
- The Migrate tab diffs in the open database's dialect (`Database::migration`), whatever
  dialect the SQL tab shows. Saved settings keep server URLs without the password.
- **Checking it visually on this machine** (GNOME Wayland, no xdotool/Xvfb):
  1. Run it under XWayland: `env -u WAYLAND_DISPLAY target/debug/sdb-gui schema.dbs app.db --tab <tab> &`.
  2. Find the window by matching `_NET_WM_PID` against the ids in
     `xprop -root _NET_CLIENT_LIST`.
  3. Capture with `import -window <id> out.png`, then kill the process.
  4. Use `--tab schema|diagram|sql|migrate|query|data|seed` to reach each screen.

  Don't inject input with XTest. Under Wayland it goes through the compositor's remote-desktop
  permission and pops up a prompt for the user. Clicks and drags therefore can't be automated
  here, so verify button actions through the library code they call.

## Conventions

- Rust edition 2024. Let-chains (`if let ... && ...`) are used freely.
- Errors: `anyhow` in the CLI and library; `parser::ParseError` carries line numbers. The GUI
  shows errors in the status bar or inline, never panics on user input.
- Matching comment density: short `///` doc comments on non-obvious items, no narration.
- All live-database work goes through `db::Database`; don't call `sqlite::` / `pg::` /
  `mysqldb::` directly from the binaries. Test servers can be run with podman, e.g.
  `podman run -d --rm --name sdb-pg -e POSTGRES_USER=sdb -e POSTGRES_PASSWORD=sdb -p 55432:5432 postgres:17-alpine`.
- Logic that both binaries need belongs in the library, not in `src/main.rs` or `src/gui/`.
