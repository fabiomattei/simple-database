//! Data integrity checks on a live database: rows whose foreign key points at
//! a parent row that doesn't exist.
//!
//! Works from the schema's foreign keys, not the database's enforcement, so it
//! also finds orphans that SQLite (with `foreign_keys` off), MyISAM tables, a
//! restore with checks disabled or keys declared only in a `.dbs` let in.

use crate::db::{Cell, Database, QueryResult};
use crate::ddl::{Dialect, quote, quote_table};
use crate::schema::{Schema, TableForeignKey};
use anyhow::{Result, bail};

/// One foreign key with rows that reference nothing.
#[derive(Debug, Clone)]
pub struct Orphans {
    pub table: String,
    pub foreign_key: TableForeignKey,
    /// Rows whose key has no parent (with `error`, as far as the check got).
    pub count: i64,
    /// The first few such rows.
    pub sample: Option<QueryResult>,
    /// The check's condition on `table`, for a WHERE clause (e.g. the Data tab's).
    pub condition: String,
    /// The check couldn't run, e.g. the referenced table or column is missing.
    pub error: Option<String>,
}

impl Orphans {
    /// `comments (post_id) -> posts (id)`.
    pub fn describe(&self) -> String {
        let fk = &self.foreign_key;
        format!("{} ({}) -> {} ({})", self.table, fk.columns.join(", "), fk.table, fk.ref_columns.join(", "))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Foreign keys checked.
    pub checked: usize,
    /// Those with orphaned rows or a failed check.
    pub problems: Vec<Orphans>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Alias of the referenced table inside the check, so a table that references
/// itself still works.
const PARENT: &str = "sdb_parent";

/// The condition on rows of `table` that are orphaned by `fk`: every key
/// column is set (a NULL in any of them means no reference, as in SQL's
/// default MATCH SIMPLE) and no parent row has those values.
pub fn orphan_condition(table: &str, fk: &TableForeignKey, schema: &Schema, d: Dialect) -> String {
    let child = quote_table(table, d);
    let set = fk.columns.iter().map(|c| format!("{child}.{} IS NOT NULL", quote(c, d)));
    let mut conds: Vec<String> = set.collect();
    // A missing parent table: every set key is an orphan.
    if schema.table(&fk.table).is_some() {
        let matches: Vec<String> = fk
            .columns
            .iter()
            .zip(&fk.ref_columns)
            .map(|(c, r)| format!("{PARENT}.{} = {child}.{}", quote(r, d), quote(c, d)))
            .collect();
        conds.push(format!("NOT EXISTS (SELECT 1 FROM {} {PARENT} WHERE {})", quote_table(&fk.table, d), matches.join(" AND ")));
    }
    conds.join(" AND ")
}

/// Check every foreign key of `schema` against the rows in `db`, keeping up to
/// `sample` orphaned rows of each. Stops between keys when cancelled.
pub fn check_foreign_keys(db: &mut Database, schema: &Schema, sample: usize) -> Result<Report> {
    let d = db.dialect();
    let mut report = Report::default();
    for t in &schema.tables {
        for fk in t.all_foreign_keys() {
            if db.cancel_requested() {
                bail!("cancelled");
            }
            report.checked += 1;
            let condition = orphan_condition(&t.name, &fk, schema, d);
            let mut found = Orphans { table: t.name.clone(), foreign_key: fk, count: 0, sample: None, condition, error: None };
            if let Some(e) = missing(schema, &found) {
                found.error = Some(e);
            }
            let from = format!("FROM {} WHERE {}", quote_table(&t.name, d), found.condition);
            match db.query_value(&format!("SELECT count(*) {from}")) {
                Ok(n) => found.count = count(&n),
                Err(e) => found.error = Some(format!("{e:#}")),
            }
            if found.count > 0 && sample > 0 {
                found.sample = db.run_limited(&format!("SELECT * {from} LIMIT {sample}"), sample)?.pop();
            }
            if found.count > 0 || found.error.is_some() {
                report.problems.push(found);
            }
        }
    }
    Ok(report)
}

/// What the foreign key references that doesn't exist.
fn missing(schema: &Schema, o: &Orphans) -> Option<String> {
    let fk = &o.foreign_key;
    let Some(parent) = schema.table(&fk.table) else {
        return Some(format!("references table '{}', which doesn't exist", fk.table));
    };
    let gone: Vec<&str> = fk.ref_columns.iter().filter(|c| parent.column(c).is_none()).map(String::as_str).collect();
    (!gone.is_empty()).then(|| format!("references {}.{}, which doesn't exist", fk.table, gone.join(", ")))
}

fn count(c: &Cell) -> i64 {
    match c {
        Cell::Int(n) => *n,
        other => other.display().parse().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn sqlite(sql: &str) -> Database {
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "test");
        db.execute(sql).unwrap();
        db
    }

    const SCHEMA: &str = "
        table users {
          id int pk
        }
        table posts {
          id int pk
          author_id int not null -> users.id
          parent_id int -> posts.id
        }
        table tags {
          post_id int
          lang text
          pk (post_id, lang)
        }
        table notes {
          id int pk
          post_id int
          lang text
          fk (post_id, lang) -> tags (post_id, lang)
        }
    ";

    /// Foreign keys off, as SQLite connections are by default: anything goes in.
    const DDL: &str = "
        CREATE TABLE users (id INTEGER PRIMARY KEY);
        CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER NOT NULL, parent_id INTEGER);
        CREATE TABLE tags (post_id INTEGER, lang TEXT, PRIMARY KEY (post_id, lang));
        CREATE TABLE notes (id INTEGER PRIMARY KEY, post_id INTEGER, lang TEXT);
        INSERT INTO users VALUES (1), (2);
        INSERT INTO posts VALUES (10, 1, NULL), (11, 2, 10), (12, 3, NULL), (13, 1, 99), (14, 7, 98);
        INSERT INTO tags VALUES (10, 'en');
        INSERT INTO notes VALUES (1, 10, 'en'), (2, 10, 'fr'), (3, NULL, 'de'), (4, 99, NULL);
    ";

    #[test]
    fn finds_orphaned_rows() {
        let schema = parse(SCHEMA).unwrap();
        let mut db = sqlite(DDL);
        let r = check_foreign_keys(&mut db, &schema, 10).unwrap();
        assert_eq!(r.checked, 3);
        let found: Vec<(String, i64)> = r.problems.iter().map(|o| (o.describe(), o.count)).collect();
        assert_eq!(
            found,
            [
                ("posts (author_id) -> users (id)".to_string(), 2),
                ("posts (parent_id) -> posts (id)".to_string(), 2),
                // (10, 'fr') has no tag; rows with a NULL in the key reference nothing.
                ("notes (post_id, lang) -> tags (post_id, lang)".to_string(), 1),
            ]
        );
        let ids = |o: &Orphans| o.sample.as_ref().unwrap().rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>();
        assert_eq!(ids(&r.problems[0]), [Cell::Int(12), Cell::Int(14)]);
        assert_eq!(ids(&r.problems[1]), [Cell::Int(13), Cell::Int(14)]);
        assert_eq!(ids(&r.problems[2]), [Cell::Int(2)]);
    }

    #[test]
    fn condition_works_as_a_data_tab_filter() {
        let schema = parse(SCHEMA).unwrap();
        let mut db = sqlite(DDL);
        let r = check_foreign_keys(&mut db, &schema, 0).unwrap();
        let o = &r.problems[1];
        assert!(o.sample.is_none());
        let view = crate::data::View { where_sql: o.condition.clone(), ..Default::default() };
        let t = schema.table("posts").unwrap();
        let rows = db.run(&view.all_sql(t, Dialect::Sqlite).unwrap()).unwrap().pop().unwrap().rows;
        assert_eq!(rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), [Cell::Int(13), Cell::Int(14)]);
    }

