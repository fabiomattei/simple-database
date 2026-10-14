# sdb — schema-first database toolkit

Design your database as a small, readable text file, then let `sdb` validate it,
generate SQL for SQLite / PostgreSQL / MySQL, draw ER diagrams, compute migrations,
and push changes into a live SQLite, PostgreSQL or MySQL database. Includes an interactive SQL shell.

```sh
cargo install --path .      # installs `sdb-gui` (desktop app) and `sdb` (CLI)
sdb-gui                     # opens with a starter schema
sdb-gui schema.dbs app.db   # open a schema and a SQLite database
sdb-gui schema.dbs postgres://me:secret@localhost/app   # ...or a server
```

## Desktop app (`sdb-gui`)

| Tab | What you do there |
|---|---|
| **Schema** | Edit the `.dbs` file with syntax highlighting and line numbers. Errors and design warnings update as you type |
| **Diagram** | Interactive ER diagram. Drag tables, drag the background to pan, scroll or use − / % / + to zoom. Choose which tables are shown from the **Tables** menu or by right-clicking a table (hide it, or show only it and its related tables). Show the schema file or the live database. The schema file's diagram is editable (see below). **Export**: PNG (the canvas as shown), SVG (every visible table at 100%, light background), Mermaid or Graphviz DOT (visible tables only) |
| **SQL** | Generated DDL for SQLite, PostgreSQL or MySQL; copy or save. In SQLite it is editable and kept in sync with the schema (see below) |
| **Migrate** | Changes between the schema file and the open database, the exact SQL that will run, and **Push** (asks for confirmation). **From** picks what to compare with: the schema file, or another database (a saved connection or a SQLite file), to make the open database match it. **Pull** replaces the editor with the database's schema |
| **Query** | Run SQL (Ctrl+Enter) and see the results in a grid. Shows up to 10,000 rows per result. **Explain** shows each statement's query plan: SQLite plans only (`EXPLAIN QUERY PLAN`, nothing runs). Postgres and MySQL use `EXPLAIN ANALYZE` in a transaction that is rolled back, so you get real timings and no changes are kept. **History** lists past queries (searchable; click to load, double-click to run) and saved queries (**Save query…**). **Export** saves the result as CSV, TSV, JSON, JSON Lines, Markdown, HTML or SQL INSERTs, or copies it |
| **Data** | Browse and edit a table, 200 rows per page (see below) |
| **Seed** | Fill tables with realistic fake data, with a row count per table and a reproducible seed |

The **Data** tab edits rows in place. Click a column header to sort (again to reverse, a third
time to clear). **Filter** adds conditions per column (=, ≠, <, >, contains, starts with, in, is
NULL…) and an optional SQL `WHERE` condition; **Apply** or Enter runs them. Double-click a cell
to edit it (Enter or clicking elsewhere keeps the value, Esc cancels), right-click for **Set
NULL**, **Revert cell**, **Copy value**, **Duplicate row** and **Delete row**. **➕ Row** adds a
row whose empty cells get their defaults. Click row numbers to select rows (Ctrl+click for
several), then **Delete** or the Delete key. Nothing is written until **Save changes**: the
pending changes are listed with **Show SQL**, values are checked against the column types first,
and everything runs in one transaction. An update or delete that no longer matches exactly one
row (someone changed it meanwhile) rolls all of it back. Editing needs a primary key; tables
without one can only get new rows. **Import…** inserts a CSV, TSV, JSON or JSON Lines file
(preview and column mapping first; see `sdb import`), and **Export** saves every row that
matches the filters.

