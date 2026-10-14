//! Core of sdb: the schema model, DSL, SQL generation, diffing, live database
//! access (SQLite, PostgreSQL, MySQL) and seeding. Shared by the `sdb` CLI and
//! the `sdb-gui` desktop app.

pub mod connections;
pub mod data;
pub mod db;
pub mod ddl;
pub mod diagram;
pub mod diff;
pub mod discover;
pub mod dump;
pub mod format;
pub mod framework;
pub mod history;
pub mod import;
pub mod integrity;
pub mod mysqldb;
pub mod output;
pub mod parser;
pub mod pg;
pub mod safety;
pub mod schema;
pub mod seed;
pub mod shell;
pub mod sqlite;
pub mod tls;
pub mod validate;

/// The example schema written by `sdb init` and the GUI's "New schema".
pub const STARTER_SCHEMA: &str = include_str!("../examples/blog.dbs");