    #[test]
    fn clean_database_and_missing_parents() {
        let schema = parse(SCHEMA).unwrap();
        let mut db = sqlite(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER NOT NULL, parent_id INTEGER);
             CREATE TABLE tags (post_id INTEGER, lang TEXT, PRIMARY KEY (post_id, lang));
             CREATE TABLE notes (id INTEGER PRIMARY KEY, post_id INTEGER, lang TEXT);
             INSERT INTO users VALUES (1);
             INSERT INTO posts VALUES (10, 1, NULL), (11, 1, 10);",
        );
        assert!(check_foreign_keys(&mut db, &schema, 5).unwrap().is_clean());

        // The schema says posts reference users, but the database has no such table.
        db.execute("DROP TABLE users").unwrap();
        let r = check_foreign_keys(&mut db, &schema, 5).unwrap();
        assert_eq!(r.problems.len(), 1);
        assert!(r.problems[0].error.as_deref().unwrap().contains("no such table"), "{:?}", r.problems[0].error);
    }

    #[test]
    fn reports_a_reference_to_a_table_missing_from_the_schema() {
        let mut schema = parse(SCHEMA).unwrap();
        schema.tables.retain(|t| t.name != "users");
        let mut db = sqlite(DDL);
        let r = check_foreign_keys(&mut db, &schema, 5).unwrap();
        let o = &r.problems[0];
        assert_eq!(o.error.as_deref(), Some("references table 'users', which doesn't exist"));
        assert_eq!(o.count, 5, "every set key is an orphan");
    }

    /// Runs with SDB_TEST_POSTGRES / SDB_TEST_MYSQL. Temporary tables, so the
    /// round-trip tests sharing the scratch database don't see them. (MySQL
    /// can't use a temporary table twice in one query: no self-reference here.)
    #[test]
    fn finds_orphans_on_servers() {
        let schema = parse(
            "table ic_users { id int pk }
             table ic_tags { post_id int\n lang text\n pk (post_id, lang) }
             table ic_posts { id int pk\n author_id int -> ic_users.id\n tag_post int\n tag_lang text\n fk (tag_post, tag_lang) -> ic_tags (post_id, lang) }",
        )
        .unwrap();
        for var in ["SDB_TEST_POSTGRES", "SDB_TEST_MYSQL"] {
            let Ok(url) = std::env::var(var) else { continue };
            let mut db = Database::open(&url).unwrap();
            for sql in [
                "CREATE TEMPORARY TABLE ic_users (id INT PRIMARY KEY)",
                "CREATE TEMPORARY TABLE ic_tags (post_id INT, lang VARCHAR(10), PRIMARY KEY (post_id, lang))",
                "CREATE TEMPORARY TABLE ic_posts (id INT PRIMARY KEY, author_id INT, tag_post INT, tag_lang VARCHAR(10))",
                "INSERT INTO ic_users VALUES (1)",
                "INSERT INTO ic_tags VALUES (1, 'en')",
                "INSERT INTO ic_posts VALUES (1, 1, 1, 'en'), (2, 5, 1, 'fr'), (3, NULL, NULL, 'fr'), (4, 6, 2, 'en')",
            ] {
                db.execute(sql).unwrap();
            }
            let r = check_foreign_keys(&mut db, &schema, 10).unwrap();
            let found: Vec<(String, i64)> = r.problems.iter().map(|o| (o.describe(), o.count)).collect();
            assert_eq!(
                found,
                [("ic_posts (author_id) -> ic_users (id)".to_string(), 2), ("ic_posts (tag_post, tag_lang) -> ic_tags (post_id, lang)".to_string(), 2)],
                "{var}"
            );
            assert_eq!(r.problems[0].sample.as_ref().unwrap().rows.len(), 2, "{var}");
        }
    }
}
