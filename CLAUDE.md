# CLAUDE.md

`sdb` is a schema-first database tool in Rust: a **desktop app** (`sdb-gui`, egui/eframe) and a
CLI (`sdb`). Both sit on one library crate (`simple_database`, `src/lib.rs`). You write schemas
in a small DSL (`.dbs`). The tool validates them, generates DDL for SQLite/Postgres/MySQL, draws
ER diagrams, diffs schemas into migrations, dumps and restores databases, pushes to and pulls from live SQLite, PostgreSQL and
MySQL databases, seeds fake data and runs SQL. The README documents the DSL, every CLI command and every GUI tab.

**It must build and work on macOS, Linux and Windows**, both binaries. See [Cross-platform](#cross-platform).

## Commands

```sh
cargo build                     # target/debug/sdb (CLI) and target/debug/sdb-gui (desktop app)
cargo test                      # unit tests in each module's `mod tests`; tests/cli.rs runs the sdb binary end to end
cargo clippy --all-targets      # keep at zero warnings
cargo run --bin sdb-gui -- schema.dbs app.db --tab diagram
```

- CI (`.github/workflows/ci.yml`) runs clippy with `-D warnings` and `cargo test` on Linux with
  Postgres and MySQL service containers (`SDB_TEST_POSTGRES` / `SDB_TEST_MYSQL` set), so server
  tests must pass too, and on macOS and Windows without them (SQLite only).
- `cargo test` and `cargo clippy` do **not** rebuild the binaries (`tests/cli.rs` does build `sdb`). Run `cargo build` before
  manual end-to-end checks, or you are testing a stale binary.
- The first build (and any release build) is slow, 1–2 minutes, because of eframe/wgpu. Every
  binary pulls in all package dependencies.
- Do manual end-to-end runs in a scratch directory, e.g.
  `sdb init && sdb push schema.dbs --db app.db -y && sdb seed --db app.db`.
- Stop a running app with `pkill -x sdb-gui` (macOS, Linux) or `taskkill /IM sdb-gui.exe /F`
  (Windows). `pkill -f sdb-gui` also matches, and kills, the shell whose command line contains it.

## Architecture

Everything flows through the dialect-independent model in `src/schema.rs`
(`Schema` → `Table` → `Column` / `Index`, `DataType`, `DefaultValue`, `ForeignKey`).
A single-column foreign key is always `Column::references`; only keys over several columns are in
`Table::foreign_keys` (`TableForeignKey`; the parser moves a one-column `fk (a) -> t (x)` onto the
column). `Table::all_foreign_keys` gives both in one shape, for code that doesn't care.
Structural edits go through `Schema::rename_table` / `drop_table` / `rename_column` /
`drop_column`, which keep composite keys, indexes and foreign keys pointing at the right names.

Core library (`src/`, exported from `lib.rs`):

| Module | Role |
|---|---|
| `lib.rs` | Module list plus `STARTER_SCHEMA` (`include_str!` of `examples/blog.dbs`) |
| `parser.rs` | `.dbs` DSL → `Schema`. Hand-written tokenizer: `(...)` groups become one raw `Paren` token, and newline, `,` and `;` all end a statement. A quoted type is taken as written (`"int unsigned"`). `outline` / `locate`: lenient table/column → line scan for the editor, works on text that doesn't parse |
| `format.rs` | `Schema` → `.dbs` text (`pull`, shell `.schema`, GUI Pull); `type_text` quotes types that wouldn't parse back. `align` re-aligns columns in place keeping comments (`fmt`, GUI Format); it returns `None` unless the result parses to the same schema. `patch` rewrites only the changed tables of a text (SQL tab and Diagram edits → schema text); `patch_renamed` also reprints renamed tables in place |
| `validate.rs` | Errors (unusable schema) and warnings (design lint); `print_issues` is the CLI's ANSI printer |
| `ddl.rs` | `Dialect` enum and all CREATE/DROP SQL; `quote()` quotes only when needed; `canonical()` = the schema as a dialect reports it back |
| `diff.rs` | `plan(old, new, dialect) -> Plan` (renames, created / altered / dropped tables, `TableDiff`s); `diff` renders a plan as a `Migration` (changes, warnings, statements). SQLite falls back to the create-copy-drop-rename table rebuild when `ALTER TABLE` can't make the change. `apply_renames` first turns `was` hints (`Table::was` / `Column::was`) into RENAMEs, including the constraints named after the old names; `canonical` strips the hints |
| `framework.rs` | `generate`: a diff as a Laravel (`framework/laravel.rs`) or Rails (`framework/rails.rs`) migration, `down` from the reverse plan. Works from `diff::plan` `Step`s; what the builder can't express (`*_blocker`) is sdb's SQL for that table. Uses sdb's constraint names. Hand-checked against the real frameworks: up/down leave `sdb push --dry-run` up to date on Postgres/MySQL |
| `diagram.rs` | Text diagrams: Mermaid / Graphviz DOT (the GUI canvas is separate, in `gui/er.rs`) |
| `data.rs` | Data tab logic: `View` (filters, raw WHERE, sort → `page_sql` / `count_sql` / `all_sql`), `literal` (a `Cell` as SQL for a column type; checks text against the type), `RowChange` (staged update/insert/delete by primary key) |
| `safety.rs` | `SafeMode` (off / warn / confirm / read-only) and `classify` (read, write, dangerous) over comment- and string-free words with their parenthesis depth. In read-only mode it is the only client-side gate, so anything that could lift the server-side read-only setting (`SET … READ WRITE`, `BEGIN READ WRITE`, `PRAGMA query_only(0)`, Postgres `set_config(...)` anywhere) or write from a "read" (`SELECT … INTO`, `PRAGMA x(v)`) must classify as a write |
| `history.rs` | `QueryLog`: query history (`history.json`, 500 entries, 0600) and saved queries (`queries.json`) in `config_dir()` |
| `integrity.rs` | `check_foreign_keys`: orphaned rows (a foreign key with no parent row), one `NOT EXISTS` count per key of a `Schema`; `orphan_condition` is also the Data tab's WHERE for **Show rows** (CLI `check --db`, GUI File → Check foreign keys) |
| `import.rs` | CSV/TSV/JSON/JSONL → `Rows`, `plan` (match columns by name), batched INSERTs, `import` in one transaction |
| `dump.rs` | `dump` a database to SQL (DDL in dependency order, batched INSERTs, Postgres `setval`; any target dialect); `prepare` + `restore` run a dump file through `write_statements` (BEGIN/COMMIT dropped, `--clean` drops existing tables, SQLite defers FKs, MySQL turns FK checks off) |
| `discover.rs` | Find databases in a project folder: `.env` URLs and Laravel `DB_*`, compose services (lenient line-based YAML scan), Prisma datasource |
| `connections.rs` | Saved connections (`Store`: `connections.json` + 0600 `secrets.json` in `config_dir()`, or the OS keychain via the `keyring` crate when `Connection::keychain`; tests swap in a `MemoryKeychain`, never touch the real one). `upsert` gives new remote connections `sslmode=verify-full`. `Connection::from_target` / `target` convert to and from URLs. `resolve("@name")` is called by `Database::open` |
| `db.rs` | `Database`: one live connection (SQLite path or `postgres://` / `mysql://` URL). `introspect` (`introspect_with_skipped` also lists views, triggers, functions, … it can't model; `format::note_skipped` puts them in a pulled schema's header), `run` / `run_limited`, `run_script` (a file statement by statement, with line numbers; optional single transaction), `row_counts`, `migration`, `apply_migration`, `normalize_checks`. Also `Cell` / `QueryResult`, `split_statements` (knows quotes, comments, `$$` bodies and trigger `BEGIN … END`), `statements_with_lines` and `schema_from_sqlite_ddl` (DDL → `Schema` via an in-memory SQLite; the GUI's editable SQL tab) |
| `sqlite.rs` | SQLite introspection, `run_limited` (multi-statement via `rusqlite::Batch`), `apply_migration`. Checks, enums and comments are read from the stored `CREATE TABLE` text (`table_items` splits it, keeping `--` / `/* */` comments) |
| `pg.rs` | Postgres via the `postgres` crate (TLS through `tokio-postgres-rustls`): catalog introspection; queries are prepared for column types, then run with the simple protocol so every value arrives as text |
| `mysqldb.rs` | MySQL via the `mysql` crate (pure Rust, rustls): `information_schema` introspection; checks are parsed from `SHOW CREATE TABLE` |
| `tls.rs` | TLS for both servers from libpq-style URL parameters (`sslmode`, default `prefer`; `sslrootcert`, `sslcert`, `sslkey`). `split_url` strips them before the URL reaches the driver; `client_config` builds the rustls config. `warning` flags remote hosts whose certificate isn't verified (`Database::tls_warning`; the CLI prints it, the GUI status bar shows it); `explain` adds advice to certificate errors |
| `seed.rs` | Fake data. Seedable SplitMix64 RNG (no `rand` crate), heuristics based on column names, retries on constraint errors |
| `shell.rs` | CLI REPL (rustyline) and its `.meta` commands; line history in `config_dir()/shell_history`, 0600 on Unix |
| `output.rs` | CLI table / csv / json / markdown rendering (comfy-table 8: `load_style`, not `load_preset`); `export` to files (adds TSV, JSONL, HTML, SQL INSERTs) |

Binaries:

| Path | Role |
|---|---|
| `src/main.rs` | `sdb` clap CLI; `load_schema` accepts a `.dbs` file **or** a SQLite file (detected by magic header) |
| `src/gui/main.rs` | `sdb-gui`: `App` state and its types, `Settings` persisted via eframe storage, connecting and the worker plumbing (`db_run`, `poll_background`), shortcuts, the `eframe::App` impl. The rest of `App` is split into `impl App` blocks in other files; methods called across them are `pub(crate)` |
| `src/gui/tabs/*.rs` | One file per tab (`schema`, `diagram`, `sql`, `migrate`, `query`, `data`, `seed`): its `tab_*` method and the actions that belong to it (e.g. `migration_for`, `run_query`, `load_data`) |
| `src/gui/panels.rs` | Menu bar, sidebar and status bar |
| `src/gui/dialogs.rs` | The modal `Dialog`s |
| `src/gui/file_menu.rs` | File menu actions on the database: dump, restore, run SQL file, check foreign keys (`Dialog::Integrity`) |
| `src/gui/er.rs` | ER diagram canvas painted by hand (drag, pan, zoom). `ErAction::OpenData` opens a table in the Data tab (`App::open_table_data`, shared with the sidebar). When editable, `ui` returns `ErAction`s (context menu, double-click, drag a column handle to link) that `App::diagram_action` applies via `format::patch_renamed` |
| `src/gui/er_layout.rs` | Where table boxes go, pure and unit-tested: `layout` (connected groups in FK-depth layers, barycenter ordering, tall layers wrapped, groups shelf-packed, loners last), `place` (one new box in free space next to its related tables), `separate` (remove overlaps; `pinned` boxes stay, used when a box grows) |
| `src/gui/table_edit.rs` | Diagram's table editor dialog: column grid → DSL lines → `parser::parse`, renames via `Schema::rename_table` / `rename_column`; refuses edits that add validation errors |
| `src/gui/conns.rs` | Connections dialog (`Manager`): list with search, favorites, recent, groups; form, test, save, duplicate, delete. Re-reads the store from disk before each change |
| `src/gui/window.rs` | One database per window: another database opens in a new `sdb-gui` process (`open`), its target passed through `SDB_GUI_*` environment variables so a typed password stays off the command line (`launch`) |
| `src/gui/worker.rs` | `Worker`: owns the open `Database` on its own thread, runs queued jobs and hands back replies; `Task`: one-off background work |
| `src/gui/grid.rs` | Read-only virtualized result grid (egui_extras `TableBuilder`): Query results, import preview |
| `src/gui/data_view.rs` | Data tab: `DataTabs` (one tab per open table, tab strip, `refresh` after a re-read), each a `DataView` (page, filters, sort, `Staged` edits/deletes/new rows), editable grid; returns `DataAction`s that `App` carries out (load, save, import, export) |
| `src/gui/editor.rs` | `code_editor`: `TextEdit` plus gutter and problem `Mark`s, line commands (`command`, pure and unit-tested), find/replace bar, column selection (multi-caret, consumed from input before `TextEdit` sees it) and autocomplete (`Vocabulary` from a `Schema`; context-aware for the DSL). `goto_line` / `caret_line` talk to it by id. Use `take_key` for exact-modifier shortcuts: `consume_key` also matches with Shift held |
| `src/gui/highlight.rs` | DSL/SQL highlighter, keyword lists (also used by autocomplete), `code_view` |

`examples/blog.dbs` is not just an example. It is the output of `sdb init` and the GUI's
Help → Example schema (the GUI itself starts with an empty schema and a welcome dialog to pick a
connection). The GUI's Help → Schema syntax dialog is built from its
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
  and introspection. Tests: `sqlite::tests::introspection_round_trips_generated_ddl` and `random_schemas_round_trip_through_sqlite` (names that need quoting).
- **Round-trip with the DSL**: `parse(format_schema(s)) == s`, and `align` leaves `format_schema` output alone. Tests: `format::tests::round_trips` and `random_schemas_round_trip` (keyword names, multi-word types, odd defaults). Add new DSL features to its generator.
- **Any DSL change** touches `parser.rs` (parse), `format.rs` (print), the round-trip tests and
  the `examples/blog.dbs` header. It usually also touches `ddl.rs`, `diff.rs`, `sqlite.rs`
  introspection, the README syntax table and the keyword lists in `gui/highlight.rs` (which also feed autocomplete).
  A new column modifier also needs a field in `gui/table_edit.rs`, which writes DSL lines and parses them.
- Foreign key constraints are named `fk_<table>_<column>` (`fk_<table>_<a>_<b>` over several
  columns). Postgres/MySQL migrations rely on that name to drop them.
- Enums (`DataType::Enum`): MySQL `ENUM(...)`; Postgres a type named `<table>_<column>_enum`
  (`ddl::enum_type_name`, created before the table by `create_table_statements`, dropped with the
  column or table, renamed with them, replaced through `<name>_new` when the values change); SQLite
  `TEXT` plus `CONSTRAINT enum CHECK (col IN (...))`, the name being what introspection looks for.
  Use `column_type_sql`, not `type_sql`, for a column's type.
- Comments (`comment '...'` on tables and columns): Postgres `COMMENT ON` (separate statements, so
  create tables with `create_table_statements`), MySQL `COMMENT`, SQLite a trailing `-- text` on the
  column's line (the table's on the `CREATE TABLE … (` line), one line only: `canonical` folds
  whitespace for SQLite. Any comment change rebuilds a SQLite table.
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
- Database work never runs on the UI thread. The open `Database` lives on a `worker::Worker`
  thread: `App::db_run(label, work, then)` queues `work(&mut Database)`, and `then(&mut App, result)`
  runs on the UI thread when `poll_background` (start of each frame) picks it up. Jobs run in
  order, one at a time. `Db` keeps what the UI shows (label, dialect, safe mode, schema, counts),
  and replies must check that the state they update is still current (see `load_data`, `migration_for`).
  `Worker::cancel` stops the running job through `Database::canceller` (SQLite interrupt,
  Postgres cancel token, MySQL `KILL QUERY` from a second connection); long loops in the library
  (`run_script`, `seed`) also check `cancel_requested` between statements.
  Connecting and other one-off work (Migrate's "From" database, Connections → Test) use `worker::Task`.
  Query results are capped with `run_limited` (10,000 rows); Data pages are 200 rows.
- Windows are separate processes, so files they share in `config_dir()` (history, saved queries,
  `data_tabs.json`, connections) are re-read before each change; eframe storage is last-writer-wins.
- Started without file arguments, the app reopens the previous session's schema file and shows
  the `Dialog::Welcome` connection picker (offering the last database) unless a database was given
  (eframe storage key `sdb-gui`). Pass files explicitly when testing for a predictable state.
- Diagram editing works only on the schema file, and only while the text parses: `patch` falls
  back to reprinting the whole file when the text doesn't parse, which would drop the user's
  comments. Diagram Undo keeps snapshots of the text and is cleared once the text changes some
  other way. Debug a dialog without clicking through a temporary startup hook that sets
  `self.dialog`, and remove the hook afterwards.
- The Migrate tab diffs in the open database's dialect (`Database::migration`), whatever
  dialect the SQL tab shows. Saved settings keep server URLs without the password.
- **Checking it visually.** Use `--tab schema|diagram|sql|migrate|query|data|seed` to reach each
  screen, and pass files explicitly. On macOS, `screencapture -x out.png` captures the screen (the
  terminal needs the Screen Recording permission). On Windows there is no scripted recipe; ask the
  user for a screenshot. On Linux (GNOME Wayland, no xdotool/Xvfb):
  1. Run it under XWayland: `env -u WAYLAND_DISPLAY target/debug/sdb-gui schema.dbs app.db --tab <tab> &`.
  2. Find the window by matching `_NET_WM_PID` against the ids in
     `xprop -root _NET_CLIENT_LIST`.
  3. Capture with `import -window <id> out.png`, then kill the process.

  Don't inject input with XTest. Under Wayland it goes through the compositor's remote-desktop
  permission and pops up a prompt for the user. Clicks and drags therefore can't be automated
  here, so verify button actions through the library code they call.

## Cross-platform

Supported: macOS, Linux and Windows. CI builds, lints and tests on all three; only Linux has the
Postgres/MySQL servers. Locally you can only check your own OS: `cargo check --target` for
another doesn't work, because the bundled SQLite and wgpu build C code for the target. So keep
code portable by construction, and let CI confirm it:

- **Paths**: `Path` / `PathBuf` and `join`, never a hard-coded `/` or `\`. Temporary files go in
  `std::env::temp_dir()`, never `/tmp`. Display paths with `.display()`.
- **Per-user files** go through `connections::config_dir()`: `SDB_CONFIG_DIR`, else `%APPDATA%\sdb`
  on Windows, else `$XDG_CONFIG_HOME/sdb`, else `~/.config/sdb` (macOS too). Don't read `HOME`
  directly: Windows doesn't set it. (The shell only falls back to an old `~/.sdb_history`.)
- **Unix-only APIs** (`std::os::unix`, file modes) sit behind `#[cfg(unix)]`, with the other
  platforms handled explicitly (`#[cfg(not(unix))]`), as in `connections::write_file`. Secrets and
  history are private through mode 0600 on Unix, and through the per-user `%APPDATA%` on Windows.
- **Line endings**: `.gitattributes` keeps the repository LF on every checkout (`blog.dbs` is
  compiled in, and tests compare `\n` output). Files written on Windows have CRLF: readers of
  schemas, SQL scripts and CSV/TSV must accept `\r\n` (the tokenizers treat `\r` as
  whitespace); sdb writes `\n`.
- **Keyboard shortcuts** in the GUI use `Modifiers::COMMAND` / `modifiers.command` (Cmd on macOS,
  Ctrl elsewhere), not `ctrl`, unless the key really is Ctrl on every platform.
- **No external programs**: don't shell out to `sh`, `which`, `open`, `xdg-open` and the like.
  Use crates that cover all three (e.g. `rfd` for file dialogs).
- **Terminal output** uses ANSI colors, which Windows 10+ terminals render. Don't rely on a Unix
  terminal for anything else (signals, `isatty` tricks, raw mode outside rustyline).
- **Tests** follow the same rules: build paths with `join` and `temp_dir()`, wrap Unix-only
  assertions (file modes) in `#[cfg(unix)]`. `tests/cli.rs` sets `HOME` and `SDB_CONFIG_DIR` to
  its scratch directory.

## Conventions

- Rust edition 2024. Let-chains (`if let ... && ...`) are used freely.
- Errors: `anyhow` in the CLI and library; `parser::ParseError` carries line numbers. The GUI
  shows errors in the status bar or inline, never panics on user input.
- Matching comment density: short `///` doc comments on non-obvious items, no narration.
- All live-database work goes through `db::Database`; don't call `sqlite::` / `pg::` /
  `mysqldb::` directly from the binaries. Test servers can be run with podman, e.g.
  `podman run -d --rm --name sdb-pg -e POSTGRES_USER=sdb -e POSTGRES_PASSWORD=sdb -p 55432:5432 postgres:17-alpine`.
- Logic that both binaries need belongs in the library, not in `src/main.rs` or `src/gui/`.
