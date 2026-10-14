# sdb — schema-first database toolkit

Design your database as a small, readable text file, then let `sdb` validate it,
generate SQL for SQLite / PostgreSQL / MySQL, draw ER diagrams, compute migrations,
and push changes into a live SQLite, PostgreSQL or MySQL database. Includes an interactive SQL shell.

```sh
cargo install --path .      # installs `sdb-gui` (desktop app) and `sdb` (CLI)
sdb-gui                     # asks which database to use (saved or new connection)
sdb-gui schema.dbs app.db   # open a schema and a SQLite database
sdb-gui schema.dbs postgres://me:secret@localhost/app   # ...or a server
```

## Desktop app (`sdb-gui`)

| Tab | What you do there |
|---|---|
| **Schema** | Edit the `.dbs` file with syntax highlighting and line numbers. Errors and design warnings update as you type |
| **Diagram** | Interactive ER diagram. Drag tables, drag the background to pan, scroll or use − / % / + to zoom. **Auto layout** keeps related tables together (parents left of the tables that reference them, tall layers wrapped into several columns, unrelated tables in a grid at the end) without overlaps; **Tidy** only moves overlapping tables apart, and a table that grows pushes its neighbors aside. **Browse data** (or right-click a table → **Browse data**, or double-click it in the read-only database diagram) opens the selected table in the Data tab. Choose which tables are shown from the **Tables** menu or by right-clicking a table (hide it, or show only it and its related tables). Show the schema file or the live database. The schema file's diagram is editable (see below). **Export**: PNG (the canvas as shown), SVG (every visible table at 100%, light background), Mermaid or Graphviz DOT (visible tables only) |
| **SQL** | Generated DDL for SQLite, PostgreSQL or MySQL; copy or save. In SQLite it is editable and kept in sync with the schema (see below) |
| **Migrate** | Changes between the schema file and the open database, the exact SQL that will run, and **Push** (asks for confirmation). **From** picks what to compare with: the schema file, or another database (a saved connection or a SQLite file), to make the open database match it. **Show as** switches the right-hand side between the SQL and the same changes as a **Laravel** or **Rails** migration, to **Copy** or **Save as…** (into the project's `database/migrations` or `db/migrate` next to the schema file, when there is one). **Pull** replaces the editor with the database's schema |
| **Query** | Run SQL and see the results in a grid. **Run** (Ctrl+Enter) runs the selection, or else the statement at the caret (the one it is in, or the last one before it); **Run all** (Ctrl+Shift+Enter) runs the whole editor. Shows up to 10,000 rows per result. **Explain** shows the query plan of what Run would run: SQLite plans only (`EXPLAIN QUERY PLAN`, nothing runs). Postgres and MySQL use `EXPLAIN ANALYZE` in a transaction that is rolled back, so you get real timings and no changes are kept. **History** lists past queries (searchable; click to load, double-click to run all of it) and saved queries (**Save query…**). **Export** saves the result as CSV, TSV, JSON, JSON Lines, Markdown, HTML or SQL INSERTs, or copies it |
| **Data** | Browse and edit tables, each in its own tab, 200 rows per page (see below) |
| **Seed** | Fill tables with realistic fake data, with a row count per table and a reproducible seed |

The window follows the light or dark system theme (View menu to override). The toolbar shows what
is open (table, query or schema file), the tab switcher, and the open database: click it for
connections; the lock shows the [safe mode](#safe-mode) and ⟳ reads the database again. The
sidebar (hide it with the button next to the menus) has a **Filter** box and two views:
**Tables** and **Connections** (saved connections, favorites first, plus **Open file…**,
**New…** and **Manage…**).

The **Tables** view lists the tables of the schema file and of the open database in one list, with
row counts. Once both are there, each table is marked by how the file differs from the
database: **+** new, **~** changed, **→** renamed, **−** only in the database (Push drops it).
The **Migrate** tab shows how many tables differ. Clicking a table moves the caret to it in the
Schema tab, selects it in the Diagram, and otherwise opens its rows in the Data tab; right-click
for the others (**Open data**, **Show in schema**, **Show in diagram**, **Show the migration**,
**Copy name**).

The **Data** tab edits rows in place. Each table opens in its own tab (click it in the sidebar,
use **+** next to the tabs, or **Browse data** in the Diagram), and every tab keeps its page,
filters and unsaved changes while you look at another; a tab with changes waiting is marked •
and can't be closed until they're saved or discarded. The open tabs are remembered per database and come
back the next time you open it. Click a column header to sort (again to reverse, a third
time to clear). Columns start as wide as their content (up to a limit; drag an edge for more), and
right-clicking a header offers **Fit columns to content**, as in Query results. **Filter** adds conditions per column (=, ≠, <, >, contains, starts with, in, is
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
matches the filters. The bar under the grid shows the rows on the page and moves between pages.

**File → Connections…** (Ctrl+K, the database button in the toolbar, or **Manage…** in the sidebar's Connections view) manages saved
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
**Duplicate** and **Delete** do what they say. Saved connections are also
listed in the sidebar's **Connections** view; click one to connect. When a connection fails, for example because
a password is missing, the dialog opens with the error.

At startup, unless a database is given on the command line, a welcome window asks which database to
use. On the right, saved connections are large rows (favorites first, with a search box); one click
connects, and the last session's database is listed when it isn't a saved one. On the left are
**New connection…**, **Open SQLite file…**, **New SQLite database…** and **Continue without a
database**. A new schema starts empty; **Help → Example
schema** shows a sample blog schema and can open it in the editor.

Each window works with one database, and its title names it. With a database already open,
opening or connecting to another one (File menu, Connections) opens it in a new window, which
starts on the Data tab with an empty schema. **File → New window** (Ctrl+Shift+N) opens an
empty window with the startup dialog. To switch the current window to another database, use
**File → Close database** first. Query history, saved queries and the open Data tabs are
shared by all windows.

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
table…**) to rename it, set its comment and edit its columns in a grid: type, PK, not null, unique,
autoincrement, default, check, comment, reference and its `on delete` action. Hovering a table or
column shows its comment. Renames carry over to keys, indexes and the
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
schema text, so comments elsewhere are kept. Inside a rewritten table, a `-- comment` at the end of
a column's line (or of the `CREATE TABLE … (` line) becomes its `comment`; others are lost. SQL
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
table users comment 'People who can sign in' {
  id          int                         pk autoincrement
  email       varchar(255)                unique not null
  role        enum('author', 'admin')     not null default 'author'
  age         int                         check (age >= 18)
  created_at  timestamp                   not null default now comment 'Set on insert'
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

table tag_votes {
  post_id  int  not null
  tag      text not null
  user_id  int  not null -> users.id
  pk (post_id, tag, user_id)
  fk (post_id, tag) -> post_tags (post_id, tag) on delete cascade
  index (user_id)
}
```

| | |
|---|---|
| Types | `int bigint smallint real decimal(p,s) bool text varchar(n) date time timestamp blob uuid json enum('a', 'b')` (anything else is passed through verbatim; quote a type of several words: `"int unsigned"`) |
| Column modifiers | `pk` `autoincrement` `unique` `not null` `null` `default <v>` `check (<expr>)` `-> table.col` (or `references table(col)`) `on delete <a>` `on update <a>` `was <old>` (or `was (a, b)`) `comment 'text'` |
| Referential actions | `cascade` `set_null` `set_default` `restrict` `no_action` |
| Defaults | `'text'` `42` `true` `null` `now` `fn()` `(any sql expression)` |
| Table level | `index [name] (a, b)` `unique index [name] (a, b)` `pk (a, b)` `fk (a, b) -> table (x, y) [on delete <a>] [on update <a>]` (or `foreign key (a, b) references table (x, y)`) |
| Table header | `table people was users comment 'text' { ... }` (`was` and `comment` are optional); `table billing.invoices` is in Postgres schema `billing` |
| Comments | `# ...` or `// ...` (kept in the file only; `comment '...'` goes to the database) |

* **Enums** are a native type on MySQL (`ENUM(...)`) and Postgres (a type named
  `<table>_<column>_enum`, created, changed and dropped with the column). SQLite has no enums: the
  column is `TEXT` with a check named `enum` (`CONSTRAINT enum CHECK (role IN (...))`), which is how
  `pull` recognizes it. Changing the values migrates the column; rows must hold one of the new values.
* **Multi-column foreign keys** point at a composite primary key or unique index, such as a join
  table's. A one-column `fk (a) -> t (x)` is the same as `a ... -> t.x`. Their constraints are named
  `fk_<table>_<a>_<b>`.
* **Comments** become Postgres `COMMENT ON`, MySQL `COMMENT`, and on SQLite a `-- comment` at the end
  of the column's line in `CREATE TABLE` (one line, since SQLite can only keep them in that text).
  `pull` reads them all back.
* **Postgres schemas**: `table billing.invoices { ... }` is table `invoices` in schema `billing`;
  refer to it as `-> billing.invoices.id`. Tables in the connection's default schema have no
  prefix. Constraint and index names leave the schema out (`fk_invoices_user_id`); the enum type is
  `billing.invoices_status_enum`. `push` creates a missing schema (`CREATE SCHEMA IF NOT EXISTS`),
  and a rename (`table archive.invoices was billing.invoices`) moves the table with `SET SCHEMA`.
  On SQLite and MySQL the dot is just part of the table's name.

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
current schema (normally `public`); add `schema=name` to the URL for another one
(`postgres://app@localhost/app?schema=billing`, or the connection's **Options** field). List
several to pull, diff and push them together: with `schema=public,billing` the tables of `public`
keep plain names and those of `billing` are qualified (`billing.invoices`; see
[Postgres schemas](#the-schema-language-dbs)). The first schema listed is the default: unqualified
names in queries resolve through all of them, in order, and new unqualified tables go there. Every
schema listed must exist. `pull` lists the other schemas that hold tables, and `push` warns about a
schema-qualified table in a schema the connection doesn't read. MySQL needs 8.0.16 or newer for check constraints.

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
verifies the CA, as libpq does. Connecting to a remote host (anything but `localhost`, a
loopback address or a socket) with `disable`, `prefer` or `require` prints a warning, because
nothing checks which server answered; the app shows "⚠ server not verified" in the status bar.
A connection saved to a remote host without an `sslmode` gets `verify-full`. If the server's
certificate comes from a private CA, pass it with `sslrootcert`:

```sh
--db "postgres://app@db.example.com/app?sslmode=verify-full&sslrootcert=ca.pem&sslcert=client.pem&sslkey=client.key"
--db "mysql://app@db.example.com/app?sslmode=verify-ca&sslrootcert=ca.pem"
```

| Command | What it does |
|---|---|
| `sdb init [path]` | Create a starter schema |
| `sdb check SCHEMA` | Validate, and lint the design: missing primary keys, unindexed foreign keys, FK type mismatches, `SET NULL` on `NOT NULL` columns, etc. |
| `sdb check [SCHEMA] --db app.db [--sample 5]` | Find rows whose foreign key points at a row that doesn't exist (see below) |
| `sdb fmt SCHEMA [-w]` | Reformat with aligned columns |
| `sdb sql SCHEMA -d sqlite\|postgres\|mysql` | Generate DDL (tables in dependency order; on Postgres/MySQL a foreign key cycle is closed with `ALTER TABLE` at the end) |
| `sdb diagram SCHEMA -f mermaid\|dot` | ER diagram: paste Mermaid into GitHub/GitLab markdown, or `\| dot -Tsvg > er.svg` |
| `sdb diff FROM TO -d DIALECT [-o file]` | Summary of changes + migration SQL |
| `sdb diff FROM TO -d DIALECT -f laravel\|rails [--name NAME] [--rails-version 8.0] [-o dir]` | The same changes as a Laravel or Rails migration (see [Framework migrations](#framework-migrations)) |
| `sdb push SCHEMA --db app.db [--dry-run] [-y] [--force]` | Migrate a database to match the schema. Refuses when rows already there would make it fail (see below) unless `--force` |
| `sdb pull --db app.db [-o schema.dbs]` | Reverse-engineer a schema from an existing database. What a schema can't describe (views, triggers, functions, enum types no column uses, expression and partial indexes, table-level checks) is listed in comments at the top |
| `sdb seed --db app.db [-n 10] [-c posts=200] [--only a,b] [--seed 42] [-y]` | Fill tables with realistic fake data (see below) |
| `sdb query --db app.db "SQL" [-f table\|csv\|json\|markdown] [-y]` | Run SQL, print results. `-y` skips safe mode's question |
| `sdb exec FILE --db app.db [-1] [--continue] [-v] [-f FORMAT] [-y]` | Run a SQL file statement by statement (see below) |
| `sdb import --db app.db FILE [-t TABLE] [-f csv\|tsv\|json\|jsonl] [-y]` | Insert rows from a file (see below) |
| `sdb dump --db app.db [-o dump.sql] [-t a,b] [--schema-only\|--data-only] [-d DIALECT]` | Write the schema and rows as a SQL script (see below) |
| `sdb restore FILE --db app.db [--clean] [-y]` | Run a dump against a database in one transaction (see below) |
| `sdb shell [--db app.db]` | Interactive SQL shell (in-memory if no `--db`) |
| `sdb conn list` | List saved connections by group |
| `sdb conn add NAME URL\|FILE [-g GROUP] [--no-password] [--keychain] [-f] [--safe-mode MODE]` | Save a connection. A password in the URL is saved unless `--no-password`, in the OS keychain with `--keychain`; `-f` replaces an existing one. Remote hosts default to `sslmode=verify-full` |
| `sdb conn detect [DIR] [--save]` | List databases a project describes; `--save` saves the new ones (without passwords) |
| `sdb conn remove NAME` | Delete a saved connection |
| `sdb conn test NAME` | Connect and report how many tables there are |

Output is colored only on a terminal: piped or redirected output is plain text. `NO_COLOR=1`
turns colors off everywhere, `CLICOLOR_FORCE=1` keeps them when piped.

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
which only you can read (mode 0600, like `~/.pgpass`; it is not encrypted), or, with
`--keychain` (the app's "In the OS keychain"), in the macOS Keychain, the Windows Credential
Manager or the Secret Service (GNOME Keyring, KWallet) under the service name `sdb`. That way
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

A statement counts as a write by what it calls too: `SELECT nextval(...)`, `setval`, `lo_import`,
`load_extension` and other built-in functions that write are writes, and `pg_terminate_backend`,
`pg_cancel_backend` or `lo_unlink` are dangerous, also when the name is quoted. So are calls to the
database's own functions that may write: on PostgreSQL every `VOLATILE` function (the default) outside
the system schemas and extensions, on MySQL every stored function not declared `NO SQL` or
`READS SQL DATA`. Declare a function `STABLE` / `READS SQL DATA` and `confirm` stops asking about it.
sdb reads the list on connect, when it reads the schema and after SQL that mentions a function.
`read-only` mode doesn't refuse these calls: the server's read-only session stops whatever they write.

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
rows go in one transaction, so a bad row means nothing is inserted. The file is read as it goes, never
loaded whole, so its size doesn't matter (it is read twice: once to count and preview the rows,
once to insert them).

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

### Checking foreign keys

`sdb check --db app.db` looks for orphaned rows: rows whose foreign key points at a parent row
that doesn't exist. They get in when the database doesn't enforce the key (SQLite connections
that never turned on `PRAGMA foreign_keys`, MySQL MyISAM tables), when checks were off during a
load, or when the key was never declared. Each foreign key is checked with one query; a row
with a NULL in any key column references nothing and is fine. The output lists the broken keys
with their row counts and the first rows (`--sample N`, 0 for none), and the command exits with
1 if there is anything, so it works as a CI step. With a schema file (`sdb check schema.dbs
--db app.db`) the schema is validated first and its foreign keys are the ones checked, including
keys that exist only in the `.dbs`. In the app: **File → Check foreign keys…** lists the broken
keys; **Show rows** opens the orphaned rows in the Data tab, filtered with a WHERE condition you
can edit or clear.

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

Statements end with `;` and may span lines. History is saved to `shell_history` in the same folder as [saved connections](#saved-connections), readable only by you.

## Seeding fake data

`sdb seed` reads the live database schema and inserts rows that satisfy it:

* **Foreign keys**: parent tables are filled first, and child rows point at existing parent rows.
  Self-references (`manager_id -> employees.id`) point at rows inserted earlier. A multi-column
  key takes all its values from one parent row.
* **Constraints**: `enum(...)` and `check (x in ('a', 'b'))` pick from the list, and `>=`, `<=` and `between`
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
  transaction and rolled back if any statement fails. An enum whose values change gets a new
  type, which the column is converted to through `text`; the old type is dropped.
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
  constraints, comments) uses SQLite's documented table-rebuild procedure: create new table → copy
  data → drop old → rename, with foreign keys disabled. `sdb push` checks
  `PRAGMA foreign_key_check` before committing and rolls back on any violation.

Destructive changes (dropped tables/columns) are flagged as warnings, and so are changes that
existing values can make fail or lose data: a column becoming NOT NULL, a narrower type
(`text` → `varchar(10)`, `bigint` → `int`, fewer decimals, enum values removed) or another
type the values must convert to. Before applying, `push` (and the app's **Push**) counts the rows
that would make the migration fail, and names them, e.g. `2 row(s) of users repeat a value of
email, which becomes unique`. It checks NULLs for NOT NULL, duplicates for new unique keys and
primary keys, rows failing a new check, foreign keys pointing at no row, enum values; on
PostgreSQL and MySQL also lengths and ranges, and on PostgreSQL 16+ whether text converts to
the new type. `push` stops when any are found; fix the rows, or pass `--force` to try anyway.

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

### Framework migrations

`sdb diff` (and the app's Migrate tab) can write a migration as a **Laravel** or **Rails**
migration file instead of SQL, with `down` / the `down()` method worked out from the reverse
diff (renames included):

```sh
sdb diff prod_snapshot.dbs schema.dbs -d postgres -f laravel -o database/migrations
sdb diff prod_snapshot.dbs schema.dbs -d mysql -f rails --name add_comments -o db/migrate
```

Given a directory, `-o` writes the framework's file name (`2026_10_14_120000_add_comments.php`,
`20261014120000_add_comments.rb`); without `-o` the migration is printed. `--name` defaults to
something like `create_comments_table` or `update_schema`. The dialect still matters: the
generated code is for that database.

Changes become the framework's schema builder calls (`Schema::create`, `$table->...->change()`,
`create_table`, `add_column`, `change_column`, `add_foreign_key`, …) using the constraint names
`sdb` uses (`fk_<table>_<col>`, `<table>_<col>_check`, index names), so later diffs and pushes
still find them. What the framework can't express is written as `sdb`'s own SQL for that table
(`DB::unprepared(...)` / `execute`), and listed as a warning:

| | Laravel | Rails |
|---|---|---|
| Check constraints | SQL (added after `Schema::create` on Postgres/MySQL) | `check_constraint` / `add_check_constraint` |
| Enums | `enum()` on MySQL; SQL elsewhere (Laravel's is a checked `varchar`) | `create_enum` + `t.enum` on Postgres, `ENUM(...)` on MySQL |
| Custom types | SQL | `t.column :x, "TYPE"` |
| Primary key / autoincrement changes | SQL | SQL |
| `on delete set default` | supported | SQL (Rails has no such option) |
| Changing an enum's values on Postgres | SQL | SQL |
| SQLite tables with checks, enums or comments | SQL | SQL |
| Foreign key changes on SQLite | SQL (Laravel can't) | `add_foreign_key` (Rails rebuilds the table) |

When `sdb`'s SQL rebuilds a SQLite table, the migration turns off the framework's transaction
(`public $withinTransaction = false;` / `disable_ddl_transaction!`) and runs with foreign keys
off in its own transaction, failing if `PRAGMA foreign_key_check` finds a violation, as
`sdb push` does.

Tested against Laravel 13 and Rails 8.1 on SQLite, PostgreSQL 17 and MySQL 8.4, up then down:
on PostgreSQL and MySQL `sdb push --dry-run` then reports the database up to date. On SQLite
each framework writes some types its own way, which `sdb` sees as differences: Laravel's
booleans (`tinyint(1)`), `varchar` without a length and `uuid` (`varchar`), and both frameworks'
unique columns (a separate unique index rather than an inline `UNIQUE`).

## Development

```sh
cargo test
cargo clippy --all-targets
```

Source layout: `src/lib.rs` is the shared core and `src/main.rs` the CLI. `src/gui/` is the
desktop app: `main.rs` (app state, window), `tabs/` (one file per tab), `panels.rs` (menu bar, toolbar, sidebar, status bar), `theme.rs` (colors, fonts, chrome widgets), `dialogs.rs`, `file_menu.rs`, `er.rs` (diagram canvas), `er_layout.rs` (table placement), `grid.rs` (result grid),
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