**File → Connections…** (Ctrl+K, or **Connections…** in the sidebar) manages saved
connections (see [Saved connections](#saved-connections)). The list on the left has a search
box (it matches name, host, database, user, type and group), **Favorites**, the five most
**Recent** connections, and the rest in collapsible groups. Double-click a connection to
connect. The form on the right edits the selected connection: name, type (PostgreSQL, MySQL,
SQLite), host, port, user, password (**Save password** keeps it), database or file, group,
color, favorite and [safe mode](#safe-mode). **TLS and options** holds the SSL mode and
certificate files. **Import URL** fills the form from a pasted URL. **Detected in project** lists
databases described next to the schema file or in the working directory (see `sdb conn
detect`); click one to fill the form. **Test** connects without opening the database,
**Connect** saves the form and connects (with an empty name it connects without saving), and
**Duplicate** and **Delete** do what they say. Favorites and recent connections are also
listed in the sidebar while no database is open. When a connection fails, for example because
a password is missing, the dialog opens with the error.

The app remembers the last schema file and database between sessions. For a saved connection
it stores `@name`. For a plain server URL it stores the URL without the password, so the
password must come from `PGPASSWORD` / `MYSQL_PWD` or be typed again. It asks before closing or
replacing unsaved edits. `--tab <name>` opens a specific tab. Ctrl+S / Ctrl+O / Ctrl+N save,
open and start a new schema. On Linux, file dialogs use the XDG desktop portal.

The Schema tab is a text editor first. Problems from validation are shown on the line they are
about, with the message after the text, and each entry in the problems list has a **line N**
link. **Outline** lists tables and columns (with a filter); clicking one moves the caret there,
as does clicking a table in the sidebar while the Schema tab is open. **Preview** shows the
generated SQL next to the editor, in the dialect you pick. **Format** (Ctrl+Shift+F) aligns the
columns of every table and keeps comments and blank lines.

The **Diagram** tab edits the schema file too. Double-click a table (or right-click → **Edit
table…**) to rename it and edit its columns in a grid: type, PK, not null, unique, autoincrement,
default, check, reference and its `on delete` action. Renames carry over to keys, indexes and the
foreign keys pointing at them and are written down as `was` hints (see [Renames](#renames)), and an
edit that would make the schema invalid is refused. Right-click
the background for **New table…**, or a table to add a column, delete the column under the pointer,
remove its reference or delete the table. To link two tables, drag the ○ handle at the end of a
column's row onto a column of another table (or onto a table, for its primary key). Only the changed
tables are rewritten in the schema text. **Undo** (Ctrl+Z) reverts diagram edits until you type in
the editor. Editing is off while the schema text has a syntax error and when showing the database.

The **SQL** tab edits the same schema as SQL. With the SQLite dialect selected, changing the
`CREATE TABLE` / `CREATE INDEX` statements updates the schema text, and editing the schema
updates the SQL. The SQL is checked by running it in a scratch in-memory SQLite database, so
errors are SQLite's, shown on the statement's line. Only tables you change are rewritten in the
schema text, so comments elsewhere are kept. Comments inside a rewritten table are lost. SQL
edits don't apply while the schema text has a syntax error. **Regenerate** discards SQL edits
that didn't apply. PostgreSQL and MySQL output is read-only.

All three editors (Schema, SQL and Query) have line numbers, editing commands, find/replace,
TextMate-style column selection and autocomplete:

| Keys | Effect |
|---|---|
| Enter | New line with the same indentation; after `{` one level deeper, closing the block if needed |
| Tab / Shift+Tab | Indent / outdent the selected lines |
| Ctrl+/ | Comment or uncomment the selected lines (`#` in schemas, `--` in SQL) |
| Ctrl+D | Duplicate the selected lines |
| Alt+↑ / ↓ | Move the selected lines up / down |
| Ctrl+Shift+K | Delete the selected lines |
| Ctrl+F, F3 / Shift+F3 | Find and replace; next / previous match (Enter / Shift+Enter in the find field) |
| Ctrl+click | In a schema, jump to the table or `table.column` under the pointer |

| Keys | Effect |
|---|---|
| Alt+drag | Select a rectangle, with one caret per line |
| Tap Alt | Turn a multi-line selection into a rectangle |
| Alt+Shift+↑ / ↓ | Add a caret on the line above / below |
| Typing, Backspace, Delete, ← → Home End, copy / cut / paste | Act on every line. Pasting as many lines as there are carets puts one on each |
| Esc, click, or any other key | Back to a single caret |
| Ctrl+Space | Open autocomplete (it also opens while you type a word, or after `table.`) |
| ↑ ↓, Enter / Tab, Esc | Choose, accept, or close a suggestion |

In SQL, suggestions are keywords, types, table names and column names, and `alias.` completes
the columns of `FROM users u`-style aliases. In a schema they follow the position on the line:
`table` (expands to a table skeleton) between tables; `index`, `unique index` and `pk` at the
start of a line; types after a column name; then the modifiers not used yet, `table.column`
after `->`, actions after `on delete`, and the table's own columns inside `index (…)`.

## Command line (`sdb`)

Everything the app does is also available from the terminal, for scripts and CI:

```sh
sdb init                    # writes a starter schema.dbs
```

## The schema language (`.dbs`)

```text
table users {
  id          int           pk autoincrement
  email       varchar(255)  unique not null
  role        text          not null default 'author' check (role in ('author', 'admin'))
  created_at  timestamp     not null default now
}

table posts {
  id         int   pk autoincrement
  author_id  int   not null -> users.id on delete cascade
  title      text  not null

  index (author_id)
  unique index posts_author_title (author_id, title)
}

table post_tags {
  post_id  int  not null -> posts.id
  tag      text not null
  pk (post_id, tag)
}
```

| | |
|---|---|
| Types | `int bigint smallint real decimal(p,s) bool text varchar(n) date time timestamp blob uuid json` (anything else is passed through verbatim) |
| Column modifiers | `pk` `autoincrement` `unique` `not null` `null` `default <v>` `check (<expr>)` `-> table.col` (or `references table(col)`) `on delete <a>` `on update <a>` `was <old>` (or `was (a, b)`) |
| Referential actions | `cascade` `set_null` `set_default` `restrict` `no_action` |
| Defaults | `'text'` `42` `true` `null` `now` `fn()` `(any sql expression)` |
| Table level | `index [name] (a, b)` `unique index [name] (a, b)` `pk (a, b)` |
| Renamed table | `table people was users { ... }` |
| Comments | `# ...` or `// ...` |

## Commands

Any `SCHEMA` argument may be a `.dbs` file, **a SQLite database file or a database URL**: databases are introspected automatically.

`--db` takes a SQLite file, a server URL or a saved connection:

```sh
--db app.db                                   # SQLite (push creates it if missing)
--db postgres://user:password@localhost:5432/app
--db mysql://user:password@localhost:3306/app # also mariadb://
--db @prod                                    # saved connection (see below)
```

Without a password in the URL, `PGPASSWORD` or `MYSQL_PWD` is used. PostgreSQL works on the
current schema (normally `public`). MySQL needs 8.0.16 or newer for check constraints.

TLS uses the same libpq-style URL parameters for both servers:

| Parameter | Meaning |
|---|---|
| `sslmode=disable` | Plain TCP |
| `sslmode=prefer` | TLS if the server offers it, certificate not verified (the default) |
| `sslmode=require` | TLS required, certificate not verified |
| `sslmode=verify-ca` | TLS, server certificate signed by a trusted CA |
| `sslmode=verify-full` | As `verify-ca`, and the certificate must match the host name |
| `sslrootcert=ca.pem` | CA to verify the server with (default: the Mozilla root store) |
| `sslcert=client.pem`, `sslkey=client.key` | Client certificate authentication (PEM; both are needed) |

Giving certificate files without `sslmode` makes TLS required, and `require` with `sslrootcert`
verifies the CA, as libpq does:

```sh
--db "postgres://app@db.example.com/app?sslmode=verify-full&sslrootcert=ca.pem&sslcert=client.pem&sslkey=client.key"
--db "mysql://app@db.example.com/app?sslmode=verify-ca&sslrootcert=ca.pem"
```

| Command | What it does |
|---|---|
| `sdb init [path]` | Create a starter schema |
| `sdb check SCHEMA` | Validate, and lint the design: missing primary keys, unindexed foreign keys, FK type mismatches, `SET NULL` on `NOT NULL` columns, etc. |
| `sdb fmt SCHEMA [-w]` | Reformat with aligned columns |
| `sdb sql SCHEMA -d sqlite\|postgres\|mysql` | Generate DDL (tables in dependency order) |
| `sdb diagram SCHEMA -f mermaid\|dot` | ER diagram: paste Mermaid into GitHub/GitLab markdown, or `\| dot -Tsvg > er.svg` |
| `sdb diff FROM TO -d DIALECT [-o file]` | Summary of changes + migration SQL |
| `sdb push SCHEMA --db app.db [--dry-run] [-y]` | Migrate a database to match the schema |
| `sdb pull --db app.db [-o schema.dbs]` | Reverse-engineer a schema from an existing database. What a schema can't describe (views, triggers, functions, enum types, expression and partial indexes, multi-column foreign keys) is listed in comments at the top |
| `sdb seed --db app.db [-n 10] [-c posts=200] [--only a,b] [--seed 42] [-y]` | Fill tables with realistic fake data (see below) |
| `sdb query --db app.db "SQL" [-f table\|csv\|json\|markdown] [-y]` | Run SQL, print results. `-y` skips safe mode's question |
| `sdb exec FILE --db app.db [-1] [--continue] [-v] [-f FORMAT] [-y]` | Run a SQL file statement by statement (see below) |
| `sdb import --db app.db FILE [-t TABLE] [-f csv\|tsv\|json\|jsonl] [-y]` | Insert rows from a file (see below) |
| `sdb dump --db app.db [-o dump.sql] [-t a,b] [--schema-only\|--data-only] [-d DIALECT]` | Write the schema and rows as a SQL script (see below) |
| `sdb restore FILE --db app.db [--clean] [-y]` | Run a dump against a database in one transaction (see below) |
| `sdb shell [--db app.db]` | Interactive SQL shell (in-memory if no `--db`) |
| `sdb conn list` | List saved connections by group |
| `sdb conn add NAME URL\|FILE [-g GROUP] [--no-password] [-f] [--safe-mode MODE]` | Save a connection. A password in the URL is saved unless `--no-password`; `-f` replaces an existing one |
| `sdb conn detect [DIR] [--save]` | List databases a project describes; `--save` saves the new ones (without passwords) |
| `sdb conn remove NAME` | Delete a saved connection |
| `sdb conn test NAME` | Connect and report how many tables there are |

### Saved connections

Connections saved in the app or with `sdb conn add` are shared by both, and `@name` works
wherever a database is expected (`--db`, `SCHEMA` arguments, `sdb-gui @prod`):

```sh
sdb conn add prod "postgres://app:secret@db.example.com/app?sslmode=verify-full" -g Work
sdb conn add dev dev.db                   # SQLite paths are stored as absolute paths
sdb push schema.dbs --db @prod --dry-run
sdb diff @prod schema.dbs -d postgres
```

They are stored in `connections.json` in `$SDB_CONFIG_DIR`, else `$XDG_CONFIG_HOME/sdb`, else
`~/.config/sdb` (`%APPDATA%\sdb` on Windows). Saved passwords are kept apart in `secrets.json`,
which only you can read (mode 0600, like `~/.pgpass`; it is not encrypted). That way
`connections.json` can be copied or shared without them. A connection without a saved
password uses `PGPASSWORD` / `MYSQL_PWD`, or the app asks for it. The app's query history
(`history.json`, last 500 queries, also 0600) and saved queries (`queries.json`) live in the
same folder.

`sdb conn detect` finds databases a project already describes: `DATABASE_URL`-style values and
Laravel `DB_*` settings in `.env`, `.env.local`, `.env.development` and `.env.example`;
PostgreSQL, MySQL and MariaDB services in `compose.yaml` / `docker-compose.yml` (user, password
and database from their environment, `${VAR}` filled from `.env`, the published port); and the
`datasource` of `prisma/schema.prisma`. SQLite paths become absolute.

### Safe mode

Each saved connection has a safe mode, set in the app or with `sdb conn add --safe-mode`:

| Mode | Effect |
|---|---|
| `off` | Run everything (the default) |
| `warn` | Ask before DROP, TRUNCATE, ALTER … DROP, and DELETE or UPDATE without WHERE |
| `confirm` | Ask before every statement that writes, including saving edits, importing and seeding |
| `read-only` | Refuse every write. The session is also made read-only on the server (`SET SESSION … READ ONLY`, SQLite `PRAGMA query_only`), so nothing gets past the check |

The app shows the mode under the database name and asks in a dialog that lists the statements.
The CLI asks on the terminal; `-y` answers yes. `push` always asks unless `-y`.

To compare two live databases, pass both: `sdb diff @prod @staging -d postgres`, or make one
match the other with `sdb push @prod --db @staging` (in the app: Migrate → **From**).

### Importing data

`sdb import --db app.db users.csv` inserts the file into the table named like the file (or
`-t TABLE`). The format comes from the extension: `.csv`, `.tsv`, `.json` (an array of
objects) or `.jsonl`. Columns are matched by name, ignoring case; others are skipped. In CSV and
TSV an empty field is NULL and `""` is an empty string. NULL in a NOT NULL column with a default
gets the default, and a column that is empty in every row and filled by the database
(auto-increment or default) is left out. Values are checked against the column types, and all
rows go in one transaction, so a bad row means nothing is inserted.

### Running SQL files

`sdb exec script.sql --db app.db` runs a file one statement at a time and prints the results of
queries (`-f` picks the format, `-` reads the script from stdin). It stops at the first failing
statement and names its line; the statements before it stay applied. `-1` (`--transaction`)
runs the whole file in one transaction that a failure rolls back (the file must then not use
BEGIN / COMMIT itself; MySQL still commits CREATE, ALTER and DROP immediately). `--continue`
runs every statement and reports each failure instead. `-v` prints each statement's line and
the rows it changed. Statements may contain `;` inside strings, comments, Postgres `$$` bodies
and `CREATE TRIGGER … BEGIN … END`. Safe mode asks before the writes, as for `sdb query`. In the
app: **File → Run SQL file…** (same options; **Show results** opens the queries' results in the
Query tab), or **Open…** in the Query tab to load a file into the editor.

### Dump and restore

`sdb dump --db app.db -o app.sql` writes the tables (CREATE TABLE and indexes, in dependency
order) and their rows (batched INSERTs, sorted by primary key) as plain SQL in the database's
dialect. `-t` limits it to some tables; `--schema-only` and `--data-only` leave out one half.
`-d postgres` writes SQL for another database, so `sdb dump --db app.db -d postgres` moves a
SQLite database to Postgres. For Postgres the dump ends with `setval` calls, so identity
columns continue after the restored ids. The file has no BEGIN/COMMIT: `sqlite3`, `psql` and
`mysql` can run it as is.

`sdb restore app.sql --db copy.db` runs a dump (sdb's or plain SQL, such as `sqlite3 app.db
.dump`) in one transaction: if a statement fails, nothing is changed. A SQLite target file is
created if it is missing. Tables the dump creates must not exist yet; `--clean` drops them
first (and always asks, unless `-y`). Foreign keys are checked at the end (SQLite) or switched
off during the restore (MySQL), so row order doesn't matter. MySQL commits each CREATE and DROP
on its own, so a failure there leaves the statements before it applied. Check expressions are
dumped as the server spells them, so a check rewritten by Postgres (`= ANY (ARRAY[...])`) may
not load into SQLite or MySQL. In the app: **File → Dump database…** and **Restore dump…**.

### Typical workflow

```sh
sdb init schema.dbs
sdb check schema.dbs
sdb push schema.dbs --db dev.db          # creates the tables
sdb seed --db dev.db -n 50               # 50 fake rows per table
# ...edit schema.dbs...
sdb push schema.dbs --db dev.db --dry-run  # review the migration
sdb push schema.dbs --db dev.db            # apply it (asks for confirmation)
sdb diff prod_snapshot.dbs schema.dbs -d postgres -o migrations/002.sql
```

### Shell meta commands

`.tables` `.schema [t]` `.sql [t]` `.ddl <dialect>` `.diagram [mermaid|dot]` `.check`
`.seed [rows] [tables...]` `.mode table|csv|json|markdown` `.timer on|off` `.read file.sql` `.help` `.quit`

Statements end with `;` and may span lines. History is saved to `~/.sdb_history`.

## Seeding fake data

`sdb seed` reads the live database schema and inserts rows that satisfy it:

* **Foreign keys**: parent tables are filled first, and child rows point at existing parent rows.
  Self-references (`manager_id -> employees.id`) point at rows inserted earlier.
* **Constraints**: `check (x in ('a', 'b'))` picks from the list, and `>=`, `<=` and `between`
  bound numbers. Rows that break any other unique or check constraint are regenerated. After
  50 rejections in a row the table stops and the reason is reported. That happens, for
  example, when a join table asks for more distinct pairs than exist.
* **Realistic values**: chosen from the column type, `varchar(n)` length and column name.
  For example `email`, `first_name`, `name` (a person's name in tables like `users` and
  `customers`), `title`, `body`, `price`, `age`, `rating`, `url`, `phone`, `city`, `slug`,
  `status` and `created_at`. Name, email and username on one row belong to the same person.
* **Reproducible**: every run prints its seed. Pass `--seed N` to generate the same data again.
* Runs in one transaction. Nullable columns are occasionally left `NULL`.

## How migrations work

* **PostgreSQL**: `ALTER TABLE ... ALTER COLUMN` for type/null/default changes; named
  constraints (`fk_<table>_<col>`) so foreign keys can be dropped and re-added. Wrapped in a
  transaction and rolled back if any statement fails.
* **Primary keys and identities** change in place. Postgres drops `<table>_pkey` and adds the
  new key; a column that becomes `autoincrement` gets `GENERATED BY DEFAULT AS IDENTITY`,
  continuing after the largest id already there. MySQL makes the whole key change (with the
  `AUTO_INCREMENT` columns) in one `ALTER TABLE`, since an `AUTO_INCREMENT` column must always
  be a key. A new `autoincrement` key column numbers the existing rows. Servers refuse to
  change a primary key that foreign keys point at; `sdb` warns about it.
* **MySQL**: `MODIFY COLUMN`, `DROP FOREIGN KEY`, etc. Check constraints are named
  `<table>_<col>_check`. MySQL DDL is not transactional: if a statement fails, the ones before
  it stay applied, and `sdb` says how many ran. `text` columns that are unique, indexed or part
  of a key become `varchar(255)`, because MySQL can't index `TEXT`.
* Servers rewrite check expressions (Postgres turns `x in ('a', 'b')` into
  `x = ANY (ARRAY['a', 'b'])`). Before diffing, `sdb` asks the server to rewrite your checks the
  same way, in a scratch temporary table, so an unchanged check never shows up as a change.
  `pull` shows the server's spelling.
* **SQLite**: simple column additions/drops use `ALTER TABLE`. Anything else (type changes,
  constraints) uses SQLite's documented table-rebuild procedure: create new table → copy
  data → drop old → rename, with foreign keys disabled. `sdb push` checks
  `PRAGMA foreign_key_check` before committing and rolls back on any violation.

Destructive changes (dropped tables/columns) are flagged as warnings.

### Renames

A renamed table or column would look like a drop plus an add, which loses its data. Say what it
was called before, and the migration renames it instead:

```text
table people was users {
  id     int           pk autoincrement
  email  varchar(255)  unique not null was mail
}
```

A hint applies when the database has the old name and not the new one, so it does nothing once
the rename is done: keep it, or delete it after pushing everywhere. `was (a, b)` lists several
earlier names (oldest first); the latest one the database has wins. The Diagram's table editor
writes these hints for you. Constraints named after the table or column (`fk_<table>_<col>`,
`<table>_<col>_check`, Postgres `_key` / `_pkey`) are renamed too; MySQL can't rename
constraints, so it drops and re-adds them. Indexes that only changed name (such as
`posts_author_id_idx` → `posts_writer_id_idx`) are renamed, except on SQLite, where they are
recreated.

## Development

```sh
cargo test
cargo clippy --all-targets
```

Source layout: `src/lib.rs` is the shared core and `src/main.rs` the CLI. `src/gui/` is the
desktop app: `main.rs` (app state, window), `tabs/` (one file per tab), `panels.rs` (menu bar, sidebar, status bar), `dialogs.rs`, `file_menu.rs`, `er.rs` (diagram canvas), `grid.rs` (result grid),
`editor.rs` (code editor: line numbers, commands, find/replace, column selection, autocomplete), `highlight.rs` (syntax colors). Core modules: `parser.rs` (DSL) → `schema.rs` (model) → `validate.rs`, `ddl.rs`,
`diff.rs`, `diagram.rs`, `format.rs`; `seed.rs` (fake data); `db.rs` (live connections), which
dispatches to `sqlite.rs`, `pg.rs` and `mysqldb.rs` (introspection & queries); `shell.rs`, `output.rs`.

Unit tests live in each module's `mod tests`; `tests/cli.rs` runs the `sdb` binary end to end
(init → push → seed → dump → restore → pull, migrations, error exits) in a temporary directory.
The PostgreSQL and MySQL tests (round trips in `db.rs`, the CLI pipeline in `tests/cli.rs`) run
only when they get a scratch database (all of its tables are dropped):

```sh
SDB_TEST_POSTGRES=postgres://sdb:sdb@localhost:5432/sdb SDB_TEST_MYSQL=mysql://sdb:sdb@127.0.0.1:3306/sdb cargo test
```

CI (`.github/workflows/ci.yml`) runs `cargo clippy --all-targets -D warnings` and `cargo test`
with PostgreSQL 17 and MySQL 8.4 service containers, so the server tests always run there.
