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
| **Diagram** | Interactive ER diagram. Drag tables, drag the background to pan, scroll or use − / % / + to zoom. Choose which tables are shown from the **Tables** menu or by right-clicking a table (hide it, or show only it and its related tables). Show the schema file or the live database. **Export**: PNG (the canvas as shown), SVG (every visible table at 100%, light background), Mermaid or Graphviz DOT (visible tables only) |
| **SQL** | Generated DDL for SQLite, PostgreSQL or MySQL; copy or save. In SQLite it is editable and kept in sync with the schema (see below) |
| **Migrate** | Changes between the schema file and the open database, the exact SQL that will run, and **Push** (asks for confirmation). **Pull** replaces the editor with the database's schema |
| **Query** | Run SQL (Ctrl+Enter) and see the results in a grid. Shows up to 10,000 rows per result. **Explain** shows each statement's query plan: SQLite plans only (`EXPLAIN QUERY PLAN`, nothing runs). Postgres and MySQL use `EXPLAIN ANALYZE` in a transaction that is rolled back, so you get real timings and no changes are kept |
| **Data** | Browse any table, 200 rows per page |
| **Seed** | Fill tables with realistic fake data, with a row count per table and a reproducible seed |

**File → Connections…** (Ctrl+K, or **Connections…** in the sidebar) manages saved
connections (see [Saved connections](#saved-connections)). The list on the left has a search
box (it matches name, host, database, user, type and group), **Favorites**, the five most
**Recent** connections, and the rest in collapsible groups. Double-click a connection to
connect. The form on the right edits the selected connection: name, type (PostgreSQL, MySQL,
SQLite), host, port, user, password (**Save password** keeps it), database or file, group,
color and favorite. **TLS and options** holds the SSL mode and certificate files. **Import
URL** fills the form from a pasted URL. **Test** connects without opening the database,
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
| Column modifiers | `pk` `autoincrement` `unique` `not null` `null` `default <v>` `check (<expr>)` `-> table.col` (or `references table(col)`) `on delete <a>` `on update <a>` |
| Referential actions | `cascade` `set_null` `set_default` `restrict` `no_action` |
| Defaults | `'text'` `42` `true` `null` `now` `fn()` `(any sql expression)` |
| Table level | `index [name] (a, b)` `unique index [name] (a, b)` `pk (a, b)` |
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
| `sdb pull --db app.db [-o schema.dbs]` | Reverse-engineer a schema from an existing database |
| `sdb seed --db app.db [-n 10] [-c posts=200] [--only a,b] [--seed 42]` | Fill tables with realistic fake data (see below) |
| `sdb query --db app.db "SQL" [-f table\|csv\|json\|markdown]` | Run SQL, print results |
| `sdb shell [--db app.db]` | Interactive SQL shell (in-memory if no `--db`) |
| `sdb conn list` | List saved connections by group |
| `sdb conn add NAME URL\|FILE [-g GROUP] [--no-password] [-f]` | Save a connection. A password in the URL is saved unless `--no-password`; `-f` replaces an existing one |
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
password uses `PGPASSWORD` / `MYSQL_PWD`, or the app asks for it.

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

Destructive changes (dropped tables/columns) are flagged as warnings. Renames are seen as
drop + add. Edit the generated SQL if you need to preserve the data.

## Development

```sh
cargo test
cargo clippy --all-targets
```

Source layout: `src/lib.rs` is the shared core and `src/main.rs` the CLI. `src/gui/` is the
desktop app: `main.rs` (window, tabs), `er.rs` (diagram canvas), `grid.rs` (result grid),
`editor.rs` (code editor: line numbers, commands, find/replace, column selection, autocomplete), `highlight.rs` (syntax colors). Core modules: `parser.rs` (DSL) → `schema.rs` (model) → `validate.rs`, `ddl.rs`,
`diff.rs`, `diagram.rs`, `format.rs`; `seed.rs` (fake data); `db.rs` (live connections), which
dispatches to `sqlite.rs`, `pg.rs` and `mysqldb.rs` (introspection & queries); `shell.rs`, `output.rs`.

The PostgreSQL and MySQL round-trip tests run only when they get a scratch database (all of its
tables are dropped):

```sh
SDB_TEST_POSTGRES=postgres://sdb:sdb@localhost:5432/sdb SDB_TEST_MYSQL=mysql://sdb:sdb@127.0.0.1:3306/sdb cargo test
```
